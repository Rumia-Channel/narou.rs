import { spawn } from "node:child_process";
import http from "node:http";
import https from "node:https";
import { isIP } from "node:net";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { createBrotliDecompress, createUnzip } from "node:zlib";
import { MAX_BODY_BYTES, MAX_HEAD_BYTES, assertResponseMetadata, readBoundedBytes } from "./worker/protocol.mjs";

const messages = {
  "not-installed": "curl is not installed",
  "not-executable": "curl is not executable",
  "tls-verification": "upstream TLS verification failed",
  "timed-out": "upstream request timed out",
  "cancelled": "request cancelled",
  "too-large": "response too large",
  "incomplete": "incomplete upstream response",
  "failed": "upstream request failed",
};

export class RelayTransportError extends Error {
  constructor(reason) {
    super(messages[reason] || messages.failed);
    this.reason = Object.hasOwn(messages, reason) ? reason : "failed";
  }
}

const curlFailure = (code) => {
  if (code === "ENOENT") return new RelayTransportError("not-installed");
  if (code === "EACCES") return new RelayTransportError("not-executable");
  if ([51, 58, 60, 77].includes(code)) return new RelayTransportError("tls-verification");
  if (code === 28) return new RelayTransportError("timed-out");
  if (code === 63) return new RelayTransportError("too-large");
  if (code === 18) return new RelayTransportError("incomplete");
  return new RelayTransportError("failed");
};

const headerValue = (headers, name) =>
  headers.find(([header]) => header.toLowerCase() === name)?.[1] || "";

// -i の中間 1xx だけを読み飛ばす。本文が HTTP/ で始まっても捨てない。
const splitHead = (buffer) => {
  let offset = 0;
  for (;;) {
    const end = buffer.indexOf("\r\n\r\n", offset);
    if (end < 0 || end + 4 > MAX_HEAD_BYTES) throw new RelayTransportError("incomplete");
    const lines = buffer.subarray(offset, end).toString("utf8").split("\r\n");
    const status = Number((lines[0].match(/^HTTP\/[\d.]+ (\d{3})/) || [])[1]);
    offset = end + 4;
    if (status >= 100 && status < 200) continue;
    if (!(status >= 200 && status <= 599)) throw new RelayTransportError("incomplete");
    const headers = [];
    for (const line of lines.slice(1)) {
      const separator = line.indexOf(":");
      if (separator > 0) headers.push([line.slice(0, separator).trim(), line.slice(separator + 1).trim()]);
    }
    const body = buffer.subarray(offset);
    if (body.length > MAX_BODY_BYTES) throw new RelayTransportError("too-large");
    return {
      status,
      contentType: headerValue(headers, "content-type"),
      location: headerValue(headers, "location") || undefined,
      setCookie: headers.filter(([name]) => name.toLowerCase() === "set-cookie").map(([, value]) => value),
      body,
      via: "curl",
    };
  }
};

