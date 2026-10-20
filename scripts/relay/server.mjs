// 外部 HTTP 取得リレー。Cloud Run (Buildpacks / npm start) と PteWorker
// (SORAHOST / sorahost.json の start) が同じファイルを動かす。ホストごとの差は
// env.mjs (PORT・待受・.env) と transport.mjs (curl の解決) だけが吸収する。
//
// curl を優先する。同じ回線でも Node の TLS クライアントだけ 403 になる実測がある。
// 取得先は許可ホスト・公開 DNS を検証して固定し、どの経路でもリダイレクトを追わない。
// Cookie・Authorization・取得 URL・子プロセスの生の診断はログへ出さない。

import { createServer, validateHeaderName, validateHeaderValue } from "node:http";
import { createHash, timingSafeEqual } from "node:crypto";
import { pathToFileURL } from "node:url";
import {
  MAX_HEAD_BYTES,
  MAX_REQUEST_BYTES,
  assertResponseMetadata,
  decodeHeaderJson,
  headerPairs,
  parseJsonObject,
  readBoundedBytes,
} from "./worker/protocol.mjs";
import { loadEnvFile, resolveHost, resolvePort } from "./env.mjs";
import { createTargetResolver, normalizeHost, RelayTargetError } from "./security.mjs";
import { resolveCurl, viaCurl, viaPinnedHttp, RelayTransportError } from "./transport.mjs";

const digest = (value) => createHash("sha256").update(value).digest();

const withSignal = async (promise, signal) => {
  let cancel;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        cancel = () => reject(new RelayTransportError("cancelled"));
        if (signal.aborted) cancel();
        else signal.addEventListener("abort", cancel, { once: true });
      }),
    ]);
  } finally {
    signal.removeEventListener("abort", cancel);
  }
};

// IncomingMessage のキャンセルでソケットを破棄せず、400 を返して残りを捨てる。
const readBody = async (req) => {
  const chunks = req.iterator({ destroyOnReturn: false });
  const stream = new ReadableStream({
    async pull(controller) {
      try {
        const { done, value } = await chunks.next();
        if (done) controller.close();
        else controller.enqueue(value);
      } catch (error) {
        controller.error(error);
      }
    },
    async cancel() {
      try {
        await chunks.return();
      } finally {
        req.resume();
      }
    },
  }, { highWaterMark: 0 });
  return parseJsonObject(await readBoundedBytes(stream, {
    limit: MAX_REQUEST_BYTES,
    contentLength: req.headers["content-length"] ?? null,
    label: "request",
  }));
};

const forwardHeaders = (raw, target) => {
  if (raw != null && (typeof raw !== "object" || Array.isArray(raw))) throw new Error();
  for (const value of Object.values(raw || {})) {
    if (typeof value !== "string" && !(Array.isArray(value) && value.every((item) => typeof item === "string"))) {
      throw new Error();
    }
  }
  const pairs = headerPairs(raw);
  let hosts = 0;
  for (const [name, value] of pairs) {
    validateHeaderName(name);
    validateHeaderValue(name, Buffer.from(value, "utf8").toString("latin1"));
    if (name.toLowerCase() === "host") {
      const host = new URL(`${target.protocol}//${value}`);
      if (++hosts > 1 || host.username || host.password || host.pathname !== "/"
        || host.search || host.hash || normalizeHost(host.hostname) !== normalizeHost(target.hostname)
        || host.port !== target.port) throw new Error();
    }
  }
  return pairs;
};

