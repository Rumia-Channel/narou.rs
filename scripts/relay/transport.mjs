import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, copyFileSync, existsSync, readFileSync, rmSync } from "node:fs";
import http from "node:http";
import https from "node:https";
import { isIP } from "node:net";
import net from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";
import { createBrotliDecompress, createUnzip } from "node:zlib";
import { MAX_BODY_BYTES, MAX_HEAD_BYTES, assertResponseMetadata, readBoundedBytes } from "./worker/protocol.mjs";

const moduleDir = dirname(fileURLToPath(import.meta.url));

const runnable = (bin, verify) => {
  if (!verify) return true;
  const probe = spawnSync(bin, ["--version"], { stdio: "ignore", timeout: 5000, windowsHide: true });
  return !probe.error && probe.status === 0;
};

const usable = (bin, verify) => existsSync(bin) && runnable(bin, verify);

// 配備に同梱した静的 curl (deploy-sorahost.mjs が置く) を書き込み可能な場所へ
// コピーして実行権限を付ける。PteWorker のコンテナには curl が無く、配備先は
// 読み取り専用で実行ビットも落ちるため、同梱パスをそのまま実行できない。
// 既存のファイルは使わず毎回上書きする (差し替えられた実行ファイルを走らせない、
// シンボリックリンクを辿って別のファイルを壊さない)。
const prepareBundledCurl = (bundled, { verify }) => {
  let digest;
  try {
    digest = createHash("sha256").update(readFileSync(bundled)).digest("hex").slice(0, 16);
  } catch {
    return null;
  }
  const target = join(tmpdir(), `narou-relay-curl-${digest}`);
  try {
    rmSync(target, { force: true });
    copyFileSync(bundled, target);
    chmodSync(target, 0o755);
  } catch {
    return null;
  }
  return usable(target, verify) ? target : null;
};

/// curl の実行ファイルと CA バンドルを決める。優先順は環境変数 → 同梱 →
/// PATH の curl。CA バンドルは `NAROU_RELAY_CACERT` が最優先で、無ければ
/// 同梱の cacert.pem を渡す (静的ビルドの既定 CA パスはコンテナに無いことがある)。
/// 指定した CA バンドルは全経路 (curl / wget / Node) で共有する。
export const resolveCurl = ({ env = process.env, dir = moduleDir, verify = true } = {}) => {
  const configured = String(env.NAROU_RELAY_CURL ?? "").trim();
  const envCacert = String(env.NAROU_RELAY_CACERT ?? "").trim() || null;
  if (configured) {
    return { bin: configured, cacert: envCacert, source: "env" };
  }
  const names = process.platform === "win32" ? ["curl.exe", "curl"] : ["curl"];
  const bundled = names.map((name) => join(dir, name)).find((path) => existsSync(path));
  if (!bundled) return { bin: "curl", cacert: envCacert, source: "path" };
  const bundledCacert = existsSync(join(dir, "cacert.pem")) ? join(dir, "cacert.pem") : null;
  const prepared = prepareBundledCurl(bundled, { verify }) || (runnable(bundled, verify) ? bundled : null);
  return prepared
    ? { bin: prepared, cacert: envCacert || bundledCacert, source: "bundled" }
    : { bin: "curl", cacert: envCacert, source: "path" };
};

/// wget の実行ファイルと CA バンドルを決める。優先順は環境変数 → 同梱 →
/// PATH の wget。wget は curl の `--resolve` に相当する DNS 固定ができない
/// ため、実際の接続は startPinnedProxy() が検証済み IP に対して行う。
/// `available` は起動時の疎通確認結果 (無い場合は取得順から外す)。
export const resolveWget = ({ env = process.env, dir = moduleDir, verify = true } = {}) => {
  const cacert = String(env.NAROU_RELAY_CACERT ?? "").trim() || null;
  const configured = String(env.NAROU_RELAY_WGET ?? "").trim();
  if (configured) {
    return { bin: configured, cacert, source: "env", available: runnable(configured, verify) };
  }
  const names = process.platform === "win32" ? ["wget.exe", "wget"] : ["wget"];
  const bundled = names.map((name) => join(dir, name)).find((path) => existsSync(path));
  if (bundled) {
    const bundledCacert = existsSync(join(dir, "cacert.pem")) ? join(dir, "cacert.pem") : cacert;
    return { bin: bundled, cacert: bundledCacert, source: "bundled", available: runnable(bundled, verify) };
  }
  return { bin: "wget", cacert, source: "path", available: runnable("wget", verify) };
};