export const viaCurl = (target, headers, { curlBin, timeoutMs, signal }) =>
  new Promise((resolve, reject) => {
    if (signal.aborted) return reject(new RelayTransportError("cancelled"));
    const args = [
      "-q", "-sS", "-i", "--compressed", "--noproxy", "*", "--proto", "=http,https",
      "--connect-timeout", String(Math.min(10, timeoutMs / 1000)),
      "--max-time", String(timeoutMs / 1000), "--max-filesize", String(MAX_BODY_BYTES),
    ];
    const host = target.url.hostname.replace(/^\[|\]$/g, "");
    if (!isIP(host)) {
      const address = target.family === 6 ? `[${target.address}]` : target.address;
      args.push("--resolve", `${host}:${target.url.port || (target.url.protocol === "https:" ? 443 : 80)}:${address}`);
    }
    // ヘッダを argv に載せず標準入力で渡す。curlrc と環境プロキシは利用しない。
    args.push("--header", "@-", "--", target.url.href);
    const child = spawn(curlBin, args, { stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
    const chunks = [];
    let length = 0;
    let failure;
    const stop = (reason) => {
      failure ||= new RelayTransportError(reason);
      child.kill("SIGKILL");
    };
    const cancel = () => stop("cancelled");
    signal.addEventListener("abort", cancel, { once: true });
    const timer = setTimeout(() => stop("timed-out"), timeoutMs);
    timer.unref();
    child.stdout.on("data", (chunk) => {
      length += chunk.length;
      if (length > MAX_HEAD_BYTES + MAX_BODY_BYTES) stop("too-large");
      else if (!failure) chunks.push(chunk);
    });
    // stderr は捨てる。内容や exec/spawn の error.message は資格情報を含み得る。
    child.stderr.on("data", () => {});
    child.stdin.on("error", () => {});
    child.on("error", (error) => { failure ||= curlFailure(error.code); });
    child.once("close", (code) => {
      clearTimeout(timer);
      signal.removeEventListener("abort", cancel);
      if (signal.aborted) return reject(new RelayTransportError("cancelled"));
      if (failure) return reject(failure);
      if (code !== 0) return reject(curlFailure(code));
      try { resolve(splitHead(Buffer.concat(chunks, length))); } catch (error) { reject(error); }
    });
    child.stdin.end(headers.map(([name, value]) => `${name}: ${value}\r\n`).join(""));
  });

// global fetch では DNS を固定できないため、代替経路も標準 HTTP(S) の lookup を固定する。
// TLS は URL の元のホスト名で検証し、プロキシ・リダイレクト・共有接続プールを使わない。
export const viaPinnedHttp = async (target, headers, { timeoutMs, signal }) => {
  const local = new AbortController();
  const timeout = setTimeout(() => local.abort(), timeoutMs);
  timeout.unref();
  const combined = AbortSignal.any([signal, local.signal]);
  const secure = target.url.protocol === "https:";
  const client = secure ? https : http;
  const agent = new client.Agent({ keepAlive: false, proxyEnv: {}, rejectUnauthorized: true });
  let request;
  let response;
  let decoder;
  let transfer;
  try {
    const pairs = headers.map(([name, value]) => [name, Buffer.from(value, "utf8").toString("latin1")]);
    if (!pairs.some(([name]) => name.toLowerCase() === "host")) pairs.push(["Host", target.url.host]);
    if (!pairs.some(([name]) => name.toLowerCase() === "accept-encoding")) pairs.push(["Accept-Encoding", "gzip, deflate, br"]);
    response = await new Promise((resolve, reject) => {
      request = client.request(target.url, {
        method: "GET", headers: pairs.flat(), agent, signal: combined,
        rejectUnauthorized: true, maxHeaderSize: MAX_HEAD_BYTES,
        lookup(_hostname, options, callback) {
          if (options.all) callback(null, [{ address: target.address, family: target.family }]);
          else callback(null, target.address, target.family);
        },
      }, resolve);
      request.on("error", reject);
      request.end();
    });
    const result = {
      status: response.statusCode,
      contentType: response.headers["content-type"] || "",
      location: response.headers.location || undefined,
      setCookie: response.headers["set-cookie"] || [],
      via: "node-http",
    };
    assertResponseMetadata(result);
    const encoding = (response.headers["content-encoding"] || "identity").toLowerCase();
    if (encoding === "gzip" || encoding === "deflate") decoder = createUnzip();
    else if (encoding === "br") decoder = createBrotliDecompress();
    else if (encoding !== "identity") throw new RelayTransportError("failed");
    if (decoder) transfer = pipeline(response, decoder).then(() => null, (error) => error);
    const bytes = await readBoundedBytes(Readable.toWeb(decoder || response, {
      strategy: { highWaterMark: 64 * 1024, size: (chunk) => chunk.byteLength },
    }), {
      limit: MAX_BODY_BYTES,
      contentLength: decoder || result.status === 204 || result.status === 304 ? null : response.headers["content-length"] ?? null,
      label: "response",
    });
    if (transfer && await transfer) throw new RelayTransportError("incomplete");
    return { ...result, body: Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength) };
  } catch (error) {
    if (signal.aborted) throw new RelayTransportError("cancelled");
    if (local.signal.aborted) throw new RelayTransportError("timed-out");
    if (error instanceof RelayTransportError) throw error;
    if (["CERT_NOT_YET_VALID", "CERT_HAS_EXPIRED", "DEPTH_ZERO_SELF_SIGNED_CERT", "SELF_SIGNED_CERT_IN_CHAIN", "ERR_TLS_CERT_ALTNAME_INVALID", "UNABLE_TO_GET_ISSUER_CERT", "UNABLE_TO_GET_ISSUER_CERT_LOCALLY", "UNABLE_TO_VERIFY_LEAF_SIGNATURE", "CERT_REVOKED", "CERT_UNTRUSTED", "CERT_REJECTED"].includes(error.code)) {
      throw new RelayTransportError("tls-verification");
    }
    // reader の診断は固定だが、他の Node エラーは URL やヘッダを含み得る。
    if (error.message?.startsWith("response too large:")) throw new RelayTransportError("too-large");
    throw new RelayTransportError(response ? "incomplete" : "failed");
  } finally {
    clearTimeout(timeout);
    request?.destroy();
    response?.destroy();
    decoder?.destroy();
    agent.destroy();
    if (transfer) await transfer;
  }
};
