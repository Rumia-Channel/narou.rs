// SORAHOST (PteWorker) 上で動かす取得プロキシ。
//
// 目的: Cloudflare Workers から取得できないサイト (ハーメルン等) を、
// SORAHOST の出口 IP から取得して中継する。依存パッケージなし。
//
// 実測にもとづき curl を主クライアントにする:
//   同じ回線・同じ UA でも curl は 200、Node の https / fetch(undici) は
//   403 (Cloudflare のチャレンジ) になる。クライアント実装 (TLS フィンガー
//   プリント等) が判定に効くため、curl を使う。
//
// PteWorker はループバックへのバインドと PORT 環境変数の使用を求めるため、
// 127.0.0.1:$PORT で待ち受ける。
//
// 使い方:
//   PROXY_TOKEN=<長いランダム文字列> node server.mjs
//
// 呼び出し (Worker 側):
//   POST /proxy  { "url": "https://syosetu.org/novel/426898/1.html",
//                  "headers": { "user-agent": "...", "cookie": "..." },
//                  "redirect": "follow" | "manual" }
//   → { "status": 200, "contentType": "text/html; charset=UTF-8", "body": "<base64>" }
//
// 認証: X-Proxy-Token ヘッダが PROXY_TOKEN と一致しない限り 403 を返す。
// オープンプロキシ化を防ぐため PROXY_TOKEN 未設定では起動しない。

