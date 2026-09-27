// SORAHOST (PteWorker) の worker モード用 取得リレー。
//
// node モード (../server.mjs) と同じ `POST /proxy` プロトコルを実装する。
// 違いはクライアント:
//   node モード : curl を起動して取得 (実測で 200 が取れる)
//   worker モード: ランタイムの fetch を使う (同一回線の実測では Node の
//                  fetch は 403 になり、curl は 200 だった)
// どちらの踏み台がハーメルンを通るかは、SORAHOST の出口 IP 次第なので
// 両方をデプロイして比べる。
//
// 呼び出し (Worker 側):
//   POST /proxy  { "url": "...", "headers": {...}, "redirect": "follow"|"manual" }
//   → { "status": 200, "contentType": "...", "body": "<base64>" }
//
// 認証: X-Proxy-Token ヘッダが環境変数 PROXY_TOKEN と一致しない限り 403。

const MAX_BODY_BYTES = 16 * 1024 * 1024;

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
    const token = env?.PROXY_TOKEN ?? "";
    if (!token) return json(500, { error: "PROXY_TOKEN is not set" });
    if (url.pathname === "/health") return json(200, { ok: true });
    if (request.method !== "POST" || url.pathname !== "/proxy") return json(404, { error: "not found" });
    if (request.headers.get("x-proxy-token") !== token) return json(403, { error: "forbidden" });

    let payload;
    try {
      payload = await request.json();
    } catch {
      return json(400, { error: "bad request: expected JSON body" });
    }

    const target = String(payload.url || "");
    if (!/^https?:\/\//.test(target)) return json(400, { error: "url must be http(s)" });

    const headers = {};
    for (const [name, value] of Object.entries(payload.headers || {})) {
      if (typeof value === "string") headers[name] = value;
    }

    try {
      const response = await fetch(target, {
        headers,
        redirect: payload.redirect === "manual" ? "manual" : "follow",
      });
      const body = new Uint8Array(await response.arrayBuffer());
      if (body.length > MAX_BODY_BYTES) {
        return json(502, { error: `response too large: ${body.length}` });
      }
      return json(200, {
        status: response.status,
        contentType: response.headers.get("content-type") || "",
        location: response.headers.get("location") || undefined,
        via: "worker-fetch",
        body: toBase64(body),
      });
    } catch (error) {
      return json(502, { error: `fetch failed: ${error.message}` });
    }
  },
};
