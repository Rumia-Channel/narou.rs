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
import { resolveCurl, resolveWget, viaCurl, viaPinnedHttp, viaWget, RelayTransportError } from "./transport.mjs";

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

const FETCH_TRANSPORTS = ["curl", "wget", "node"];

// 取得順は環境変数 NAROU_RELAY_FETCH で指定する (既定 curl,node)。
// Cloudflare が curl を 403 にするが wget は通るホストがあるため、VPS 等では
// `NAROU_RELAY_FETCH=wget,node` のように wget を先頭にできる。
export const resolveFetchOrder = (requested, { wget = null, logger = () => {} } = {}) => {
  const raw = requested == null ? (process.env.NAROU_RELAY_FETCH ?? "") : requested;
  const text = String(raw ?? "").trim().toLowerCase();
  const list = (text ? text.split(",") : ["curl", "node"]).map((name) => name.trim()).filter(Boolean);
  if (list.length === 0) return ["curl", "node"];
  const order = [];
  for (const name of list) {
    if (!FETCH_TRANSPORTS.includes(name)) throw new Error(`invalid NAROU_RELAY_FETCH: ${name}`);
    if (!order.includes(name)) order.push(name);
  }
  const resolved = order.filter((name) => {
    if (name !== "wget") return true;
    if (wget && wget.available !== false) return true;
    logger("wget が見つからないため取得順から外します (NAROU_RELAY_WGET または PATH の wget)");
    return false;
  });
  return resolved.length > 0 ? resolved : ["node"];
};