// テストは別ディレクトリの fixture から resolver を注入する。本番に私設 IP 許可設定は置かない。
export const createRelayServer = ({
  token = process.env.NAROU_RELAY_KEY || "",
  allowedHosts = process.env.NAROU_RELAY_ALLOWED_HOSTS || "",
  resolveTarget,
  curl,
  curlBin,
  curlCacert = null,
  curlTimeoutMs = 30_000,
  fallbackTimeoutMs = 30_000,
  requestTimeoutMs = 75_000,
  logger = (message) => console.warn(message),
} = {}) => {
  if (typeof token !== "string" || !token) throw new Error("NAROU_RELAY_KEY is required");
  // curlBin / curlCacert はテストと明示指定用。未指定なら同梱 curl を探す。
  const curlConfig = curl ?? (curlBin ? { bin: curlBin, cacert: curlCacert } : resolveCurl({ env: process.env }));
  const expectedToken = digest(token);
  const resolve = resolveTarget || createTargetResolver(allowedHosts);
  const handler = async (req, res) => {
    const json = (status, value) => {
      if (res.destroyed || res.writableEnded || res.headersSent) return;
      const body = JSON.stringify(value);
      res.writeHead(status, {
        "content-type": "application/json",
        "content-length": Buffer.byteLength(body),
        ...(status >= 400 ? { connection: "close" } : {}),
      });
      res.end(body);
    };
    const abort = new AbortController();
    const disconnect = () => { if (!res.writableFinished) abort.abort(); };
    req.once("aborted", disconnect);
    res.once("close", disconnect);
    const deadline = setTimeout(() => abort.abort(), requestTimeoutMs);
    deadline.unref();
    try {
      let requestUrl;
      try { requestUrl = new URL(req.url, "http://localhost"); } catch { return json(400, { error: "invalid request" }); }
      if (requestUrl.pathname === "/health") {
        return req.method === "GET" ? json(200, { ok: true }) : json(405, { error: "method not allowed" });
      }
      if (requestUrl.pathname !== "/proxy") return json(404, { error: "not found" });
      const supplied = req.headers["x-proxy-token"];
      if (!timingSafeEqual(expectedToken, digest(typeof supplied === "string" ? supplied : ""))) {
        return json(403, { error: "forbidden" });
      }
      let payload;
      try {
        if (req.method === "POST") {
          payload = await withSignal(readBody(req), abort.signal);
        } else if (req.method === "GET") {
          const raw = requestUrl.searchParams.get("headers");
          payload = {
            url: requestUrl.searchParams.get("url") || "",
            headers: raw === null ? {} : decodeHeaderJson(raw),
          };
        } else {
          return json(405, { error: "method not allowed" });
        }
      } catch {
        return json(abort.signal.aborted ? 502 : 400, { error: abort.signal.aborted ? "request timed out" : "invalid request" });
      }
      const target = await withSignal(resolve(payload.url, { signal: abort.signal }), abort.signal);
      let headers;
      try { headers = forwardHeaders(payload.headers, target.url); } catch { return json(400, { error: "invalid request headers" }); }
      let result;
      try {
        result = await viaCurl(target, headers, {
          curlBin: curlConfig.bin, cacert: curlConfig.cacert, timeoutMs: curlTimeoutMs, signal: abort.signal,
        });
      } catch (error) {
        if (!(error instanceof RelayTransportError)) throw error;
        logger(`narou relay curl: ${error.reason}`);
        if (["cancelled", "too-large", "tls-verification"].includes(error.reason)) throw error;
        result = await viaPinnedHttp(target, headers, { timeoutMs: fallbackTimeoutMs, signal: abort.signal });
      }
      const { body, ...metadata } = result;
      try { assertResponseMetadata(metadata); } catch { throw new RelayTransportError("too-large"); }
      return json(200, { ...metadata, body: body.toString("base64") });
    } catch (error) {
      if (error instanceof RelayTargetError) return json(error.status, { error: error.message });
      if (error instanceof RelayTransportError) {
        logger(`narou relay request: ${error.reason}`);
        return json(502, { error: error.message });
      }
      logger("narou relay internal failure");
      return json(500, { error: "internal server error" });
    } finally {
      clearTimeout(deadline);
      req.removeListener("aborted", disconnect);
      res.removeListener("close", disconnect);
      abort.abort();
      req.resume();
    }
  };
  const server = createServer({ maxHeaderSize: MAX_HEAD_BYTES }, (req, res) => {
    handler(req, res).catch(() => {
      logger("narou relay internal failure");
      if (!res.headersSent && !res.destroyed) {
        res.writeHead(500, { "content-type": "application/json", connection: "close" });
        res.end('{"error":"internal server error"}');
      } else if (!res.writableEnded) res.destroy();
    });
  });
  server.requestTimeout = requestTimeoutMs;
  server.headersTimeout = Math.min(10_000, requestTimeoutMs);
  return server;
};

export const startRelay = ({ env = process.env, ...options } = {}) => {
  // PteWorker (SORAHOST) は sorahost.json から環境変数を渡せないため .env も読む。
  // Cloud Run では環境変数だけで完結し、この読み込みは何もしない。
  const loaded = loadEnvFile({ env });
  if (loaded) {
    const applied = loaded.unreadable ? "読み取り失敗" : (loaded.applied.join(", ") || "新しい値なし");
    console.log(`narou relay: ${loaded.path} を読み込みました (${applied})`);
  }
  const port = resolvePort(env);
  const host = resolveHost(env);
  const curl = options.curl
    ?? (options.curlBin ? { bin: options.curlBin, cacert: options.curlCacert ?? null, source: "env" } : resolveCurl({ env }));
  const server = createRelayServer({
    token: env.NAROU_RELAY_KEY || "",
    allowedHosts: env.NAROU_RELAY_ALLOWED_HOSTS || "",
    ...options,
    curl,
  });
  server.listen(port, host, () => console.log(`narou relay listening on ${host}:${server.address().port} (curl: ${curl.source})`));
  return server;
};

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    startRelay().once("error", () => {
      console.error("narou relay listener failed");
      process.exitCode = 1;
    });
  } catch {
    console.error("narou relay startup failed: check key, allowed hosts and PORT");
    process.exitCode = 1;
  }
}
