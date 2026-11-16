// Real HTTP gateway fixture for browser transfer tests. No body is accumulated.
// Only request/response byte counts and paths (never headers or query strings)
// are retained. The target is restricted to an explicitly supplied loopback URL.
import { createServer, request } from 'node:http';
import { Transform } from 'node:stream';
import { once } from 'node:events';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const HOP_HEADERS = new Set([
  'connection', 'keep-alive', 'proxy-authenticate', 'proxy-authorization',
  'te', 'trailer', 'transfer-encoding', 'upgrade',
]);

function forwardedHeaders(headers) {
  const blocked = new Set(HOP_HEADERS);
  for (const name of String(headers.connection || '').split(',')) blocked.add(name.trim().toLowerCase());
  return Object.fromEntries(Object.entries(headers).filter(([name]) => !blocked.has(name.toLowerCase())));
}

export async function startTransferLimitProxy({ target, maxBodyBytes = 10 * 1024 * 1024, port = 0 }) {
  const upstream = new URL(target);
  if (upstream.protocol !== 'http:' || !['127.0.0.1', 'localhost', '[::1]'].includes(upstream.hostname)
      || upstream.username || upstream.password || upstream.pathname !== '/' || upstream.search || upstream.hash) {
    throw new TypeError('The transfer fixture target must be a loopback HTTP origin');
  }
  if (!Number.isSafeInteger(maxBodyBytes) || maxBodyBytes < 1) throw new RangeError('Invalid body limit');
  const records = [];
  let omittedRecords = 0;
  const sockets = new Set();
  const stats = { requests: 0, blocked: 0, acceptedBytes: 0, maxAcceptedBody: 0 };
  const server = createServer((incoming, outgoing) => {
    const record = {
      method: incoming.method, path: incoming.url.split('?', 1)[0],
      requestBytes: 0, responseBytes: 0, status: 0, complete: false, blocked: false,
    };
    stats.requests++;
    if (records.length < 10000) records.push(record); else omittedRecords++;
    outgoing.on('finish', () => { record.complete = true; });
    let remote;
    let rejected = false;
    incoming.on('aborted', () => remote?.destroy());
    incoming.on('error', () => remote?.destroy());
    const fail = (status, message) => {
      if (rejected || outgoing.destroyed) return;
      rejected = true;
      record.status = status;
      if (status === 413) { record.blocked = true; stats.blocked++; }
      incoming.unpipe();
      remote?.destroy();
      incoming.resume();
      if (outgoing.headersSent) { outgoing.destroy(); return; }
      outgoing.writeHead(status, {
        'content-type': 'text/plain; charset=utf-8',
        'content-length': Buffer.byteLength(message),
        // Drain the rejected body while the client reads the fixed-length
        // response; closing here can reset a chunked upload before its 413.
        connection: 'keep-alive',
      });
      outgoing.end(message);
    };
    const declared = incoming.headers['content-length'];
    if (declared && (!/^\d+$/.test(declared) || BigInt(declared) > BigInt(maxBodyBytes))) {
      fail(413, 'Transfer fixture request body limit exceeded');
      return;
    }
    remote = request({
      hostname: upstream.hostname.replace(/^\[|\]$/g, ''), port: upstream.port || 80,
      method: incoming.method, path: incoming.url, headers: forwardedHeaders(incoming.headers),
    }, (response) => {
      if (rejected) { response.destroy(); return; }
      record.status = response.statusCode;
      outgoing.writeHead(response.statusCode, forwardedHeaders(response.headers));
      const count = new Transform({
        transform(chunk, encoding, callback) { record.responseBytes += chunk.length; callback(null, chunk); },
      });
      response.on('error', () => outgoing.destroy());
      count.on('error', () => outgoing.destroy());
      response.pipe(count).pipe(outgoing);
    });
    const count = new Transform({
      transform(chunk, encoding, callback) {
        record.requestBytes += chunk.length;
        if (record.requestBytes > maxBodyBytes) {
          fail(413, 'Transfer fixture request body limit exceeded');
          callback(new Error('request limit'));
        } else callback(null, chunk);
      },
    });
    count.on('error', () => { incoming.unpipe(count); remote.destroy(); });
    remote.on('error', () => fail(502, 'Transfer fixture upstream connection failed'));
    incoming.on('end', () => {
      if (!record.blocked) {
        stats.acceptedBytes += record.requestBytes;
        stats.maxAcceptedBody = Math.max(stats.maxAcceptedBody, record.requestBytes);
      }
    });
    outgoing.on('close', () => { if (!outgoing.writableFinished) remote.destroy(); });
    incoming.pipe(count).pipe(remote);
  });
  server.on('connection', (socket) => { sockets.add(socket); socket.on('close', () => sockets.delete(socket)); });
  // Forward the UI's existing WebSocket connection; transfer bytes still use HTTP.
  server.on('upgrade', (incoming, socket, head) => {
    const remote = request({
      hostname: upstream.hostname.replace(/^\[|\]$/g, ''), port: upstream.port || 80,
      method: incoming.method, path: incoming.url, headers: incoming.headers,
    });
    remote.on('upgrade', (response, upstreamSocket, upstreamHead) => {
      sockets.add(upstreamSocket);
      upstreamSocket.on('close', () => sockets.delete(upstreamSocket));
      const headers = [];
      for (let index = 0; index < response.rawHeaders.length; index += 2) {
        headers.push(`${response.rawHeaders[index]}: ${response.rawHeaders[index + 1]}`);
      }
      socket.write(`HTTP/1.1 101 Switching Protocols\r\n${headers.join('\r\n')}\r\n\r\n`);
      if (head.length) upstreamSocket.write(head);
      if (upstreamHead.length) socket.write(upstreamHead);
      socket.on('error', () => upstreamSocket.destroy());
      upstreamSocket.on('error', () => socket.destroy());
      socket.pipe(upstreamSocket).pipe(socket);
    });
    remote.on('response', (response) => { response.destroy(); socket.destroy(); });
    remote.on('error', () => socket.destroy());
    socket.on('close', () => remote.destroy());
    remote.end();
  });
  server.listen(port, '127.0.0.1');
  await once(server, 'listening');
  return {
    base: `http://127.0.0.1:${server.address().port}`,
    snapshot() { return { ...stats, omittedRecords, records: records.map((record) => ({ ...record })) }; },
    async close() {
      const closed = once(server, 'close');
      server.close();
      for (const socket of sockets) socket.destroy();
      await closed;
    },
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const fixture = await startTransferLimitProxy({
    target: process.argv[2], port: Number(process.env.PORT || 0),
    maxBodyBytes: Number(process.env.MAX_BODY_BYTES || 10 * 1024 * 1024),
  });
  console.log(`transfer limit proxy listening on ${fixture.base}`);
  let closing = false;
  const close = async () => {
    if (closing) return;
    closing = true;
    await fixture.close();
    console.log(JSON.stringify(fixture.snapshot()));
  };
  process.on('SIGINT', close);
  process.on('SIGTERM', close);
}
