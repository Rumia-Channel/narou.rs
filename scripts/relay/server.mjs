// 外部サーバー上で動かす取得リレー。
//
// 目的: Cloudflare Workers から取得できないサイト (ハーメルン等) を、
// 配置先の出口 IP から取得して中継する。依存パッケージなし。
//
// 実測にもとづき curl を主クライアントにする:
//   同じ回線・同じ UA でも curl は 200、Node の https / fetch(undici) は
//   403 (Cloudflare のチャレンジ) になる。クライアント実装 (TLS フィンガー
//   プリント等) が判定に効くため、curl を使う。
//
// TLS 終端などのリバースプロキシの背後で動かすため、
// 127.0.0.1:$PORT で待ち受ける。
//
// 使い方:
//   NAROU_RELAY_KEY=<長いランダム文字列> node server.mjs
//
// 呼び出し (Worker 側):
//   POST /proxy  { "url": "https://syosetu.org/novel/426898/1.html",
//                  "headers": { "user-agent": "...", "cookie": "..." } }
//   → { "status": 200, "contentType": "text/html; charset=UTF-8",
//        "location": "...", "setCookie": ["..."], "body": "<base64>" }
//
// 取得は 1 回だけでリダイレクトは辿らない (`Location` を返す)。チェーンを辿る
// 責務は呼び出し側にあり、hop ごとの検証もそちらが行う。curl -L を無条件に
// 付けると資格情報を私設アドレスへ運びうるため、ここでは辿らない。
//
// 同名ヘッダは値の配列で受け取り (`{"X": ["a", "b"]}`)、受けた順に送る。
// 応答の Set-Cookie は全部 `setCookie` に載せる (ログインセッションの更新は
// 1 本も落とせない)。
//
// 認証: X-Proxy-Token ヘッダが NAROU_RELAY_KEY と一致しない限り 403 を返す。
// オープンプロキシ化を防ぐため NAROU_RELAY_KEY 未設定では起動しない。

import { createServer } from "node:http";
import { execFile } from "node:child_process";
import { existsSync, copyFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";

import {
  MAX_BODY_BYTES,
  MAX_HEAD_BYTES,
  MAX_REQUEST_BYTES,
  assertResponseMetadata,
  decodeHeaderJson,
  headerPairs,
  parseJsonObject,
  readBoundedBytes,
  readResponseBytes,
} from "./worker/protocol.mjs";
const PORT = Number(process.env.PORT || 3000);
const TOKEN = process.env.NAROU_RELAY_KEY || "";
const FETCH_TIMEOUT_MS = 30_000;
const CURL_TIMEOUT_SECS = 30;

// 配置先の curl を使う。静的 curl を同梱した場合はそれを優先する。
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
  console.error("NAROU_RELAY_KEY が未設定です。長いランダム文字列を設定してください。");
  process.exit(1);
}

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

/// `-i` 出力 (status 行 + ヘッダ + 空行 + 本文) を最後のヘッダブロックと本文に
/// 分ける。100 Continue や CONNECT 応答が混ざる場合は最後の status 行から
/// 始まるブロックだけが本体の応答。
const splitHead = (buffer) => {
  let offset = 0;
  for (;;) {
    const end = buffer.indexOf("\r\n\r\n", offset);
    if (end < 0) return null;
    const next = end + 4;
    if (next > MAX_HEAD_BYTES) return null;
    if (buffer.subarray(next, next + 5).toString("utf8").startsWith("HTTP/")) {
      offset = next;
      continue;
    }
    const lines = buffer.subarray(offset, end).toString("utf8").split("\r\n");
    const status = Number((lines[0].match(/^HTTP\/[\d.]+ (\d{3})/) || [])[1] || 0);
    const headers = [];
    for (const line of lines.slice(1)) {
      const separator = line.indexOf(":");
      if (separator > 0) {
        headers.push([line.slice(0, separator).trim(), line.slice(separator + 1).trim()]);
      }
    }
    return { status, headers, body: buffer.subarray(next) };
  }
};

const headerValue = (headers, name) =>
  headers.find(([header]) => header.toLowerCase() === name)?.[1] || "";