// テストは別ディレクトリの fixture から resolver を注入する。本番に私設 IP 許可設定は置かない。
export const createRelayServer = ({
  token = process.env.NAROU_RELAY_KEY || "",
  allowedHosts = process.env.NAROU_RELAY_ALLOWED_HOSTS || "",
  resolveTarget,
  curl,
  curlBin,
  curlCacert = null,
  wget,
  wgetBin,
  wgetCacert = null,
  // Node 経路の TLS 検証にも同じ CA バンドルを使う (指定時は既定ストアを置換する)。
  cacert = null,
  fetchOrder,
  curlTimeoutMs = 30_000,
  wgetTimeoutMs = 30_000,
  fallbackTimeoutMs = 30_000,
  requestTimeoutMs = 75_000,
  logger = (message) => console.warn(message),
} = {}) => {
  if (typeof token !== "string" || !token) throw new Error("NAROU_RELAY_KEY is required");
  // curlBin / curlCacert はテストと明示指定用。未指定なら同梱 curl を探す。
  const curlConfig = curl ?? (curlBin ? { bin: curlBin, cacert: curlCacert } : resolveCurl({ env: process.env }));
  // wget は curl が 403 になる環境向けの代替経路。注入された設定を最優先する。
  let wgetConfig;
  if (wget !== undefined) wgetConfig = wget;
  else if (wgetBin) wgetConfig = { bin: wgetBin, cacert: wgetCacert, source: "env", available: true };
  else {
    wgetConfig = resolveWget({ env: process.env });
    if (wgetCacert) wgetConfig = { ...wgetConfig, cacert: wgetCacert };
  }
  const order = resolveFetchOrder(fetchOrder, { wget: wgetConfig, logger });
  const expectedToken = digest(token);
  const resolve = resolveTarget || createTargetResolver(allowedHosts);
  // 取得順の 1 段を実行する。HTTP 応答が返ればそこで確定し、403 等でも
  // 次の経路へは進まない (1 リクエスト = 上流 1 回の契約を維持する)。
  const runFetch = (transport, target, headers, signal) => {
    switch (transport) {
      case "curl":
        return viaCurl(target, headers, {
          curlBin: curlConfig.bin, cacert: curlConfig.cacert, timeoutMs: curlTimeoutMs, signal,
        });
      case "wget":
        return viaWget(target, headers, {
          wgetBin: wgetConfig.bin, cacert: wgetConfig.cacert, timeoutMs: wgetTimeoutMs, signal,
        });
      case "node":
        return viaPinnedHttp(target, headers, { timeoutMs: fallbackTimeoutMs, signal, cacert });
      default:
        return Promise.reject(new RelayTransportError("failed"));
    }
  };
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
        return req.method === "GET"
          ? json(200, { ok: true, fetch: order.join(",") })
          : json(405, { error: "method not allowed" });
      }
      if (requestUrl.pathname !== "/proxy") return json(404, { error: "not found" });
      const supplied = req.headers["x-proxy-token"];
      if (!timingSafeEqual(expectedToken, digest(typeof supplied === "string" ? supplied : ""))) {
        // 403 の内訳を VPS のログで切り分けられるようにする (値は出さない)。
        logger("narou relay proxy: authentication failed (X-Proxy-Token mismatch)");
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
      let result = null;
      let lastError = null;
      for (const transport of order) {
        try {
          result = await runFetch(transport, target, headers, abort.signal);
          break;
        } catch (error) {
          if (!(error instanceof RelayTransportError)) throw error;
          logger(`narou relay ${transport}: ${error.reason}`);
          if (["cancelled", "too-large", "tls-verification"].includes(error.reason)) throw error;
          lastError = error;
        }
      }
      if (!result) throw lastError || new RelayTransportError("failed");
      const { body, ...metadata } = result;
      try { assertResponseMetadata(metadata); } catch { throw new RelayTransportError("too-large"); }
      // 上流の 403 は「リレー自身の認証拒否」と区別できるようログに残す (URL は出さない)。
      if (result.status === 403) logger("narou relay upstream status 403");
      return json(200, { ...metadata, body: body.toString("base64") });
    } catch (error) {
      if (error instanceof RelayTargetError) {
        logger(`narou relay target: ${error.message}`);
        return json(error.status, { error: error.message });
      }
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
  server.fetchOrder = order;
  server.fetchSummary = order.map((name) => {
    if (name === "curl") return `curl:${curlConfig.source ?? "path"}`;
    if (name === "wget") return `wget:${wgetConfig?.source ?? "path"}`;
    return name;
  }).join(", ");
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
  const wget = options.wget
    ?? (options.wgetBin
      ? { bin: options.wgetBin, cacert: options.wgetCacert ?? null, source: "env", available: true }
      : { ...resolveWget({ env }), ...(options.wgetCacert ? { cacert: options.wgetCacert } : {}) });
  // curl / wget / Node の全経路で同じ CA バンドルを使う。既知の侵害ルート
  // (eMudhra 中間 CA 侵害 2026-10) を含むバンドルでは起動しない (fail closed)。
  // 指定が無い場合だけ各クライアントの既定ストアに任せる。
  const caBundle = options.cacert
    ?? (String(env.NAROU_RELAY_CACERT ?? "").trim() || curl.cacert || wget.cacert || null);
  if (caBundle) {
    try {
      verifyCacertFile(caBundle);
    } catch (error) {
      throw new Error(`CA バンドルを検証できません: ${error && error.message ? error.message : error}`);
    }
  }
  const server = createRelayServer({
    token: env.NAROU_RELAY_KEY || "",
    allowedHosts: env.NAROU_RELAY_ALLOWED_HOSTS || "",
    ...options,
    curl,
    wget,
    cacert: caBundle,
    fetchOrder: options.fetchOrder ?? env.NAROU_RELAY_FETCH ?? "",
  });
  // Node 経路は既定の (更新が遅れ得る) バンドルを信頼してしまうため、
  // 代替経路として残っているときは明示的な CA バンドル設定を促す。
  if (server.fetchOrder.includes("node") && !caBundle) {
    console.warn("narou relay: Node 経路の CA バンドルが未設定です。NAROU_RELAY_CACERT に更新済みの CA バンドルを指定してください");
  }
  server.listen(port, host, () => console.log(`narou relay listening on ${host}:${server.address().port} (fetch: ${server.fetchSummary})`));
  return server;
};

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    startRelay().once("error", () => {
      console.error("narou relay listener failed");
      process.exitCode = 1;
    });
  } catch (error) {
    console.error(`narou relay startup failed: ${error && error.message ? error.message : "check key, allowed hosts and PORT"}`);
    process.exitCode = 1;
  }
}
