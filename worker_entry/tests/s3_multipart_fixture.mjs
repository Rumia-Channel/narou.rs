// S3 wire behavior shared by the real workerd integration scenarios.
// Buffers here belong to the external fixture process, never the Worker isolate.
import { createHash } from 'node:crypto';

const xml = (value) => String(value).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');
const unxml = (value) => value.replaceAll('&quot;', '"').replaceAll('&apos;', "'").replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');
const md5 = (value) => createHash('md5').update(value).digest('hex');
const etag = (value) => `"${md5(value)}"`;

// The byte span one read actually delivers. A read without a Range header
// takes the whole object; a ranged read takes the clamped window. The
// cancellation gates use it to tell a full delivery from a metadata probe.
const rangeSpan = (value, size) => {
  if (value === undefined) return size;
  const match = /^bytes=(\d+)-(\d*)$/.exec(value);
  if (!match) return 0;
  const start = Number(match[1]);
  const end = match[2] ? Math.min(Number(match[2]), size - 1) : size - 1;
  return end - start + 1;
};

export function createS3MultipartFixture({ objects, failPut = () => false, maxPartBytes = 8 * 1024 * 1024 }) {
  let nextId = 1;
  const state = {
    uploads: new Map(),
    etags: new Map(),
    completed: [],
    aborted: [],
    partRequests: [],
    rangeRequests: [],
    // Every object read this fixture served, in arrival order. A gate that
    // holds one read must be able to name it exactly and to prove that no
    // other delivery of the same bytes was still open, so the wire shape of
    // each read (method, key, Range, precondition, span, lifetime) is kept.
    reads: [],
    activeParts: 0,
    maxActiveParts: 0,
    failComplete: false,
    ignoreRange: false,
    beforePart: null,
    beforeReadBody: null,
    beforeReadHeaders: null,
    checkConditions(req, res, key) {
      const exists = objects.has(key);
      const tag = exists ? state.etags.get(key) ?? etag(objects.get(key)) : undefined;
      const match = req.headers['if-match'];
      const none = req.headers['if-none-match'];
      const sourceMatch = req.headers['x-amz-copy-source-if-match'];
      let failed = (match && (match === '*' ? !exists : match !== tag))
        || (none && (none === '*' ? exists : none === tag));
      if (sourceMatch) {
        const source = decodeURIComponent(req.headers['x-amz-copy-source']);
        const sourceKey = source.startsWith('/') ? source : `/${source}`;
        const sourceBytes = objects.get(sourceKey);
        failed ||= !sourceBytes || sourceMatch !== (state.etags.get(sourceKey) ?? etag(sourceBytes));
      }
      if (!failed) return true;
      req.resume();
      res.writeHead(412, { 'content-type': 'application/xml' });
      res.end('<Error><Code>PreconditionFailed</Code></Error>');
      return false;
    },
    async handle(req, res, url, key) {
      const error = (status, code) => {
        req.resume();
        res.writeHead(status, { 'content-type': 'application/xml' });
        res.end(`<Error><Code>${code}</Code></Error>`);
        return true;
      };
      const result = (body) => {
        res.setHeader('content-type', 'application/xml');
        res.end(body);
        return true;
      };
      if (url.searchParams.has('uploads')) {
        if (req.method === 'POST') {
          if (failPut()) return error(503, 'SlowDown');
          const id = `fixture-upload-${nextId++}`;
          state.uploads.set(id, { key, parts: new Map(), initiated: new Date().toISOString() });
          req.resume();
          return result(`<InitiateMultipartUploadResult><Key>${xml(key.split('/').slice(2).join('/'))}</Key><UploadId>${id}</UploadId></InitiateMultipartUploadResult>`);
        }
        if (req.method === 'GET') {
          const bucket = key.replace(/\/$/, '');
          const prefix = url.searchParams.get('prefix') ?? '';
          const uploads = [...state.uploads].filter(([, upload]) => upload.key.startsWith(`${bucket}/${prefix}`));
          return result(`<ListMultipartUploadsResult><IsTruncated>false</IsTruncated>${uploads.map(([id, upload]) => `<Upload><Key>${xml(upload.key.slice(bucket.length + 1))}</Key><UploadId>${id}</UploadId><Initiated>${upload.initiated}</Initiated></Upload>`).join('')}</ListMultipartUploadsResult>`);
        }
      }
      const uploadId = url.searchParams.get('uploadId');
      if (uploadId !== null) {
        const upload = state.uploads.get(uploadId);
        if (!upload || upload.key !== key) return error(404, 'NoSuchUpload');
        if (req.method === 'DELETE') {
          state.uploads.delete(uploadId);
          state.aborted.push({ uploadId, key });
          req.resume();
          res.writeHead(204); res.end();
          return true;
        }
        if (req.method === 'PUT') {
          if (failPut()) return error(503, 'SlowDown');
          const number = Number(url.searchParams.get('partNumber'));
          if (!Number.isSafeInteger(number) || number < 1 || number > 10000) return error(400, 'InvalidArgument');
          state.activeParts++;
          state.maxActiveParts = Math.max(state.maxActiveParts, state.activeParts);
          try {
            if (state.beforePart) await state.beforePart({ uploadId, key, number });
            let bytes;
            if (req.headers['x-amz-copy-source']) {
              // UploadPartCopy: the body is empty; bytes come from the source.
              req.resume();
              const source = decodeURIComponent(req.headers['x-amz-copy-source']);
              const sourceKey = source.startsWith('/') ? source : `/${source}`;
              const sourceBytes = objects.get(sourceKey);
              if (!sourceBytes) return error(404, 'NoSuchKey');
              const sourceIfMatch = req.headers['x-amz-copy-source-if-match'];
              if (sourceIfMatch && sourceIfMatch !== (state.etags.get(sourceKey) ?? etag(sourceBytes))) {
                return error(412, 'PreconditionFailed');
              }
              const range = req.headers['x-amz-copy-source-range'];
              if (range !== undefined) {
                const match = /^bytes=(\d+)-(\d+)$/.exec(range);
                if (!match) return error(400, 'InvalidArgument');
                const start = Number(match[1]);
                const end = Number(match[2]);
                if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start > end || end >= sourceBytes.length) {
                  return error(400, 'InvalidArgument');
                }
                bytes = Buffer.from(sourceBytes.subarray(start, end + 1));
              } else {
                bytes = Buffer.from(sourceBytes);
              }
              if (bytes.length > maxPartBytes) return error(413, 'EntityTooLarge');
              upload.parts.set(number, bytes);
              state.partRequests.push({ uploadId, key, number, size: bytes.length });
              return result(`<CopyPartResult><LastModified>2026-10-15T00:00:00Z</LastModified><ETag>${xml(etag(bytes))}</ETag></CopyPartResult>`);
            }
            const chunks = [];
            let size = 0;
            for await (const chunk of req) {
              size += chunk.length;
              if (size > maxPartBytes) return error(413, 'EntityTooLarge');
              chunks.push(chunk);
            }
            bytes = Buffer.concat(chunks);
            const expected = req.headers['x-amz-content-sha256'];
            if (expected && expected !== createHash('sha256').update(bytes).digest('hex')) return error(403, 'SignatureDoesNotMatch');
            upload.parts.set(number, bytes);
            state.partRequests.push({ uploadId, key, number, size });
            res.setHeader('etag', etag(bytes)); res.end();
            return true;
          } finally {
            state.activeParts--;
          }
        }
        if (req.method === 'GET') {
          return result(`<ListPartsResult><IsTruncated>false</IsTruncated>${[...upload.parts].sort(([a], [b]) => a - b).map(([number, bytes]) => `<Part><PartNumber>${number}</PartNumber><ETag>${xml(etag(bytes))}</ETag><Size>${bytes.length}</Size></Part>`).join('')}</ListPartsResult>`);
        }
        if (req.method === 'POST') {
          const chunks = [];
          let size = 0;
          for await (const chunk of req) {
            size += chunk.length;
            if (size > 1024 * 1024) return error(413, 'EntityTooLarge');
            chunks.push(chunk);
          }
          if (state.failComplete) return error(200, 'InternalError');
          const body = Buffer.concat(chunks).toString();
          const parts = [...body.matchAll(/<Part>(.*?)<\/Part>/gs)];
          if (parts.length === 0) return error(400, 'InvalidRequest');
          const ordered = [];
          let previous = 0;
          for (const [, part] of parts) {
            const number = Number(/<PartNumber>(\d+)<\/PartNumber>/.exec(part)?.[1]);
            const tag = /<ETag>(.*?)<\/ETag>/s.exec(part)?.[1];
            if (!Number.isSafeInteger(number) || !tag) return error(400, 'InvalidPart');
            if (number <= previous) return error(400, 'InvalidPartOrder');
            const bytes = upload.parts.get(number);
            if (!bytes || unxml(tag) !== etag(bytes)) return error(400, 'InvalidPart');
            ordered.push(bytes);
            previous = number;
          }
          // Destination conditional write: If-None-Match:*/If-Match apply to
          // the completed key, exactly like a conditional PutObject.
          if (!state.checkConditions(req, res, key)) return true;
          if (ordered.slice(0, -1).some((part) => part.length < 5 * 1024 * 1024)) return error(400, 'EntityTooSmall');
          const bytes = Buffer.concat(ordered);
          const finalEtag = `"${md5(Buffer.concat(ordered.map((part) => Buffer.from(md5(part), 'hex'))))}-${ordered.length}"`;
          objects.set(key, bytes);
          state.etags.set(key, finalEtag);
          state.uploads.delete(uploadId);
          state.completed.push({ uploadId, key, size: bytes.length, parts: ordered.length });
          return result(`<CompleteMultipartUploadResult><Key>${xml(key.split('/').slice(2).join('/'))}</Key><ETag>${xml(finalEtag)}</ETag></CompleteMultipartUploadResult>`);
        }
        return error(405, 'MethodNotAllowed');
      }
      if ((req.method === 'GET' || req.method === 'HEAD') && objects.has(key)) {
        const bytes = objects.get(key);
        const tag = state.etags.get(key) ?? etag(bytes);
        if (req.headers['if-match'] && req.headers['if-match'] !== tag) return error(412, 'PreconditionFailed');
        const read = {
          method: req.method,
          key,
          range: req.headers.range ?? null,
          ifMatch: req.headers['if-match'] ?? null,
          span: rangeSpan(req.headers.range, bytes.length),
          at: Date.now(),
          closedAt: null,
          held: false,
          done: false,
        };
        state.reads.push(read);
        // The read is finished once its response is gone: either it completed
        // or the peer tore the connection down.
        res.once('close', () => { read.done = true; read.closedAt = Date.now(); });
        if (req.method === 'GET' && state.beforeReadHeaders) {
          read.held = await state.beforeReadHeaders(req, res, key, bytes, read);
          if (read.held) return true;
        }
        res.setHeader('etag', tag);
        if (!req.headers.range || req.method === 'HEAD') {
          res.writeHead(200, { 'content-length': bytes.length });
          if (req.method === 'GET' && state.beforeReadBody && await state.beforeReadBody(req, res, key, bytes, undefined, read)) return true;
          res.end(req.method === 'HEAD' ? undefined : bytes);
          return true;
        }
        const match = /^bytes=(\d+)-(\d*)$/.exec(req.headers.range);
        if (!match) return error(400, 'InvalidArgument');
        const start = Number(match[1]);
        const end = match[2] ? Math.min(Number(match[2]), bytes.length - 1) : bytes.length - 1;
        if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start > end || start >= bytes.length) {
          res.setHeader('content-range', `bytes */${bytes.length}`);
          return error(416, 'InvalidRange');
        }
        state.rangeRequests.push({ key, start, end });
        if (state.ignoreRange) {
          res.writeHead(200, { 'content-length': bytes.length });
          res.end(bytes);
        } else {
          res.writeHead(206, { 'content-range': `bytes ${start}-${end}/${bytes.length}`, 'content-length': end - start + 1 });
          if (state.beforeReadBody && await state.beforeReadBody(req, res, key, bytes, { start, end }, read)) return true;
          res.end(bytes.subarray(start, end + 1));
        }
        return true;
      }
      return false;
    },
  };
  return state;
}
