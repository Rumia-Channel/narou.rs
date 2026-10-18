// fetch ランタイム用の外部取得リレー。
//
// Node 版 (../server.mjs) と同じ `POST /proxy` プロトコルを実装する。
// 違いはクライアント:
//   node モード : curl を起動して取得 (実測で 200 が取れる)
//   worker モード: ランタイムの fetch を使う (同一回線の実測では Node の
//                  fetch は 403 になり、curl は 200 だった)
// 取得可否はランタイムと出口 IP に依存するため、配置先で確認する。
//
// 呼び出し (Worker 側):
//   POST /proxy  { "url": "...", "headers": {...} }
//   → { "status": 200, "contentType": "...", "location": "...",
//        "setCookie": ["..."], "body": "<base64>" }
//
// node モードと同じく取得は 1 回だけでリダイレクトは辿らない (`Location` を
// 返し、追跡は呼び出し側が hop ごとの検証つきで行う)。同名ヘッダは値の配列で
// 受け取り、受けた順に送る。応答の Set-Cookie は全部 `setCookie` に載せる。
//
// 認証: X-Proxy-Token ヘッダが環境変数 NAROU_RELAY_KEY と一致しない限り 403。

import {
  MAX_REQUEST_BYTES,
  assertResponseMetadata,
  decodeHeaderJson,
  headerPairs,
  parseJsonObject,
  readBoundedBytes,
  readResponseBytes,
} from "./protocol.mjs";

const toBase64 = (bytes) => {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
};


export default {
  async fetch(request, env) {
    const json = (status, value) =>
      new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json" } });

    const url = new URL(request.url);
    const token = env?.NAROU_RELAY_KEY ?? "";
    if (!token) return json(500, { error: "NAROU_RELAY_KEY is not set" });
    if (url.pathname === "/health") return json(200, { ok: true });
    if (url.pathname !== "/proxy") return json(404, { error: "not found" });
    if (request.headers.get("x-proxy-token") !== token) return json(403, { error: "forbidden" });

    // POST (JSON body) と GET (クエリ) の両方を受け付ける。Cloudflare Workers の
    // `connect()` 経由で呼ぶ場合、HTTP/1.1 の GET が最も素直なため。
    let payload;
    if (request.method === "POST") {
      try {
        payload = parseJsonObject(await readBoundedBytes(request.body, {
          limit: MAX_REQUEST_BYTES,
          contentLength: request.headers.get("content-length"),
          label: "request",
          // Sending a 400 must not abort the inbound connection first.
          cancelOnLimit: false,
        }));
      } catch (error) {
        return json(400, { error: `bad request: ${error.message}` });
      }
    } else if (request.method === "GET") {
      const rawHeaders = url.searchParams.get("headers");
      let headers = {};
      if (rawHeaders !== null) {
        try {
          headers = decodeHeaderJson(rawHeaders);
        } catch (error) {
          return json(400, { error: `bad request: ${error.message}` });
        }
      }
      payload = { url: url.searchParams.get("url") || "", headers };
    } else {
      return json(405, { error: "method not allowed" });
    }

    const target = String(payload.url || "");
    if (!/^https?:\/\//.test(target)) return json(400, { error: "url must be http(s)" });

    const headers = headerPairs(payload.headers);

    try {
      // リダイレクトは辿らない。3xx は `location` 付きでそのまま返す。
      const response = await fetch(target, {
        headers,
        redirect: "manual",
      });
      const metadata = {
        status: response.status,
        contentType: response.headers.get("content-type") || "",
        location: response.headers.get("location") || undefined,
        setCookie: response.headers.getSetCookie
          ? response.headers.getSetCookie()
          : response.headers.getAll("Set-Cookie"),
        via: "worker-fetch",
      };
      try {
        assertResponseMetadata(metadata);
      } catch (error) {
        await response.body?.cancel(error).catch(() => {});
        throw error;
      }
      const body = await readResponseBytes(response);
      return json(200, { ...metadata, body: toBase64(body) });
    } catch (error) {
      return json(502, { error: `fetch failed: ${error.message}` });
    }
  },
};