const messages = {
  "not-installed": "curl is not installed",
  "not-executable": "curl is not executable",
  "tls-verification": "upstream TLS verification failed",
  "ca-bundle": "relay CA bundle could not be read",
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

export const viaCurl = (target, headers, { curlBin, cacert = null, timeoutMs, signal }) =>
  new Promise((resolve, reject) => {
    if (signal.aborted) return reject(new RelayTransportError("cancelled"));
    const args = [
      "-q", "-sS", "-i", "--compressed", "--noproxy", "*", "--proto", "=http,https",
      "--connect-timeout", String(Math.min(10, timeoutMs / 1000)),
      "--max-time", String(timeoutMs / 1000), "--max-filesize", String(MAX_BODY_BYTES),
    ];
    if (cacert) args.push("--cacert", cacert);
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

// wget には curl の `--resolve` に相当する接続先固定が無いため、127.0.0.1 に
// 使い捨ての CONNECT プロキシを立て、検証済み IP へだけトンネルする。
// TLS の SNI と証明書検証は wget が元のホスト名に対して行う。
const WGET_UNFORWARDED = new Set([
  "host", "user-agent", "accept-encoding", "connection", "proxy-connection", "content-length",
]);
const WGET_OUTPUT_LIMIT = MAX_HEAD_BYTES + MAX_BODY_BYTES;

const normalizedHost = (value) =>
  String(value).replace(/^\[|\]$/g, "").replace(/\.$/, "").toLowerCase();

const startPinnedProxy = (target, { signal }) => new Promise((resolve, reject) => {
  const expectedHost = normalizedHost(target.url.hostname);
  const expectedPort = Number(target.url.port || (target.url.protocol === "https:" ? 443 : 80));
  const sockets = new Set();
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => socket.destroy());
    let head = Buffer.alloc(0);
    const onData = (chunk) => {
      head = Buffer.concat([head, chunk]);
      if (head.length > MAX_HEAD_BYTES) { socket.destroy(); return; }
      const end = head.indexOf("\r\n\r\n");
      if (end < 0) return;
      socket.off("data", onData);
      const line = head.subarray(0, head.indexOf("\r\n")).toString("latin1");
      const match = /^CONNECT\s+([^\s]+):(\d+)\s+HTTP\/1\.[01]$/i.exec(line);
      if (!match || Number(match[2]) !== expectedPort || normalizedHost(match[1]) !== expectedHost) {
        socket.destroy();
        return;
      }
      const upstream = net.connect({ host: target.address, port: expectedPort });
      upstream.on("error", () => socket.destroy());
      socket.on("close", () => upstream.destroy());
      upstream.once("connect", () => {
        socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
        const rest = head.subarray(end + 4);
        if (rest.length > 0) upstream.write(rest);
        socket.pipe(upstream);
        upstream.pipe(socket);
      });
    };
    socket.on("data", onData);
  });
  const close = () => {
    for (const socket of sockets) socket.destroy();
    server.close();
  };
  server.on("error", reject);
  if (signal.aborted) { close(); reject(new RelayTransportError("cancelled")); return; }
  server.listen(0, "127.0.0.1", () => resolve({
    port: server.address().port,
    close: () => {
      for (const socket of sockets) socket.destroy();
      server.close();
    },
  }));
});

// wget --save-headers の出力 (応答ヘッド + 本文) を分解する。
const parseWgetOutput = (buffer) => {
  const end = buffer.indexOf("\r\n\r\n");
  if (end < 0 || end > MAX_HEAD_BYTES) throw new RelayTransportError("incomplete");
  const lines = buffer.subarray(0, end).toString("utf8").split("\r\n");
  const status = Number((lines[0].match(/^HTTP\/[\d.]+ (\d{3})/) || [])[1]);
  if (!(status >= 200 && status <= 599)) throw new RelayTransportError("incomplete");
  const headers = [];
  for (const line of lines.slice(1)) {
    const separator = line.indexOf(":");
    if (separator > 0) headers.push([line.slice(0, separator).trim(), line.slice(separator + 1).trim()]);
  }
  const body = buffer.subarray(end + 4);
  if (body.length > MAX_BODY_BYTES) throw new RelayTransportError("too-large");
  return {
    status,
    contentType: headerValue(headers, "content-type"),
    location: headerValue(headers, "location") || undefined,
    setCookie: headers.filter(([name]) => name.toLowerCase() === "set-cookie").map(([, value]) => value),
    body,
    via: "wget",
  };
};