import { createServer } from "node:http";
import { execFile } from "node:child_process";
import { existsSync, copyFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";

const PORT = Number(process.env.PORT || 3000);
const TOKEN = process.env.PROXY_TOKEN || "";
const MAX_BODY_BYTES = 16 * 1024 * 1024;
const FETCH_TIMEOUT_MS = 30_000;
const CURL_TIMEOUT_SECS = 30;

// CI でビルドした静的 curl をデプロイに同梱している場合はそれを使う。
// (Pterodactyl のコンテナには curl / wget / python3 が入っていないため)
const BUNDLED_CURL = "./curl";
const BUNDLED_CACERT = "./cacert.pem";
// デプロイ先は読み取り専用で実行ビットも落ちるため、書き込み可能な場所へ
// コピーしてから実行権限を付ける。
const prepareCurl = () => {
  if (!existsSync(BUNDLED_CURL)) return "curl";
  for (const candidate of [`${tmpdir()}/.narou-relay-curl`, "./.relay-curl"]) {
    try {
      copyFileSync(BUNDLED_CURL, candidate);
      chmodSync(candidate, 0o755);
      return candidate;
    } catch {
      /* 次の候補へ */
    }
  }
  return "curl";
};
const CURL_BIN = prepareCurl();

if (!TOKEN) {
  console.error("PROXY_TOKEN が未設定です。長いランダム文字列を設定してください。");
  process.exit(1);
}

const readBody = async (req) => {
  let body = "";
  for await (const chunk of req) {
    body += chunk;
    if (body.length > 64 * 1024) throw new Error("request too large");
  }
  return body;
};

/// curl で取得する。`-i` でヘッダも stdout に出すので、最初の空行で分ける。
const viaCurl = (url, headers, manual) =>
  new Promise((resolve, reject) => {
    const args = [
      "-sS",
      "-i",
      "--compressed",
      "--connect-timeout",
      "10",
      "--max-time",
      String(CURL_TIMEOUT_SECS),
      "--max-redirs",
      manual ? "0" : "5",
      "--max-filesize",
      String(MAX_BODY_BYTES),
    ];
    if (existsSync(BUNDLED_CACERT)) {
      args.push("--cacert", BUNDLED_CACERT);
    }
    for (const [name, value] of Object.entries(headers)) {
      args.push("-H", `${name}: ${value}`);
    }
    args.push("--", url);
    execFile(CURL_BIN, args, { maxBuffer: MAX_BODY_BYTES, encoding: "buffer" }, (error, stdout) => {
      if (error && !stdout?.length) {
        return reject(new Error(`curl failed: ${error.message}`));
      }
      const buffer = Buffer.isBuffer(stdout) ? stdout : Buffer.from(stdout ?? "");
      const split = buffer.indexOf("\r\n\r\n");
      if (split < 0) return reject(new Error("curl returned no header block"));
      const head = buffer.subarray(0, split).toString("utf8");
      const body = buffer.subarray(split + 4);
      const status = Number((head.match(/^HTTP\/[\d.]+ (\d{3})/m) || [])[1] || 0);
      const contentType = (head.match(/^content-type:\s*(.+)$/im) || [])[1]?.trim() || "";
      const location = (head.match(/^location:\s*(.+)$/im) || [])[1]?.trim() || undefined;
      resolve({ status, contentType, location, body });
    });
  });

/// curl が無い環境向けの保険。Node の fetch は Cloudflare にチャレンジされ
/// やすいため、あくまで代替。
const viaFetch = async (url, headers, manual) => {
  const response = await fetch(url, {
    headers,
    redirect: manual ? "manual" : "follow",
    signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
  });
  const body = Buffer.from(await response.arrayBuffer());
  return {
    status: response.status,
    contentType: response.headers.get("content-type") || "",
    location: response.headers.get("location") || undefined,
    body,
  };
};

const handler = async (req, res) => {
  const json = (status, value) => {
    res.writeHead(status, { "content-type": "application/json" });
    res.end(JSON.stringify(value));
  };

  if (req.method === "GET" && req.url.startsWith("/health")) {
    return json(200, { ok: true });
  }
  const requestUrl = new URL(req.url, "http://localhost");
  if (requestUrl.pathname !== "/proxy") {
    return json(404, { error: "not found" });
  }
  if (req.headers["x-proxy-token"] !== TOKEN) {
    return json(403, { error: "forbidden" });
  }

  // POST (JSON ボディ) と GET (クエリ) の両方を受け付ける。Worker の生ソケット
  // 経路は HTTP/1.1 の GET しか話せないため、GET も必須。
  let payload;
  if (req.method === "POST") {
    try {
      payload = JSON.parse(await readBody(req));
    } catch (error) {
      return json(400, { error: `bad request: ${error.message}` });
    }
  } else if (req.method === "GET") {
    let headers = {};
    const rawHeaders = requestUrl.searchParams.get("headers");
    if (rawHeaders) {
      try {
        headers = JSON.parse(Buffer.from(rawHeaders, "base64").toString("utf8"));
      } catch {
        return json(400, { error: "bad request: headers must be base64 JSON" });
      }
    }
    payload = {
      url: requestUrl.searchParams.get("url") || "",
      headers,
      redirect: requestUrl.searchParams.get("redirect") || "follow",
    };
  } else {
    return json(405, { error: "method not allowed" });
  }

  const target = String(payload.url || "");
  if (!/^https?:\/\//.test(target)) {
    return json(400, { error: "url must be http(s)" });
  }

  const headers = {};
  for (const [name, value] of Object.entries(payload.headers || {})) {
    if (typeof value === "string") headers[name] = value;
  }
  const manual = payload.redirect === "manual";

  try {
    let result;
    try {
      result = await viaCurl(target, headers, manual);
    } catch (curlError) {
      result = await viaFetch(target, headers, manual);
      result.via = `fetch (curl unavailable: ${curlError.message})`;
    }
    if (result.body.length > MAX_BODY_BYTES) {
      return json(502, { error: `response too large: ${result.body.length}` });
    }
    return json(200, {
      status: result.status,
      contentType: result.contentType,
      location: result.location,
      via: result.via,
      body: result.body.toString("base64"),
    });
  } catch (error) {
    return json(502, { error: `fetch failed: ${error.message}` });
  }
};

createServer((req, res) => {
  handler(req, res).catch((error) => {
    res.writeHead(500, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: String(error) }));
  });
}).listen(PORT, "127.0.0.1", () => {
  console.log(`sorahost proxy listening on 127.0.0.1:${PORT}`);
});