/// curl で取得する。リダイレクトは辿らない (呼び出し側が hop ごとに追跡する)。
const viaCurl = (url, headers) =>
  new Promise((resolve, reject) => {
    const args = [
      "-sS",
      "-i",
      "--compressed",
      "--connect-timeout",
      "10",
      "--max-time",
      String(CURL_TIMEOUT_SECS),
      "--max-filesize",
      String(MAX_BODY_BYTES),
    ];
    if (existsSync(BUNDLED_CACERT)) {
      args.push("--cacert", BUNDLED_CACERT);
    }
    for (const [name, value] of headers) {
      args.push("-H", `${name}: ${value}`);
    }
    args.push("--", url);
    execFile(CURL_BIN, args, {
      maxBuffer: MAX_HEAD_BYTES + MAX_BODY_BYTES,
      encoding: "buffer",
    }, (error, stdout) => {
      if (error) {
        // execFile の message は command / headers / stderr を含みうる。
        // 資格情報や任意長の診断を返さず、有限の分類だけを伝える。
        let reason = "request failed";
        if (Number.isSafeInteger(error.code)) reason = `exit ${error.code}`;
        else if (error.code === "ENOENT") reason = "not installed";
        else if (error.code === "EACCES") reason = "not executable";
        else if (error.code === "ERR_CHILD_PROCESS_STDIO_MAXBUFFER") reason = "output limit exceeded";
        else if (error.killed) reason = "terminated";
        return reject(new Error(`curl failed: ${reason}`));
      }
      const buffer = Buffer.isBuffer(stdout) ? stdout : Buffer.from(stdout ?? "");
      const split = splitHead(buffer);
      if (!split) return reject(new Error("curl returned no header block"));
      if (split.body.length > MAX_BODY_BYTES) {
        return reject(new Error(`response too large: ${split.body.length}`));
      }
      resolve({
        status: split.status,
        contentType: headerValue(split.headers, "content-type"),
        location: headerValue(split.headers, "location") || undefined,
        setCookie: split.headers
          .filter(([name]) => name.toLowerCase() === "set-cookie")
          .map(([, value]) => value),
        body: split.body,
      });
    });
  });

/// curl が無い環境向けの保険。Node の fetch は Cloudflare にチャレンジされ
/// やすいため、あくまで代替。curl 経路と同じくリダイレクトは辿らない。
const viaFetch = async (url, headers) => {
  const response = await fetch(url, {
    headers: headers.map(([name, value]) => [
      name, Buffer.from(value, "utf8").toString("latin1"),
    ]),
    redirect: "manual",
    signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
  });
  const bytes = await readResponseBytes(response);
  const body = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return {
    status: response.status,
    contentType: response.headers.get("content-type") || "",
    location: response.headers.get("location") || undefined,
    setCookie: response.headers.getSetCookie ? response.headers.getSetCookie() : [],
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
      payload = await readBody(req);
    } catch (error) {
      return json(400, { error: `bad request: ${error.message}` });
    }
  } else if (req.method === "GET") {
    let headers = {};
    const rawHeaders = requestUrl.searchParams.get("headers");
    if (rawHeaders !== null) {
      try {
        headers = decodeHeaderJson(rawHeaders);
      } catch (error) {
        return json(400, { error: `bad request: ${error.message}` });
      }
    }
    payload = {
      url: requestUrl.searchParams.get("url") || "",
      headers,
    };
  } else {
    return json(405, { error: "method not allowed" });
  }

  const target = String(payload.url || "");
  if (!/^https?:\/\//.test(target)) {
    return json(400, { error: "url must be http(s)" });
  }

  const headers = headerPairs(payload.headers);

  try {
    let result;
    try {
      result = await viaCurl(target, headers);
    } catch (curlError) {
      result = await viaFetch(target, headers);
      result.via = `fetch (curl unavailable: ${curlError.message})`;
    }
    const metadata = {
      status: result.status,
      contentType: result.contentType,
      location: result.location,
      setCookie: result.setCookie,
      via: result.via,
    };
    assertResponseMetadata(metadata);
    return json(200, { ...metadata, body: result.body.toString("base64") });
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
  console.log(`narou relay listening on 127.0.0.1:${PORT}`);
});