/// GNU wget 経由の取得。Cloudflare は同じ IP でもクライアント指紋で結果が
/// 変わることがあるため、curl が 403 になる環境向けの代替経路として使う。
/// 接続先はローカル CONNECT プロキシが固定し、Wgetrc と環境プロキシは潰す。
export const viaWget = async (target, headers, { wgetBin, cacert = null, timeoutMs, signal }) => {
  if (signal.aborted) throw new RelayTransportError("cancelled");
  // HTTP はプロキシ経由の絶対形式になり TLS 固定の前提が崩れるため、curl/Node に任せる。
  if (target.url.protocol !== "https:") throw new RelayTransportError("failed");
  const proxy = await startPinnedProxy(target, { signal });
  const local = new AbortController();
  const timer = setTimeout(() => local.abort(), timeoutMs);
  timer.unref();
  const combined = AbortSignal.any([signal, local.signal]);
  let child;
  let leftover = null;
  let length = 0;
  const chunks = [];
  try {
    const userAgent = headerValue(headers, "user-agent");
    const seconds = Math.max(1, Math.ceil(timeoutMs / 1000));
    const args = [
      // wgetrc を読ませない。ユーザー設定のプロキシ・ヘッダで経路が変わらないようにする。
      // (--no-config は Wget 1.16 以降)
      "--no-config",
      "--no-verbose", "--save-headers", "--content-on-error",
      "--output-document=-", "--tries=1", "--max-redirect=0",
      `--connect-timeout=${Math.min(10, seconds)}`,
      `--read-timeout=${seconds}`, `--timeout=${seconds}`,
    ];
    if (cacert) args.push(`--ca-certificate=${cacert}`);
    if (userAgent) args.push(`--user-agent=${userAgent}`);
    for (const [name, value] of headers) {
      if (WGET_UNFORWARDED.has(name.toLowerCase())) continue;
      args.push(`--header=${name}: ${value}`);
    }
    args.push("--", target.url.href);
    const proxyUrl = `http://127.0.0.1:${proxy.port}`;
    child = spawn(wgetBin, args, {
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
      env: {
        ...process.env,
        http_proxy: proxyUrl, https_proxy: proxyUrl,
        HTTP_PROXY: proxyUrl, HTTPS_PROXY: proxyUrl,
        no_proxy: "", NO_PROXY: "",
      },
    });
    return await new Promise((resolve, reject) => {
      let settled = false;
      const finish = (callback, value) => { if (settled) return; settled = true; callback(value); };
      const stop = (reason) => {
        leftover ||= new RelayTransportError(reason);
        try { child.kill("SIGKILL"); } catch { /* already gone */ }
        finish(reject, leftover);
      };
      const onAbort = () => stop(signal.aborted ? "cancelled" : "timed-out");
      combined.addEventListener("abort", onAbort, { once: true });
      child.stdout.on("data", (chunk) => {
        length += chunk.length;
        if (length > WGET_OUTPUT_LIMIT) return stop("too-large");
        if (!settled) chunks.push(chunk);
      });
      // 診断は捨てる。URL・Cookie・資格情報を含む可能性がある。
      child.stderr.on("data", () => {});
      child.on("error", () => stop("not-installed"));
      child.once("close", (code) => {
        combined.removeEventListener("abort", onAbort);
        if (settled) return;
        if (combined.aborted) return finish(reject, leftover || new RelayTransportError("cancelled"));
        try {
          finish(resolve, parseWgetOutput(Buffer.concat(chunks, length)));
        } catch (error) {
          finish(reject, error instanceof RelayTransportError
            ? error
            : new RelayTransportError(code === 8 ? "failed" : "incomplete"));
        }
      });
    });
  } finally {
    clearTimeout(timer);
    proxy.close();
    if (child && child.exitCode === null) { try { child.kill("SIGKILL"); } catch { /* already gone */ } }
  }
};

// global fetch では DNS を固定できないため、代替経路も標準 HTTP(S) の lookup を固定する。
// TLS は URL の元のホスト名で検証し、プロキシ・リダイレクト・共有接続プールを使わない。
// `cacert` を渡したときは Node 同梱の既定ストアではなく、その CA バンドルだけで
// 検証する (侵害されたルート CA を除外したバンドルを全経路で共有するため)。
export const viaPinnedHttp = async (target, headers, { timeoutMs, signal, cacert = null }) => {
  const local = new AbortController();
  const timeout = setTimeout(() => local.abort(), timeoutMs);
  timeout.unref();
  const combined = AbortSignal.any([signal, local.signal]);
  const secure = target.url.protocol === "https:";
  const client = secure ? https : http;
  const agentOptions = { keepAlive: false, proxyEnv: {}, rejectUnauthorized: true };
  if (secure && cacert) {
    try {
      agentOptions.ca = readFileSync(cacert);
    } catch {
      throw new RelayTransportError("ca-bundle");
    }
  }
  const agent = new client.Agent(agentOptions);
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
