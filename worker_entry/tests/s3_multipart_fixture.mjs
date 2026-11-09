// S3 wire behavior shared by the real workerd integration scenarios.
// Buffers here belong to the external fixture process, never the Worker isolate.
import { createHash } from 'node:crypto';

const xml = (value) => String(value).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');
const unxml = (value) => value.replaceAll('&quot;', '"').replaceAll('&apos;', "'").replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');
const md5 = (value) => createHash('md5').update(value).digest('hex');
const etag = (value) => `"${md5(value)}"`;

export function createS3MultipartFixture({ objects, failPut = () => false, maxPartBytes = 8 * 1024 * 1024 }) {
  let nextId = 1;
  const state = {
    uploads: new Map(),
    etags: new Map(),
    completed: [],
    aborted: [],
    partRequests: [],
    rangeRequests: [],
    activeParts: 0,
    maxActiveParts: 0,
    failComplete: false,
    ignoreRange: false,
    beforePart: null,
    beforeReadBody: null,
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
            const chunks = [];
            let size = 0;
            for await (const chunk of req) {
              size += chunk.length;
              if (size > maxPartBytes) return error(413, 'EntityTooLarge');
              chunks.push(chunk);
            }
            const bytes = Buffer.concat(chunks);
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
        res.setHeader('etag', tag);
        if (!req.headers.range || req.method === 'HEAD') {
          res.writeHead(200, { 'content-length': bytes.length });
          if (req.method === 'GET' && state.beforeReadBody && await state.beforeReadBody(req, res, key, bytes)) return true;
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
          res.end(bytes.subarray(start, end + 1));
        }
        return true;
      }
      return false;
    },
  };
  return state;
}
