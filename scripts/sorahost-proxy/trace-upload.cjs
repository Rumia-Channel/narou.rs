'use strict';

// NODE_OPTIONS の preload 専用。認証情報・URL・ヘッダ値・本文は記録しない。
const { channel } = require('node:diagnostics_channel');
const { isIP } = require('node:net');

let endpoint;
try {
  endpoint = new URL(`${(process.env.SORAHOST_ENDPOINT || '').trim().replace(/\/+$/, '')}/deploy`);
} catch {
  // CLI 自身が設定エラーを処理する。診断で例外や秘密値を追加しない。
}

process.stderr.write(`[relay-upload] ${JSON.stringify({ event: 'preload', node: process.version, configured: Boolean(endpoint) })}\n`);

if (endpoint) {
  // start は本文の終端に到達しないと発火しない。早期 408 も created から追う。
  channel('http.client.request.created').subscribe(({ request }) => {
    if (request.method !== 'POST' || request.getHeader('host') !== endpoint.host || request.path !== endpoint.pathname + endpoint.search) return;

    const started = performance.now();
    const report = (event, fields = {}) => {
      process.stderr.write(`[relay-upload] ${JSON.stringify({ event, elapsedMs: Math.round(performance.now() - started), ...fields })}\n`);
    };
    const counts = () => ({
      bytesWritten: request.socket?.bytesWritten ?? 0,
      bytesRead: request.socket?.bytesRead ?? 0,
      writableLength: request.writableLength,
      writableFinished: request.writableFinished,
    });
    report('request', {
      node: process.version,
      protocol: endpoint.protocol,
      port: Number(endpoint.port || (endpoint.protocol === 'https:' ? 443 : 80)),
      addressLiteral: Boolean(isIP(endpoint.hostname)),
      contentLength: Number(request.getHeader('content-length')),
    });
    const attachSocket = (socket) => {
      report('socket', { connecting: socket.connecting, family: socket.remoteFamily });
      socket.once('lookup', (error, address, family) => report('lookup', { family, failed: Boolean(error) }));
      socket.once('connect', () => report('connected', { family: socket.remoteFamily }));
      socket.once('close', (hadError) => report('socket-closed', { hadError, ...counts() }));
    };
    if (request.socket) attachSocket(request.socket);
    else request.once('socket', attachSocket);
    request.once('finish', () => report('request-finished', counts()));
    request.once('response', (response) => {
      report('response', { status: response.statusCode, ...counts() });
      let bytes = 0;
      response.on('data', (chunk) => { bytes += chunk.length; });
      response.once('end', () => report('response-ended', { bodyBytes: bytes, ...counts() }));
    });
    request.once('error', (error) => report('error', {
      code: /^[A-Z0-9_]+$/.test(error.code ?? '') ? error.code : 'unknown',
      ...counts(),
    }));
    const timer = setInterval(() => report('progress', counts()), 5000);
    timer.unref();
    request.once('close', () => { clearInterval(timer); });
  });
}
