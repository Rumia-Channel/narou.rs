// Cloudflare Workers の契約テスト。
//
// HTTP だけを叩くので、ローカルの `wrangler dev` でもデプロイ済みの環境でも
// 同じものを使える。
//
//   NAROU_ADMIN_TOKEN=... BASE_URL=http://127.0.0.1:8787 node tests/contract.mjs
//
// Zero Trust (Cloudflare Access) を境界にして `NAROU_AUTH_REQUIRED=false` で
// 動かす場合は token 不要 (認証系の検査は自動で省略される)。Access の service
// token (`CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET`) を渡すと、Access の
// 内側へもそのまま流せる。前段が Access で弾く場合は exit 3 で「省略」を伝える。
//
// CONTRACT_QUEUE=1 のときだけ queue consumer が回る前提の検査 (Convert ジョブが
// 終端状態になるまで待つ) を行う。ローカルでも `wrangler dev` は queue を
// 処理するが、環境によっては配送されないので既定では無効にする。
import assert from "node:assert/strict";
import crypto from "node:crypto";
import http from "node:http";
import https from "node:https";
import process from "node:process";
import test from "node:test";

const BASE_URL = process.env.BASE_URL ?? "http://127.0.0.1:8787";
const TOKEN = process.env.NAROU_ADMIN_TOKEN;
const QUEUE_CHECKS = process.env.CONTRACT_QUEUE === "1";
const TERMINAL_STATUSES = new Set(["succeeded", "blocked", "permanent"]);
const AUTH_REQUIRED = (process.env.NAROU_AUTH_REQUIRED ?? "true").toLowerCase() !== "false";
// ローカルの `wrangler dev` にしか無い検査（テスト用エンドポイント等）を切り分ける。
const IS_LOCAL = /^https?:\/\/(127\.0\.0\.1|localhost|0\.0\.0\.0)([:/]|$)/.test(BASE_URL);
const ACCESS_CLIENT_ID = process.env.CF_ACCESS_CLIENT_ID;
const ACCESS_CLIENT_SECRET = process.env.CF_ACCESS_CLIENT_SECRET;

const UNCONFIGURED = process.env.CONTRACT_EXPECT_UNCONFIGURED === "1";
if (!TOKEN && !UNCONFIGURED && AUTH_REQUIRED) {
  console.error("NAROU_ADMIN_TOKEN is required (or set NAROU_AUTH_REQUIRED=false)");
  process.exit(2);
}

/** Access の service token を全リクエストへ載せる。 */
function accessHeaders() {
  if (!ACCESS_CLIENT_ID || !ACCESS_CLIENT_SECRET) {
    return {};
  }
  return {
    "cf-access-client-id": ACCESS_CLIENT_ID,
    "cf-access-client-secret": ACCESS_CLIENT_SECRET,
  };
}

function fetchWithAccess(input, init = {}) {
  return fetch(input, { ...init, headers: { ...accessHeaders(), ...(init.headers ?? {}) } });
}

// Access が前段にあると資格なしの GET は Access のログインへ飛ぶ。service token が
// 無い場合は smoke として意味が無いので、その旨を exit 3 で伝える。
// 到達できない場合（DNS・TLS・タイムアウト）はそのまま検査へ進み、失敗として報告する。
if (!ACCESS_CLIENT_ID && process.env.CONTRACT_SKIP_ACCESS_PROBE !== "1") {
  const attempts = Number(process.env.CONTRACT_PROBE_ATTEMPTS ?? 6);
  const delayMs = Number(process.env.CONTRACT_PROBE_DELAY_MS ?? 20_000);
  let unreachable = null;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    try {
      const probe = await fetchWithAccess(url("/health/live"), {
        redirect: "manual",
        signal: AbortSignal.timeout(10_000),
      });
      unreachable = null;
      const location = probe.headers.get("location") ?? "";
      if (
        [301, 302, 303, 307, 308].includes(probe.status) &&
        location.includes("cloudflareaccess.com")
      ) {
        console.log(`Cloudflare Access is in front of ${BASE_URL}; skipping the remote smoke`);
        process.exit(3);
      }
      break;
    } catch (error) {
      unreachable = error;
      if (IS_LOCAL || attempt === attempts) {
        break;
      }
      // 新しい custom domain は証明書の発行に少し時間がかかる。
      console.log(`probe ${attempt}/${attempts} failed (${error?.message ?? error}); retrying`);
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    }
  }
  if (unreachable) {
    if (IS_LOCAL) {
      console.warn(`probe failed (${unreachable?.message ?? unreachable}); running the checks anyway`);
    } else {
      console.log(`${BASE_URL} is not reachable (${unreachable?.message ?? unreachable})`);
      process.exit(4);
    }
  }
}

/** 検査を 1 件定義する（`node --test` で走る）。 */
function check(name, fn) {
  if (process.env.CONTRACT_EXPECT_UNCONFIGURED === "1") {
    return;
  }
  test(name, fn);
}

/** ローカルの `wrangler dev` でだけ意味がある検査。 */
function checkLocal(name, fn) {
  if (!IS_LOCAL) {
    return;
  }
  check(name, fn);
}

/** 認証が無効な環境 (Zero Trust) では走らせない検査。 */
function checkWithAuth(name, fn) {
  if (!AUTH_REQUIRED) {
    return;
  }
  check(name, fn);
}

/** 認証ヘッダ付き (token 省略時は付けない = 未認証の検査用)。 */
function auth(token = TOKEN) {
  return token ? { headers: { authorization: `Bearer ${token}` } } : {};
}

function url(path) {
  return new URL(path, BASE_URL).toString();
}

async function json(response) {
  const text = await response.text();
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`expected JSON, got ${response.status}: ${text.slice(0, 200)}`);
  }
}

async function request(path, init = {}) {
  return fetchWithAccess(url(path), init);
}

/** エラー応答の機械可読コードを取り出す。 */
async function errorCode(response) {
  const body = await json(response);
  return body?.error?.code;
}

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** 設定ページと同じ経路で local/global 設定を保存する。 */
async function saveSettings(settings) {
  const response = await request("/api/global_setting", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ settings }),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.success === true, `settings save failed: ${JSON.stringify(body)}`);
}

/** GET /api/global_setting の view。 */
async function fetchSettingsView() {
  return json(await request("/api/global_setting", auth()));
}

/** 設定ページの (name → value) スナップショット。 */
function settingValues(view) {
  const values = new Map();
  for (const item of view.settings ?? []) {
    values.set(item.name, item.value);
  }
  return values;
}

/** `/api/global_setting` を素の応答 (status + body) で叩く (失敗応答の検査用)。 */
async function postSettings(body) {
  const response = await request("/api/global_setting", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify(body),
  });
  return { status: response.status, body: await json(response) };
}

/** 1 プランを `/api/jobs` で投入し、受理された job_id を返す。 */
async function enqueuePlan(kind, target) {
  const response = await request("/api/jobs", {
    method: "POST",
    ...auth(),
    body: JSON.stringify({ kind, targets: [String(target)], options: [] }),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(
    Array.isArray(body.ids) && body.ids.length === 1,
    `${kind} must be accepted: ${JSON.stringify(body)}`,
  );
  return body.ids[0];
}

/** ジョブが終端状態 (succeeded / blocked / permanent) になるまで待つ。 */
async function waitForTerminal(jobId, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  let status = "pending";
  while (Date.now() < deadline) {
    const body = await json(await request(`/api/jobs/${jobId}`, auth()));
    status = body.status;
    if (TERMINAL_STATUSES.has(status)) return status;
    await delay(500);
  }
  throw new Error(`job ${jobId} stayed ${status} for ${timeoutMs}ms`);
}

/** ジョブが指定 status になるまで待つ。 */
async function waitForStatus(jobId, expected, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  let status = "pending";
  while (Date.now() < deadline) {
    const body = await json(await request(`/api/jobs/${jobId}`, auth()));
    status = body.status;
    if (status === expected) return;
    await delay(200);
  }
  throw new Error(`job ${jobId} never became ${expected} (last: ${status})`);
}

/**
 * 依存なしの最小 WebSocket クライアント (Node の WebSocket はテスト用の
 * Authorization ヘッダを送れないため、`http` の upgrade を直接使う)。
 * server → client の text frame だけを JSON として記録する。
 */
async function openWebSocket(path = "/ws") {
  const target = new URL(path, BASE_URL);
  const transport = target.protocol === "https:" ? https : http;
  const { socket, head } = await new Promise((resolve, reject) => {
    const upgrade = transport.request(target, {
      headers: {
        Connection: "Upgrade",
        Upgrade: "websocket",
        "Sec-WebSocket-Key": crypto.randomBytes(16).toString("base64"),
        "Sec-WebSocket-Version": "13",
        ...accessHeaders(),
        ...(TOKEN ? { authorization: `Bearer ${TOKEN}` } : {}),
      },
    });
    // 101 と同じ TCP セグメントで届いた frame は `head` に入るので捨てない。
    upgrade.once("upgrade", (_response, socket, head) => resolve({ socket, head }));
    upgrade.once("response", (response) =>
      reject(new Error(`ws upgrade rejected: ${response.statusCode}`)),
    );
    upgrade.once("error", reject);
    upgrade.end();
  });
  return new WebSocketRecorder(socket, head);
}

class WebSocketRecorder {
  constructor(socket, head) {
    this.socket = socket;
    this.messages = [];
    this.buffer = Buffer.alloc(0);
    socket.on("data", (chunk) => this.ingest(chunk));
    socket.on("error", () => {});
    if (head?.length) {
      this.ingest(head);
    }
  }

  ingest(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    for (;;) {
      const frame = decodeWsFrame(this.buffer);
      if (!frame) break;
      this.buffer = frame.rest;
      if (frame.opcode === 0x8) {
        this.socket.end();
        this.buffer = Buffer.alloc(0);
        break;
      }
      if (frame.opcode === 0x1 && frame.fin) {
        try {
          this.messages.push(JSON.parse(frame.payload));
        } catch {
          // JSON 以外 (将来のバイナリ等) は無視する。
        }
      }
    }
  }

  /** 指定位置からのイベント列。 */
  slice(from) {
    return this.messages.slice(from);
  }

  mark() {
    return this.messages.length;
  }

  /** 受信済み echo イベントの本文 (marker 指定時は部分一致で絞る)。 */
  echoBodies(marker = null) {
    return this.messages
      .filter((message) => message?.type === "echo" && typeof message.body === "string")
      .map((message) => message.body)
      .filter((body) => marker === null || body.includes(marker));
  }

  close() {
    this.socket.destroy();
  }
}

/** server → client frame (mask なし) を 1 件デコードする。未完なら null。 */
function decodeWsFrame(buffer) {
  if (buffer.length < 2) return null;
  const fin = (buffer[0] & 0x80) !== 0;
  const opcode = buffer[0] & 0x0f;
  let length = buffer[1] & 0x7f;
  let offset = 2;
  if (length === 126) {
    if (buffer.length < 4) return null;
    length = buffer.readUInt16BE(2);
    offset = 4;
  } else if (length === 127) {
    if (buffer.length < 10) return null;
    length = Number(buffer.readBigUInt64BE(2));
    offset = 10;
  }
  if (buffer.length < offset + length) return null;
  return {
    fin,
    opcode,
    payload: buffer.subarray(offset, offset + length).toString("utf8"),
    rest: buffer.subarray(offset + length),
  };
}

/** `from` 以降に条件を満たすメッセージが届くまで待つ。 */
async function waitForEvent(socket, from, predicate, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const hit = socket.slice(from).find(predicate);
    if (hit) return hit;
    await delay(100);
  }
  throw new Error(
    `event not observed; received: ${JSON.stringify(socket.slice(from).map((m) => m?.type))}`,
  );
}

/** 指定 stream の履歴に marker が現れるまで待つ (配信は終端書き込み直後)。 */
async function waitForHistory(stream, marker, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  let last = "";
  while (Date.now() < deadline) {
    const body = await json(
      await request(`/api/history?format=json&stream=${stream}`, auth()),
    );
    last = body.history;
    if (last.includes(marker)) return last;
    await delay(250);
  }
  throw new Error(`history for ${stream} never contained ${marker}: ${last.slice(-400)}`);
}

await check("GET /health/live reports the auth configuration", async () => {
  const response = await request("/health/live");
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.status === "alive", `unexpected status: ${JSON.stringify(body)}`);
  assert(
    body.authentication_required === AUTH_REQUIRED,
    `authentication_required must match NAROU_AUTH_REQUIRED (${AUTH_REQUIRED})`,
  );
  assert(
    body.authentication_configured === true,
    "the deployment must report a usable configuration",
  );
});

await check("GET /health/ready returns 200 (D1 + queue + DO + composition)", async () => {
  const response = await request("/health/ready");
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}: ${JSON.stringify(body)}`);
  assert(body.status === "ready", `unexpected status: ${JSON.stringify(body)}`);
});

await checkWithAuth("GET /api/novels is closed without a token", async () => {
  const response = await request("/api/novels");
  assert(response.status === 401, `status ${response.status}`);
  assert(
    (await errorCode(response)) === "authentication_required",
    "the failure must be machine readable",
  );
});

await checkWithAuth("GET /api/novels rejects a wrong token", async () => {
  const response = await request("/api/novels", auth("definitely-not-the-token"));
  assert(response.status === 401, `status ${response.status}`);
  assert(
    (await errorCode(response)) === "authentication_required",
    "the failure must be machine readable",
  );
});

/** `/api/auth/login` でブラウザ用トークン Cookie を取得する。 */
async function loginCookie() {
  const response = await request("/api/auth/login", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token: TOKEN }),
  });
  assert(response.status === 200, `login status ${response.status}`);
  const setCookie = response.headers.get("set-cookie") ?? "";
  assert(setCookie.includes("narou_api_token="), `missing cookie: ${setCookie}`);
  return setCookie.split(";")[0];
}

await checkWithAuth("POST /api/auth/login rejects a wrong token", async () => {
  const response = await request("/api/auth/login", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token: "definitely-not-the-token" }),
  });
  assert(response.status === 401, `status ${response.status}`);
  assert(
    (await errorCode(response)) === "authentication_required",
    "the failure must be machine readable",
  );
});

await checkWithAuth("POST /api/auth/login rejects a cross-origin request", async () => {
  const response = await request("/api/auth/login", {
    method: "POST",
    headers: { "content-type": "application/json", origin: "https://evil.example" },
    body: JSON.stringify({ token: TOKEN }),
  });
  assert(response.status === 403, `status ${response.status}`);
  assert((await errorCode(response)) === "cross_origin_rejected", "machine readable code");
});

await checkWithAuth("the login cookie authenticates later requests", async () => {
  const response = await request("/api/auth/login", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token: TOKEN }),
  });
  const setCookie = response.headers.get("set-cookie") ?? "";
  assert(setCookie.toLowerCase().includes("httponly"), `cookie must be HttpOnly: ${setCookie}`);
  assert(
    setCookie.toLowerCase().includes("samesite=lax"),
    `cookie must be SameSite=Lax: ${setCookie}`,
  );

  const cookie = setCookie.split(";")[0];
  const list = await request("/api/novels?limit=1", { headers: { cookie } });
  assert(list.status === 200, `cookie-authenticated status ${list.status}`);
});

await checkWithAuth("a cookie request from another origin is rejected", async () => {
  const cookie = await loginCookie();
  const response = await request("/api/novels?limit=1", {
    headers: { cookie, origin: "https://evil.example" },
  });
  assert(response.status === 401, `status ${response.status}`);
});

await checkWithAuth("a same-origin cookie request is accepted", async () => {
  const cookie = await loginCookie();
  const response = await request("/api/novels?limit=1", {
    headers: { cookie, origin: new URL(BASE_URL).origin },
  });
  assert(response.status === 200, `status ${response.status}`);
});

await checkWithAuth("GET /login serves the token login page", async () => {
  const response = await request("/login", { redirect: "manual" });
  assert(response.status === 200, `status ${response.status}`);
  const html = await response.text();
  assert(html.includes("/api/auth/login"), "the page must post to the login endpoint");
});

await checkWithAuth("GET /login returns an authenticated browser to the UI", async () => {
  const cookie = await loginCookie();
  const response = await request("/login?return=/settings", {
    headers: { cookie },
    redirect: "manual",
  });
  assert(
    [301, 302, 303, 307, 308].includes(response.status),
    `status ${response.status}`,
  );
  const location = response.headers.get("location") ?? "";
  assert(location.endsWith("/settings"), `unexpected location: ${location}`);
});

await check("GET /api/novels returns the paged shape", async () => {
  const response = await request("/api/novels?limit=5", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(Array.isArray(body.data), "data must be an array");
  assert(typeof body.records_total === "number", "records_total must be a number");
  assert(typeof body.records_filtered === "number", "records_filtered must be a number");
});

await check("POST /api/novels is not allowed", async () => {
  const response = await request("/api/novels", { method: "POST", ...auth() });
  assert(response.status === 405, `status ${response.status}`);
});

await check("GET /api/novels/:id returns 404 for an unknown novel", async () => {
  const response = await request("/api/novels/999999999", auth());
  assert(response.status === 404, `status ${response.status}`);
});

await check("GET /api/novels/:id/download.epub returns 404 for an unknown novel", async () => {
  const response = await request("/api/novels/999999999/download.epub", auth());
  assert(response.status === 404, `status ${response.status}`);
});

await check("HEAD preserves text and JSON error metadata without a body", async () => {
  for (const [path, status, contentType] of [
    ["/api/novels/999999999/download.epub", 404, "text/plain; charset=utf-8"],
    ["/api/novels/not-a-number", 400, "application/json"],
  ]) {
    const get = await request(path, auth());
    assert.equal(get.status, status);
    assert.equal(get.headers.get("content-type"), contentType);
    await get.arrayBuffer();
    const head = await request(path, { method: "HEAD", ...auth() });
    assert.equal(head.status, status);
    assert.equal(head.headers.get("content-type"), contentType);
    assert.equal((await head.arrayBuffer()).byteLength, 0);
  }
});

await check("GET /api/jobs/:id returns 404 for an unknown job", async () => {
  const response = await request("/api/jobs/does-not-exist", auth());
  assert(response.status === 404, `status ${response.status}`);
});

await check("POST /api/download rejects an unknown target", async () => {
  // native の api_download はリクエスト単位の失敗も HTTP 200 + success:false で返す。
  const response = await request("/api/download", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ targets: ["not-a-novel-target"] }),
  });
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(body.success === false, `unexpected body: ${JSON.stringify(body).slice(0, 200)}`);
  assert(Array.isArray(body.results) && body.results.length === 0, "results must be empty");
  assert(
    typeof body.message === "string" && body.message.length > 0,
    "the failure must explain itself",
  );
});

await check("POST /api/download rejects the unsupported mail option", async () => {
  // --mail の消費者は Worker に無い。成功に見せず 501 + 機械可読コードで拒否する。
  const response = await request("/api/download", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ targets: ["999999999"], mail: true }),
  });
  assert(response.status === 501, `status ${response.status}`);
  assert(
    (await errorCode(response)) === "mail_not_supported_on_worker",
    "the refusal must be machine readable",
  );
});

await check("GET /api/notepad/read returns the notepad shape", async () => {
  const response = await request("/api/notepad/read", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(typeof body.content === "string", "content must be a string");
  assert(typeof body.text === "string", "text must be a string");
  assert("object_id" in body, "object_id must be present");
});

await check("POST /api/notepad/save round-trips the notepad", async () => {
  // native は object_id (本文の SHA-256) で楽観ロックする。読み出した値をそのまま返す。
  const before = await json(await request("/api/notepad/read", auth()));
  const saved = await request("/api/notepad/save", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ content: "contract notepad", object_id: before.object_id }),
  });
  assert(saved.status === 200, `status ${saved.status}`);
  const result = await json(saved);
  assert(result.success === true, `saving must succeed: ${JSON.stringify(result).slice(0, 200)}`);
  const read = await json(await request("/api/notepad/read", auth()));
  assert(read.content === "contract notepad", `unexpected content: ${read.content}`);
  assert(read.object_id === result.object_id, "the saved object_id must come back");
});

await checkLocal("competing notepad saves cannot overwrite the winner", async () => {
  const before = await json(await request("/api/notepad/read", auth()));
  const save = (content, objectId) => request("/api/notepad/save", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ content, object_id: objectId }),
  }).then(json);
  const contents = ["contract competing save A", "contract competing save B"];
  try {
    const results = await Promise.all(contents.map(content => save(content, before.object_id)));
    const winner = results.findIndex(result => result.success === true);
    assert.notEqual(winner, -1, "one save must succeed");
    assert.equal(results.filter(result => result.success === true).length, 1);
    const loser = results[1 - winner];
    assert.equal(loser.conflict, true);
    assert.equal(loser.content, contents[winner]);
    assert.equal(loser.object_id, results[winner].object_id);
    const current = await json(await request("/api/notepad/read", auth()));
    assert.equal(current.content, contents[winner]);
    assert.equal(current.object_id, results[winner].object_id);
  } finally {
    const current = await json(await request("/api/notepad/read", auth()));
    const restored = await save(before.content, current.object_id);
    assert.equal(restored.success, true, "restore the original local notepad");
  }
});

await check("GET /api/story returns 404 for an unknown novel", async () => {
  const response = await request("/api/story?id=999999999", auth());
  assert(response.status === 404, `status ${response.status}`);
});

await check("GET /api/taginfo.json returns an array", async () => {
  const response = await request("/api/taginfo.json", auth());
  assert(response.status === 200, `status ${response.status}`);
  assert(Array.isArray(await json(response)), "taginfo must be an array");
});

await check("GET /api/history returns the JSON history shape", async () => {
  const response = await request("/api/history?format=json", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(typeof body.history === "string", `history must be a string: ${JSON.stringify(body)}`);
});

await check("GET /api/version/current.json reports the version", async () => {
  const response = await request("/api/version/current.json", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(typeof body.version === "string" && body.version.length > 0, "version must be set");
});

await check("POST /api/inspect is refused on the worker", async () => {
  // 調査ログはローカルファイル前提なので、成功を偽装せず 501 を返す。
  const response = await request("/api/inspect", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ targets: ["999999999"] }),
  });
  assert(response.status === 501, `status ${response.status}`);
});

await check("POST /api/convert rejects an invalid target", async () => {
  const response = await request("/api/convert", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ targets: ["--not-a-target"] }),
  });
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(body.success === false, `unexpected body: ${JSON.stringify(body).slice(0, 200)}`);
  assert(Array.isArray(body.results) && body.results.length === 0, "results must be empty");
});

await check("POST /api/convert rejects non-EPUB devices while EPUB is accepted", async () => {
  // Worker の出力は組み込み EPUB だけ。native 専用端末 (kindle 等) を
  // 受理して共通出力を返すと、指定どおりの形式を黙って落とすことになる。
  const rejected = await request("/api/convert", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ targets: ["999999999"], device: "kindle" }),
  });
  assert(rejected.status === 200, `status ${rejected.status}`);
  const rejectedBody = await json(rejected);
  assert(rejectedBody.success === false, `kindle must be refused: ${JSON.stringify(rejectedBody).slice(0, 200)}`);
  assert(
    String(rejectedBody.message).includes("kindle") && String(rejectedBody.message).includes("EPUB"),
    `the refusal must explain the supported format: ${JSON.stringify(rejectedBody)}`,
  );
  assert(
    Array.isArray(rejectedBody.results) && rejectedBody.results.length === 0,
    "a refused plan must not queue anything",
  );

  const accepted = await request("/api/convert", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ targets: ["999999999"], device: "epub" }),
  });
  assert(accepted.status === 200, `status ${accepted.status}`);
  const acceptedBody = await json(accepted);
  assert(acceptedBody.success === true, `epub must be accepted: ${JSON.stringify(acceptedBody).slice(0, 200)}`);
  assert(acceptedBody.results.length === 1, `expected one result: ${JSON.stringify(acceptedBody)}`);
  assert.equal(acceptedBody.results[0].status, "queued");
  assert.equal(acceptedBody.results[0].device, "epub");
});

await check("POST /api/update rejects an invalid target", async () => {
  const response = await request("/api/update", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ targets: ["--not-a-target"] }),
  });
  assert(response.status === 200, `status ${response.status}`);
  assert((await json(response)).success === false, "an invalid target must be rejected");
});

await check("POST /api/update_general_lastup is refused on the worker", async () => {
  // 全 TOC の再取得が要る操作で、Worker の JobKind::Update では表現できない。
  const response = await request("/api/update_general_lastup", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ option: "all" }),
  });
  assert(response.status === 501, `status ${response.status}`);
  assert(
    (await errorCode(response)) === "not_supported_on_worker",
    "the refusal must be machine readable",
  );
});

await check("POST /api/update/start is refused on the worker", async () => {
  const response = await request("/api/update/start", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: "{}",
  });
  assert(response.status === 501, `status ${response.status}`);
});

await check("POST /api/login/import rejects an empty envelope", async () => {
  const response = await request("/api/login/import", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ envelope: "" }),
  });
  assert(response.status === 200, `status ${response.status}`);
  assert((await json(response)).success === false, "an empty import must be refused");
});

await check("POST /api/login/order rejects a mismatched order", async () => {
  const response = await request("/api/login/order", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ site: "not-a-site.example", order: [0, 1] }),
  });
  assert(response.status === 200, `status ${response.status}`);
  // サイト無し = 保存 0 件に対して [0, 1] は件数不一致。
  assert((await json(response)).success === false, "a mismatched order must be refused");
});

await check("POST /api/queue/clear reports success", async () => {
  const response = await request("/api/queue/clear", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: "{}",
  });
  assert(response.status === 200, `status ${response.status}`);
  assert((await json(response)).success === true, "clearing an empty queue must succeed");
});

await check("POST /api/cancel reports success", async () => {
  const response = await request("/api/cancel", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({}),
  });
  assert(response.status === 200, `status ${response.status}`);
  assert((await json(response)).success === true, "cancelling with nothing running must succeed");
});

await check("POST /api/reorder_pending_tasks is refused on the worker", async () => {
  // キュー配送順は送信時に固定されるため、表示順だけを偽装しない。
  const response = await request("/api/reorder_pending_tasks", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ task_ids: [] }),
  });
  assert(response.status === 501, `status ${response.status}`);
  assert(
    (await errorCode(response)) === "queue_reorder_not_supported",
    "the refusal must be machine readable",
  );
});

await check("POST /api/tag/change_color rejects an unknown color", async () => {
  const response = await request("/api/tag/change_color", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ tag: "contract-tag", color: "not-a-color" }),
  });
  assert(response.status === 200, `status ${response.status}`);
  assert((await json(response)).success === false, "an unknown color must be rejected");
});

await check("POST /api/tag/change_color stores a color", async () => {
  const response = await request("/api/tag/change_color", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ tag: "contract-tag", color: "green" }),
  });
  assert(response.status === 200, `status ${response.status}`);
  assert((await json(response)).success === true, "the color must be stored");
});

await check("POST /api/edit_tag requires states", async () => {
  // native の EditTagBody は `states` 必須（欠けると axum の JSON 拒否 = 400）。
  const response = await request("/api/edit_tag", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ ids: [] }),
  });
  assert(response.status === 400, `status ${response.status}`);
});

await check("POST /api/edit_tag reports no valid ids at 200", async () => {
  // native は「対象なし」を 200 + {success:false,error} で返す（HTTP エラーにしない）。
  const response = await request("/api/edit_tag", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ ids: [], states: {} }),
  });
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(body.success === false, "an empty edit must be reported as such");
  assert(typeof body.error === "string", `the reason must be set: ${JSON.stringify(body)}`);
});

await check("GET /api/feature_tour/all lists every tour", async () => {
  const response = await request("/api/feature_tour/all", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(body.success === true, `unexpected body: ${JSON.stringify(body).slice(0, 200)}`);
  assert(Array.isArray(body.entries), "entries must be an array");
  assert(
    typeof body.latest_pending_version === "string",
    "latest_pending_version must be a string",
  );
});

await check("POST /api/feature_tour/seen rejects an unknown version", async () => {
  const response = await request("/api/feature_tour/seen", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ version: "0.0.0-not-a-tour" }),
  });
  assert(response.status === 200, `status ${response.status}`);
  assert((await json(response)).success === false, "an unknown version must be rejected");
});

await check("POST /api/feature_tour/config is reflected by pending", async () => {
  const response = await request("/api/feature_tour/config", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ disabled: false }),
  });
  assert(response.status === 200, `status ${response.status}`);
  assert((await json(response)).success === true, "the config save must succeed");
  const pending = await json(await request("/api/feature_tour/pending", auth()));
  assert(pending.disabled === false, "pending must report the saved config");
});

// Worker では実現できない操作は、成功を偽装せず 501 + 機械可読なコードで断る。
for (const path of [
  "/api/shutdown",
  "/api/reboot",
  "/api/folder",
  "/api/backup",
  "/api/backup_bookmark",
  "/api/setting_burn",
  "/api/csv/import",
  "/api/mail",
  "/api/send",
]) {
  await check(`POST ${path} is refused on the worker`, async () => {
    const response = await request(path, {
      method: "POST",
      headers: { "content-type": "application/json", ...auth().headers },
      body: "{}",
    });
    assert(response.status === 501, `status ${response.status}`);
    assert(
      (await errorCode(response)) === "not_supported_on_worker",
      "the refusal must be machine readable",
    );
  });
}

await check("GET /api/csv/download is refused on the worker", async () => {
  const response = await request("/api/csv/download", auth());
  assert(response.status === 501, `status ${response.status}`);
});

await check("GET /api/storage/mode reports the worker storage", async () => {
  const response = await request("/api/storage/mode", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(body.success === true, `unexpected body: ${JSON.stringify(body)}`);
  assert(body.mode === "sqlite", "metadata mode must remain sqlite");
  assert(body.metadata_backend === "d1", "metadata backend must be D1");
  assert(["d1", "s3"].includes(body.illustration_backend), "illustration backend must be explicit");
  if (process.env.NAROU_REQUIRE_S3 === "true") {
    assert(body.s3_required === true, "deployed runtime must enforce S3");
    assert(body.illustration_backend === "s3", "deployed illustrations must use S3");
    const probe = await request("/api/storage/mode?probe=s3", auth());
    assert(probe.status === 200, `S3 LIST probe status ${probe.status}`);
    const result = await json(probe);
    assert(result.success === true && result.s3_list === "ok", "S3 LIST must succeed");
  }
});

await check("GET /api/webui/config returns the UI configuration", async () => {
  const response = await request("/api/webui/config", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(typeof body.theme === "string" && body.theme.length > 0, "theme must be a string");
  assert(typeof body.performance_mode === "string", "performance_mode must be a string");
  assert(typeof body.reload_timing === "string", "reload_timing must be a string");
  assert(typeof body.debug_mode === "boolean", "debug_mode must be a boolean");
  assert(
    typeof body.ws_port === "number" && typeof body.port === "number",
    "ports must be numbers",
  );
  assert(typeof body.concurrency_enabled === "boolean", "concurrency_enabled must be a boolean");
});

await check("POST /api/global_setting refreshes the fresh config and broadcasts a reload", async () => {
  // 表示設定 (theme / concurrency) の保存は: (1) キャッシュを迂回した
  // `?fresh=1` の設定に反映され、(2) `webui.config.reload` を購読者へ送る。
  const view = await fetchSettingsView();
  const valueOf = (name) => view.settings.find((item) => item.name === name)?.value ?? null;
  const themeBefore = valueOf("webui.theme");
  const concurrencyBefore = valueOf("concurrency");
  const themeNext = themeBefore === "Cerulean" ? "Superhero" : "Cerulean";
  const concurrencyNext = !concurrencyBefore;

  const socket = await openWebSocket();
  try {
    const mark = socket.mark();
    const saved = await postSettings({
      settings: { "webui.theme": themeNext, concurrency: concurrencyNext },
      expected_settings: { "webui.theme": themeBefore, concurrency: concurrencyBefore },
    });
    assert.equal(saved.body.success, true, `display save must succeed: ${JSON.stringify(saved.body)}`);
    await waitForEvent(socket, mark, (message) => message?.type === "webui.config.reload");

    const fresh = await json(await request("/api/webui/config?fresh=1", auth()));
    assert.equal(fresh.theme, themeNext, "the fresh config must reflect the saved theme");
    assert.equal(
      fresh.concurrency_enabled,
      concurrencyNext,
      "the fresh config must reflect the saved concurrency",
    );
  } finally {
    socket.close();
    await postSettings({
      settings: { "webui.theme": themeBefore, concurrency: concurrencyBefore },
      expected_settings: { "webui.theme": themeNext, concurrency: concurrencyNext },
    });
  }
});

await check("GET /api/sort_state returns the current sort", async () => {
  const response = await request("/api/sort_state", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(typeof body.column === "number", `column must be a number: ${JSON.stringify(body)}`);
  assert(["asc", "desc"].includes(body.dir), `dir must be asc or desc: ${JSON.stringify(body)}`);
});

await check("POST /api/sort_state rejects an invalid column", async () => {
  const response = await request("/api/sort_state", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ column: "not-a-number", dir: "sideways" }),
  });
  assert(response.status === 200, `status ${response.status}`);
  assert((await json(response)).success === false, "an invalid sort must be rejected");
});

await check("GET /api/tag_list?format=json returns tags and colors", async () => {
  const response = await request("/api/tag_list?format=json", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(Array.isArray(body.tags), "tags must be an array");
  assert(body.tag_colors && typeof body.tag_colors === "object", "tag_colors must be an object");
});

await check("GET /api/queue/status returns the queue counters", async () => {
  const response = await request("/api/queue/status", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  for (const key of ["pending", "completed", "partial", "failed", "cancelled", "running_count"]) {
    assert(typeof body[key] === "number", `${key} must be a number: ${JSON.stringify(body)}`);
  }
  assert(
    body.running === null || typeof body.running === "string",
    "running must be null or a label",
  );
  assert(body.lane_sizes && typeof body.lane_sizes === "object", "lane_sizes must be an object");
});

await check("GET /api/get_pending_tasks returns pending and running lists", async () => {
  const response = await request("/api/get_pending_tasks", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(Array.isArray(body.pending) && Array.isArray(body.running), "lists must be arrays");
  assert(
    typeof body.pending_count === "number" && typeof body.running_count === "number",
    "counts must be numbers",
  );
});

await check("GET /api/feature_tour/pending returns the tour state", async () => {
  const response = await request("/api/feature_tour/pending", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(body.success === true, `unexpected body: ${JSON.stringify(body).slice(0, 200)}`);
  assert(Array.isArray(body.entries), "entries must be an array");
  assert(typeof body.current_version === "string", "current_version must be a string");
});

await check("GET /api/list?all=true returns the library shape", async () => {
  const response = await request("/api/list?all=true", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  // native の NovelListResponse (`src/web/state.rs`) と同じ snake_case キー。
  for (const key of ["draw", "records_total", "records_filtered", "data"]) {
    assert(key in body, `missing key ${key}: ${JSON.stringify(body).slice(0, 200)}`);
  }
  assert(Array.isArray(body.data), "data must be an array");
  assert(typeof body.records_total === "number", "records_total must be a number");
  for (const item of body.data) {
    for (const key of ["id", "title", "tags", "frozen", "suspend", "toc_url"]) {
      assert(key in item, `item missing ${key}: ${JSON.stringify(item).slice(0, 200)}`);
    }
    assert(Array.isArray(item.tags), "tags must be an array");
  }
});

await check("GET /api/list rejects an over-long search", async () => {
  const response = await request(`/api/list?filter=${"a".repeat(5000)}`, auth());
  assert(response.status === 400, `status ${response.status}`);
  assert((await errorCode(response)) === "invalid_request", "the failure must be machine readable");
});

await check("GET /api/library_backup reports no pending offer", async () => {
  const response = await request("/api/library_backup", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(body.pending === false, `pending must be false on the worker: ${JSON.stringify(body)}`);
  assert(typeof body.running === "boolean", "running must be a boolean");
});

await check("POST /api/library_backup is not supported on the worker", async () => {
  // ローカル FS に zip を書けないので、成功を偽装せず 501 を返す。
  const response = await request("/api/library_backup", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ action: "create" }),
  });
  assert(response.status === 501, `status ${response.status}`);
  assert(
    (await errorCode(response)) === "library_backup_not_supported",
    "the failure must be machine readable",
  );
});

await check("GET /api/global_setting returns the settings page payload", async () => {
  const response = await request("/api/global_setting", auth());
  assert(response.status === 200, `status ${response.status}`);
  const body = await json(response);
  assert(body.error === undefined, `unexpected error: ${body.error}`);
  assert(Array.isArray(body.tabs) && body.tabs.length > 0, "tabs must be present");
  assert(Array.isArray(body.settings) && body.settings.length > 0, "settings must be listed");
  assert(
    body.settings.every((item) => typeof item.name === "string" && typeof item.tab === "string"),
    "every setting needs a name and a tab",
  );
});

await check("POST /api/global_setting rejects a stale expected value without overwriting", async () => {
  // 最初の保存は「D1 に行が無い」スコープでも通る (画面が見た null が expected)。
  // 続く古い画面からの保存は conflict で弾かれ、先の勝者を上書きしない。
  // local スコープ (concurrency / webui.theme) と global スコープ
  // (convert.epub-font) を同時に変え、どちらの行にも同じ CAS が効くことを見る。
  const before = settingValues(await fetchSettingsView());
  const themeBefore = before.get("webui.theme") ?? null;
  const concurrencyBefore = before.get("concurrency") ?? null;
  const fontSizeBefore = before.get("convert.epub-font") ?? null;
  // 再実行でも「最初の保存が必ず値を変える」ように交互の値を選ぶ。
  const themeWinner = themeBefore === "Darkly" ? "Slate" : "Darkly";
  const concurrencyWinner = !concurrencyBefore;
  const fontSizeWinner = fontSizeBefore === "always" ? "auto" : "always";

  try {
  const first = await postSettings({
    settings: {
      "webui.theme": themeWinner,
      concurrency: concurrencyWinner,
      "convert.epub-font": fontSizeWinner,
    },
    expected_settings: {
      "webui.theme": themeBefore,
      concurrency: concurrencyBefore,
      "convert.epub-font": fontSizeBefore,
    },
  });
  assert.equal(first.status, 200, `status ${first.status}`);
  assert.equal(first.body.success, true, `first save must succeed: ${JSON.stringify(first.body)}`);

  const stale = await postSettings({
    settings: {
      "webui.theme": themeBefore ?? "Readable",
      concurrency: concurrencyBefore ?? true,
      "convert.epub-font": fontSizeBefore ?? "auto",
    },
    expected_settings: {
      "webui.theme": themeBefore,
      concurrency: concurrencyBefore,
      "convert.epub-font": fontSizeBefore,
    },
  });
  // native と同じ流儀: 競合は HTTP 409。Worker ハンドラの 409 化に合わせる
  // (それ以前の 200 + success:false ビルドではこの検査は落ちる)。
  assert.equal(stale.status, 409, `status ${stale.status}`);
  assert.equal(stale.body.success, false, `a stale save must not succeed: ${JSON.stringify(stale.body)}`);
  assert.equal(stale.body.conflict, true, `the conflict must be machine readable: ${JSON.stringify(stale.body)}`);

  const after = settingValues(await fetchSettingsView());
  assert.equal(after.get("webui.theme"), themeWinner, "the stale save must not overwrite the theme");
  assert.equal(after.get("concurrency"), concurrencyWinner, "the stale save must not overwrite concurrency");
  assert.equal(after.get("convert.epub-font"), fontSizeWinner, "the stale save must not overwrite the global scope");
  } finally {
    await postSettings({
      settings: { "webui.theme": themeBefore, concurrency: concurrencyBefore, "convert.epub-font": fontSizeBefore },
      expected_settings: { "webui.theme": themeWinner, concurrency: concurrencyWinner, "convert.epub-font": fontSizeWinner },
    });
  }
});

await check("POST /api/global_setting writes only the dirty keys", async () => {
  // 未変更の設定 (未設定 = nil を含む) は保存で書き換わらない。W1 の
  // 「触っていない nil の boolean が false に化ける」回帰を固定する。
  const before = settingValues(await fetchSettingsView());
  const themeBefore = before.get("webui.theme") ?? null;
  const concurrencyBefore = before.get("concurrency") ?? null;
  const themeNext = themeBefore === "United" ? "Readable" : "United";
  const concurrencyNext = !concurrencyBefore;

  try {
  const saved = await postSettings({
    settings: { "webui.theme": themeNext, concurrency: concurrencyNext },
    expected_settings: { "webui.theme": themeBefore, concurrency: concurrencyBefore },
  });
  assert.equal(saved.body.success, true, `dirty save must succeed: ${JSON.stringify(saved.body)}`);

  const after = settingValues(await fetchSettingsView());
  for (const [name, value] of before) {
    if (name === "webui.theme" || name === "concurrency") {
      continue;
    }
    assert.deepEqual(
      after.get(name),
      value,
      `${name} must not change when it is not in the request (was ${JSON.stringify(value)})`,
    );
  }
  } finally {
    await postSettings({
      settings: { "webui.theme": themeBefore, concurrency: concurrencyBefore },
      expected_settings: { "webui.theme": themeNext, concurrency: concurrencyNext },
    });
  }
});

await check("POST /api/global_setting rejects settings the worker cannot honor", async () => {
  const view = await fetchSettingsView();
  const ineffective = view.settings.filter((item) => item.worker_ineffective === true);
  assert(ineffective.length > 0, "the worker must mark settings that have no execution path");
  const logging = view.settings.find((item) => item.name === "logging");
  assert.equal(logging?.worker_ineffective, true, "logging must be marked worker_ineffective");
  const target = ineffective[0];
  const themeName = "webui.theme";
  const themeBefore = view.settings.find((item) => item.name === themeName)?.value ?? null;

  const rejected = await postSettings({
    settings: { [target.name]: "1", [themeName]: "Darkly" },
  });
  assert.equal(rejected.body.success, false, `an ineffective setting must be refused: ${JSON.stringify(rejected.body)}`);
  assert(
    String(rejected.body.message).includes("変更できません"),
    `the refusal must be machine readable in the message: ${JSON.stringify(rejected.body)}`,
  );
  assert(
    String(rejected.body.message).includes(target.name),
    `the refusal must name the setting: ${JSON.stringify(rejected.body)}`,
  );

  // 配備上限 (Cloudflare Queues の max_retries=3) を超える再試行回数も拒否。
  const tooManyRetries = await postSettings({ settings: { "queue.max-retries": 10 } });
  assert.equal(tooManyRetries.body.success, false, `max-retries>3 must be refused: ${JSON.stringify(tooManyRetries.body)}`);
  assert(
    String(tooManyRetries.body.message).includes("最大 3"),
    `the refusal must state the cap: ${JSON.stringify(tooManyRetries.body)}`,
  );

  const after = await fetchSettingsView();
  const valueOf = (name) => after.settings.find((item) => item.name === name)?.value;
  assert.deepEqual(valueOf(target.name), target.value, "a rejected setting must keep its stored value");
  assert.notEqual(valueOf("queue.max-retries"), 10, "the rejected retry count must not be stored");
  assert.equal(valueOf(themeName), themeBefore, "a rejected request must not write its other settings");
});

await check("POST /api/global_setting rejects a stale replace_content without writing settings", async () => {
  const view = await fetchSettingsView();
  const replaceBefore = view.replace_content ?? "";
  // 再実行でも「最初の保存が必ず内容を変える」ように交互の値を使う。
  const firstValue = replaceBefore === "contract-replace-alpha" ? "contract-replace-beta" : "contract-replace-alpha";
  const staleValue = firstValue === "contract-replace-alpha" ? "contract-replace-beta" : "contract-replace-alpha";

  // 初回は (行が無い状態の) 期待値そのままで保存が通る。
  const first = await postSettings({
    settings: {},
    replace_content: firstValue,
    expected_replace_content: replaceBefore,
  });
  assert.equal(first.body.success, true, `the first replace save must succeed: ${JSON.stringify(first.body)}`);

  // 古い画面: settings の変更も同梱しているが、置換の競合で全体が弾かれる。
  const victim = "webui.table.reload-timing";
  const victimBefore = view.settings.find((item) => item.name === victim)?.value ?? null;
  const victimNext = victimBefore === "queue" ? "every" : "queue";
  const stale = await postSettings({
    settings: { [victim]: victimNext },
    expected_settings: { [victim]: victimBefore },
    replace_content: staleValue,
    expected_replace_content: replaceBefore,
  });
  // native と同じ流儀: 競合は HTTP 409 (Worker ハンドラの 409 化に合わせる)。
  assert.equal(stale.status, 409, `status ${stale.status}`);
  assert.equal(stale.body.success, false, `a stale replace must not succeed: ${JSON.stringify(stale.body)}`);
  assert.equal(stale.body.conflict, true, `the replace conflict must be machine readable: ${JSON.stringify(stale.body)}`);

  const after = await fetchSettingsView();
  assert.equal(after.replace_content, firstValue, "the stale replace must not overwrite the winner");
  assert.equal(
    after.settings.find((item) => item.name === victim)?.value,
    victimBefore,
    "the rejected request must not write its settings either",
  );

  // 後始末: 元の内容へ戻す。
  const restored = await postSettings({
    settings: {},
    replace_content: replaceBefore,
    expected_replace_content: firstValue,
  });
  assert.equal(restored.body.success, true, `restoring replace_content must succeed: ${JSON.stringify(restored.body)}`);
});

await check("POST /api/global_setting stores a value", async () => {
  const post = await request("/api/global_setting", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ settings: { "webui.theme": "Darkly" } }),
  });
  assert(post.status === 200, `status ${post.status}`);
  assert((await json(post)).success === true, "the save must succeed");

  const after = await json(await request("/api/global_setting", auth()));
  const theme = after.settings.find((item) => item.name === "webui.theme");
  assert(theme, "webui.theme must be listed after saving");
  assert.strictEqual(theme.value, "Darkly");
});

await checkWithAuth("POST /api/jobs requires auth", async () => {
  const response = await request("/api/jobs", { method: "POST", body: "{}" });
  assert(response.status === 401, `status ${response.status}`);
  assert((await errorCode(response)) === "authentication_required", "code must be set");
});

await check("POST /api/jobs rejects a malformed body", async () => {
  const response = await request("/api/jobs", {
    method: "POST",
    ...auth(),
    body: "{not json",
  });
  assert(response.status === 400, `status ${response.status}`);
});

await check("POST /api/jobs rejects an unknown kind", async () => {
  const response = await request("/api/jobs", {
    method: "POST",
    ...auth(),
    body: JSON.stringify({ kind: "Nope", targets: ["1"], options: [] }),
  });
  assert(response.status === 400, `status ${response.status}`);
});

let convertJobId = null;

await check("GET / serves the Web UI with versioned assets", async () => {
  const response = await request("/");
  assert(response.status === 200, `status ${response.status}`);
  const html = await response.text();
  assert(
    html.includes("__NAROU_RS_WEBUI_BUILD__"),
    "the build script must be injected into the page",
  );
  assert(
    /main\.js\?v=[0-9a-f]{16}/.test(html),
    "asset references must carry a content hash",
  );
});

await check("GET /assets/<file>?v=<hash> serves the static asset", async () => {
  const html = await (await request("/")).text();
  const match = html.match(/\/assets\/([^"'?]*main\.js)\?v=([0-9a-f]{16})/);
  assert(match, `the page should reference a versioned main.js: ${html.slice(0, 200)}`);
  const response = await request(`/assets/${match[1]}?v=${match[2]}`);
  assert(response.status === 200, `status ${response.status}`);
  const type = response.headers.get("content-type") ?? "";
  assert(type.includes("javascript"), `unexpected content type: ${type}`);
});

await checkLocal("the scheduled handler runs (cron planner)", async () => {
  // `wrangler dev --test-scheduled` のテスト用エンドポイントは wrangler の
  // バージョンで名前が変わるので両方試す。
  const paths = ["/__scheduled?cron=*+*+*+*+*", "/cdn-cgi/handler/scheduled?cron=*+*+*+*+*"];
  const statuses = [];
  for (const path of paths) {
    const response = await request(path);
    statuses.push(`${path} -> ${response.status}`);
    if (response.status === 200) return;
  }
  throw new Error(`scheduled endpoint not reachable: ${statuses.join(", ")}`);
});

await checkWithAuth("GET /api/novels/:id/illustrations/:name requires auth", async () => {
  const response = await request("/api/novels/1/illustrations/0001.jpg");
  assert(response.status === 401, `status ${response.status}`);
  assert((await errorCode(response)) === "authentication_required", "code must be set");
});

await check("GET /api/novels/:id/illustrations/:name is 404 for an unknown novel", async () => {
  const response = await request("/api/novels/999999999/illustrations/0001.jpg", auth());
  assert(response.status === 404, `status ${response.status}`);
});

await check("POST /api/jobs accepts a Convert plan", async () => {
  const response = await request("/api/jobs", {
    method: "POST",
    ...auth(),
    body: JSON.stringify({ kind: "Convert", targets: ["999999999"], options: [] }),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(Array.isArray(body.ids), "ids must be an array");
  assert(Array.isArray(body.blocked), "blocked must be an array");
  assert(body.ids.length === 1, `expected one accepted job: ${JSON.stringify(body)}`);
  assert(body.blocked.length === 0, `Convert must not be blocked: ${JSON.stringify(body)}`);
  convertJobId = body.ids[0];
});

await check("a Convert plan for a missing novel is not reported as blocked", async () => {
  const response = await request("/api/jobs", {
    method: "POST",
    ...auth(),
    body: JSON.stringify({ kind: "Send", targets: ["1"], options: [] }),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.blocked.length === 1, `Send must be blocked: ${JSON.stringify(body)}`);
});

const SITE_YAML = [
  "name: Contract Test",
  "domain: contract-test.example",
  "top_url: https://contract-test.example",
  "sitename: Contract Test",
  "toc_url: https://contract-test.example/\\k<url>",
  "",
].join("\n");

await check("GET /api/sites lists bundled definitions", async () => {
  const response = await request("/api/sites", auth());
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  const sites = body.data?.sites ?? [];
  assert(sites.length > 0, "bundled sites must be listed");
  assert(
    sites.some((site) => site.origin === "bundled"),
    `expected a bundled entry: ${JSON.stringify(sites.slice(0, 3))}`,
  );
});

await checkWithAuth("GET /api/sites is closed without a token", async () => {
  const response = await request("/api/sites");
  assert(response.status === 401, `status ${response.status}`);
  assert((await errorCode(response)) === "authentication_required", "code must be set");
});

await check("PUT /api/sites/{name} rejects an invalid definition", async () => {
  const response = await request("/api/sites/contract-test", {
    method: "PUT",
    ...auth(),
    body: "name: Broken\n",
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.success === false, `unexpected body: ${JSON.stringify(body)}`);
});

await check("GET /api/sites/{name} returns the effective definition", async () => {
  const response = await request("/api/sites/contract-test", auth());
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.success === false, `unknown name must fail: ${JSON.stringify(body)}`);
});

await check("PUT /api/sites/{name} stores a user definition", async () => {
  const response = await request("/api/sites/contract-test", {
    method: "PUT",
    ...auth(),
    body: SITE_YAML,
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.success === true, `unexpected body: ${JSON.stringify(body)}`);
  const entry = (body.data?.sites ?? []).find(
    (site) => site.name === "contract-test.yaml",
  );
  assert(entry, `stored definition must be listed: ${JSON.stringify(body.data)}`);
  assert(entry.origin === "user", `origin must be user: ${JSON.stringify(entry)}`);

  // 実効定義として本文が返り、bundle の定義も読める。
  const stored = await request("/api/sites/contract-test.yaml", auth());
  const storedBody = await json(stored);
  assert(storedBody.success === true, `unexpected body: ${JSON.stringify(storedBody)}`);
  assert(storedBody.data.origin === "user", "the override must win");
  assert(storedBody.data.yaml.includes("contract-test.example"), "the body must round-trip");

  const bundled = await request("/api/sites/ncode.syosetu.com.yaml", auth());
  const bundledBody = await json(bundled);
  assert(bundledBody.success === true, `bundled lookup failed: ${JSON.stringify(bundledBody)}`);
  assert(bundledBody.data.origin === "bundled", "an untouched site stays bundled");

  // ユーザー定義を足しても構成は壊れない (readiness は 200 のまま)。
  const ready = await request("/health/ready");
  assert(ready.status === 200, `readiness regressed: ${ready.status}`);
});

await check("DELETE /api/sites/{name} reverts to the bundled set", async () => {
  const response = await request("/api/sites/contract-test", {
    method: "DELETE",
    ...auth(),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(
    !(body.data?.sites ?? []).some((site) => site.name === "contract-test.yaml"),
    `the override must be gone: ${JSON.stringify(body.data)}`,
  );
  const gone = await request("/api/sites/contract-test.yaml", auth());
  assert((await json(gone)).success === false, "the override must not resolve");
});

// ─── ログイン (サイト単位の LoginGroup。native `src/web/login.rs` と同じ形) ──
//
// Cookie の直接登録 (旧 `POST /api/login/set` / `/add`) は廃止: 登録経路は
// `narou_rs_login` の書き出しファイルを `POST /api/login/import` で取り込むだけ。
// 一覧は `GET /api/login` の `data.sites[].logins[]` (host ではなく site がキー)。
//
// `NAROU_RS_LOGIN_KEY` 未設定のデプロイは fail-closed: 保存系は失敗し、値は
// 保存も返却もしない。その場合の後続チェックは「成功扱いで中身を見ない」
// 旧来の挙動に揃えて skip する。
const LOGIN_SITE = "contract-login.example";
const LOGIN_EXTRA_SITE = "contract-extra.example";
const LOGIN_ENVELOPE = [
  "version: 3",
  "exported_at: 2026-09-27T00:00:00+09:00",
  "encrypted: false",
  "sites:",
  "  contract-login.example:",
  "  - site: contract-login.example",
  "    cookies:",
  "    - host: contract-login.example",
  "      cookie: session=contract-secret",
  "  - site: contract-login.example",
  "    cookies:",
  "    - host: contract-login.example",
  "      cookie: session=contract-alt-secret",
  "  contract-extra.example:",
  "  - site: contract-extra.example",
  "    cookies:",
  "    - host: contract-extra.example",
  "      cookie: token=contract-extra-secret",
  "",
].join("\n");

// 保存系の検査は、import が書けた (= 鍵あり) 環境でのみ意味がある。
// `null` = 未判定、それ以外は GET /api/login の key_source で決める。
let loginsWritable = null;
async function loginWritable() {
  if (loginsWritable === null) {
    const body = await json(await request("/api/login", auth()));
    loginsWritable = body.data?.key_source !== "none";
  }
  return loginsWritable;
}

await check("POST /api/login/import stores named logins without echoing cookies", async () => {
  const response = await request("/api/login/import", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ envelope: LOGIN_ENVELOPE, name: "契約" }),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  if (body.success !== true) {
    // 復号鍵 (`NAROU_RS_LOGIN_KEY`) が未設定のデプロイは fail-closed。値は保存も返却もしない。
    assert(
      !JSON.stringify(body).includes("contract-secret"),
      `the cookie value must never be echoed: ${JSON.stringify(body)}`,
    );
    assert(
      String(body.message ?? "").includes("NAROU_RS_LOGIN_KEY"),
      `unexpected failure: ${JSON.stringify(body)}`,
    );
    return;
  }
  const site = (body.data?.sites ?? []).find((entry) => entry.site === LOGIN_SITE);
  assert(site, `site should be listed: ${JSON.stringify(body)}`);
  assert(site.encrypted === true, "the stored value must be encrypted at rest");
  const raw = JSON.stringify(body);
  assert(!raw.includes("contract-secret"), "the cookie value must never be returned");
  assert(
    !raw.includes("contract-alt-secret") && !raw.includes("contract-extra-secret"),
    "no cookie value may be returned",
  );
  // 1 ファイルの 2 セッションは別ログインのまま (ホストが重なるので畳まれない)。
  assert.strictEqual(site.logins.length, 2, `two sessions stay two logins: ${JSON.stringify(site)}`);
  // `name` で取り込んだので 1 サイト複数件には連番が付く。
  assert.deepStrictEqual(
    site.logins.map((login) => login.label),
    ["契約 1", "契約 2"],
    `the import name must label each session: ${JSON.stringify(site.logins)}`,
  );
  assert(site.logins[0].hosts[0].cookies.includes("session=…"), "the value must be masked");
});

await checkWithAuth("GET /api/login is closed without a token", async () => {
  const response = await request("/api/login");
  assert(response.status === 401, `status ${response.status}`);
  assert((await errorCode(response)) === "authentication_required", "code must be set");
});

await check("GET /api/login lists sites without values", async () => {
  const response = await request("/api/login", auth());
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(Array.isArray(body.data?.sites), "sites must be an array");
  assert(typeof body.data?.sites_count === "number", "sites_count must be a number");
  assert(typeof body.data?.logins_count === "number", "logins_count must be a number");
  assert(
    ["NAROU_RS_LOGIN_KEY", "none"].includes(body.data?.key_source),
    `unexpected key_source=${body.data?.key_source}`,
  );
  const raw = JSON.stringify(body);
  assert(!raw.includes("contract-secret"), "values must stay hidden");
  assert(!raw.includes("contract-extra-secret"), "values must stay hidden");
});

await check("POST /api/login/rename labels one login", async () => {
  if (!(await loginWritable())) return;
  const response = await request("/api/login/rename", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ site: LOGIN_SITE, index: 0, label: "リネーム済み" }),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.success === true, `rename must succeed: ${JSON.stringify(body).slice(0, 200)}`);
  const site = (body.data?.sites ?? []).find((entry) => entry.site === LOGIN_SITE);
  assert.strictEqual(site?.logins[0]?.label, "リネーム済み", "the label must be stored");
});

await check("POST /api/login/order reorders a site's logins", async () => {
  if (!(await loginWritable())) return;
  const response = await request("/api/login/order", {
    method: "POST",
    headers: { "content-type": "application/json", ...auth().headers },
    body: JSON.stringify({ site: LOGIN_SITE, order: [1, 0] }),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.success === true, `order must succeed: ${JSON.stringify(body).slice(0, 200)}`);
  const site = (body.data?.sites ?? []).find((entry) => entry.site === LOGIN_SITE);
  // 並べ替え前は [リネーム済み, 契約 2]。
  assert.deepStrictEqual(
    site?.logins?.map((login) => login.label),
    ["契約 2", "リネーム済み"],
    `the order must be applied: ${JSON.stringify(site)}`,
  );
});

await check("DELETE /api/login/{site}/{index} removes one login", async () => {
  if (!(await loginWritable())) return;
  const response = await request(`/api/login/${LOGIN_SITE}/0`, {
    method: "DELETE",
    ...auth(),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.success === true, `delete must succeed: ${JSON.stringify(body).slice(0, 200)}`);
  const site = (body.data?.sites ?? []).find((entry) => entry.site === LOGIN_SITE);
  assert.deepStrictEqual(
    site?.logins?.map((login) => login.label),
    ["リネーム済み"],
    `only the other login may remain: ${JSON.stringify(site)}`,
  );
});

await check("DELETE /api/login/{site} clears a site's logins", async () => {
  if (!(await loginWritable())) return;
  const response = await request(`/api/login/${LOGIN_SITE}`, {
    method: "DELETE",
    ...auth(),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.success === true, `clear_site must succeed: ${JSON.stringify(body).slice(0, 200)}`);
  const sites = body.data?.sites ?? [];
  assert(
    !sites.some((entry) => entry.site === LOGIN_SITE),
    `site should be gone: ${JSON.stringify(body)}`,
  );
});

await check("DELETE /api/login/{site}/{index} removes a site's last login", async () => {
  if (!(await loginWritable())) return;
  const response = await request(`/api/login/${LOGIN_EXTRA_SITE}/0`, {
    method: "DELETE",
    ...auth(),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.success === true, `delete must succeed: ${JSON.stringify(body).slice(0, 200)}`);
  const sites = body.data?.sites ?? [];
  assert(
    !sites.some((entry) => entry.site === LOGIN_EXTRA_SITE),
    `site should be gone: ${JSON.stringify(body)}`,
  );
});

await check("the retired direct-cookie routes stay unavailable", async () => {
  // `set` / `add` は `{site}` として DELETE 専用ルートに落ちるので
  // 登録は 405 で断られる (native の `/{site}` DELETE と同じ応答)。
  for (const path of ["/api/login/set", "/api/login/add"]) {
    const response = await request(path, {
      method: "POST",
      headers: { "content-type": "application/json", ...auth().headers },
      body: JSON.stringify({ host: "example.com", cookie: "session=x" }),
    });
    assert(response.status === 405, `${path} must not register cookies: ${response.status}`);
  }
});

await check("POST /api/admin/object-migration status reports progress", async () => {
  const response = await request("/api/admin/object-migration", {
    method: "POST",
    ...auth(),
    body: JSON.stringify({ action: "status" }),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.action === "status", `unexpected action: ${JSON.stringify(body)}`);
  assert(typeof body.copied === "number", "copied must be a number");
  assert(Array.isArray(body.failed), "failed must be an array");
});

await checkWithAuth("POST /api/admin/object-migration requires auth", async () => {
  const response = await request("/api/admin/object-migration", {
    method: "POST",
    body: JSON.stringify({ action: "status" }),
  });
  assert(response.status === 401, `status ${response.status}`);
  assert((await errorCode(response)) === "authentication_required", "code must be set");
});

if (QUEUE_CHECKS) {
  await check("Convert job for a missing novel reaches a terminal state", async () => {
    assert(convertJobId, "the Convert plan was not accepted");
    const deadline = Date.now() + 60_000;
    let status = "pending";
    while (Date.now() < deadline) {
      const response = await request(`/api/jobs/${convertJobId}`, auth());
      assert(response.status === 200, `status ${response.status}`);
      const body = await json(response);
      status = body.status;
      if (TERMINAL_STATUSES.has(status)) return;
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
    throw new Error(`job stayed ${status} for 60s (queue consumer did not run)`);
  });

  await check("console history keeps the convert lines per stream and clear empties both", async () => {
    const prior = settingValues(await fetchSettingsView()).get("concurrency") ?? null;
    await saveSettings({ concurrency: true });
    try {
    // 直前の検査の Convert ジョブ (999999999) の終端 echo が履歴に載るまで
    // 待ってから消す — 消去後に in-flight の echo が着地する競合を避ける。
    await waitForHistory("stdout", "999999999");

    // 前回実行の残りを落としてから、専用の Convert ジョブで履歴を作る。
    const cleared = await json(
      await request("/api/clear_history", { method: "POST", ...auth() }),
    );
    assert(cleared.success === true, `clear must succeed: ${JSON.stringify(cleared)}`);
    for (const stream of ["stdout", "stdout2"]) {
      const empty = await json(
        await request(`/api/history?format=json&stream=${stream}`, auth()),
      );
      assert.equal(empty.history, "", `cleared ${stream} history must be empty`);
    }

    const marker = "999999901";
    const jobId = await enqueuePlan("Convert", marker);
    await waitForTerminal(jobId);
    // Convert 行は concurrency 既定 (有効) で stdout2、終端の失敗 echo は
    // stdout に出る (native `console_target_for_job` と同じ分離)。
    const subHistory = await waitForHistory("stdout2", `処理中: ${marker}`);
    assert(subHistory.includes("変換処理開始"), `stdout2 must hold the convert lines: ${subHistory.slice(-400)}`);

    const main = await json(await request("/api/history?format=json", auth()));
    assert(main.history.includes(marker), `stdout must hold the terminal failure echo: ${main.history.slice(-400)}`);
    assert(!main.history.includes("処理中"), `stdout must not receive stdout2 lines: ${main.history.slice(-400)}`);

    // 「すべての履歴を取得」の再読で、まだ上限内の行が消えない。
    const again = await json(
      await request("/api/history?format=json&stream=stdout2", auth()),
    );
    assert.equal(again.history, subHistory, "re-reading history must not drop earlier lines");

    // format 無しは native と同じ text/plain。
    const text = await request("/api/history?stream=stdout2", auth());
    assert(text.status === 200, `status ${text.status}`);
    assert(
      text.headers.get("content-type")?.includes("text/plain"),
      "text history must be plain text",
    );
    assert.equal(await text.text(), subHistory, "text history must match the JSON history");
    } finally {
      await saveSettings({ concurrency: prior });
    }
  });

  await check("a websocket reconnect replays history and cannot revive cleared lines", async () => {
    // 事前条件: 直前の検査の履歴が残っている。再接続がそれを再生することを
    // 確かめてから消す (消去後の「再生されない」を意味ある検査にする)。
    const replaySocket = await openWebSocket();
    try {
      const deadline = Date.now() + 5000;
      while (Date.now() < deadline && replaySocket.echoBodies("999999901").length === 0) {
        await delay(100);
      }
      assert(
        replaySocket.echoBodies("999999901").length > 0,
        `reconnect must replay persisted history: ${JSON.stringify(replaySocket.echoBodies().slice(-3))}`,
      );
    } finally {
      replaySocket.close();
    }

    const cleared = await json(
      await request("/api/clear_history", { method: "POST", ...auth() }),
    );
    assert(cleared.success === true, `clear must succeed: ${JSON.stringify(cleared)}`);
    for (const stream of ["stdout", "stdout2"]) {
      const after = await json(
        await request(`/api/history?format=json&stream=${stream}`, auth()),
      );
      assert.equal(after.history, "", `clear must empty the persisted ${stream} history`);
    }

    const clearedSocket = await openWebSocket();
    try {
      await delay(500);
      assert.deepEqual(
        clearedSocket.echoBodies(),
        [],
        `a reconnect must not revive cleared lines: ${JSON.stringify(clearedSocket.echoBodies().slice(-3))}`,
      );
    } finally {
      clearedSocket.close();
    }
  });

  await check("webui.table.reload-timing gates the terminal table refresh", async () => {
    // ローカルの queue consumer はメッセージを 1 通ずつ直列に配送する
    // (max_batch_size=1。max_concurrency はローカルでは効かない) ため、
    // 「別のジョブが残っている間の終端」は pacing に入る Update の後ろへ速い
    // Convert を並べて作る: 直列配送では Convert が pending、並行配送では
    // Update が running のまま残る。どちらでも最後の終端だけがキューを掃くので、
    // 同じイベント列の順序で契約を検査できる。
    const view = await json(await request("/api/global_setting", auth()));
    const currentValue = (name) =>
      view.settings?.find((item) => item.name === name)?.value ?? null;
    const previousInterval = currentValue("update.interval");
    const previousTiming = currentValue("webui.table.reload-timing");

    await request("/api/queue/clear", {
      method: "POST",
      headers: { "content-type": "application/json", ...auth().headers },
      body: "{}",
    });
    // pacing (native の下限は 2.5 秒) を確実に観測できる間隔にする。
    await saveSettings({ "update.interval": 5, "webui.table.reload-timing": "queue" });

    const terminalEvents = (messages, first, second) =>
      messages.filter(
        (message) =>
          message?.type === "queue_failed" &&
          (message?.data?.job_id === first || message?.data?.job_id === second),
      );
    const socket = await openWebSocket();
    try {
      // A: このドメインの pacing スロットを消費して終端する (最初の Update は
      // 待たず、2 通目から update.interval の待ちに入る)。
      const pacingJobId = await enqueuePlan("Update", 999999911);
      await waitForTerminal(pacingJobId);

      // queue タイミング: もう 1 つのジョブが pending または running のまま
      // 先に終端する側では table.reload を送らず、キューが掃けた最後の終端で
      // だけ送る。progressbar.clear / notification.queue は抑制中も届く。
      const queueMark = socket.mark();
      const pacedJobId = await enqueuePlan("Update", 999999912);
      await waitForStatus(pacedJobId, "running");
      const convertC = await enqueuePlan("Convert", 999999913);
      await waitForTerminal(convertC);
      await waitForTerminal(pacedJobId);
      await waitForEvent(socket, queueMark, (message) => message?.type === "table.reload");
      const duringQueue = socket.slice(queueMark);
      const queueTerminals = terminalEvents(duringQueue, pacedJobId, convertC);
      assert.equal(
        queueTerminals.length,
        2,
        `both terminals must appear: ${JSON.stringify(duringQueue.map((m) => m?.type))}`,
      );
      const beforeLastQueueTerminal = duringQueue.slice(
        0,
        duringQueue.lastIndexOf(queueTerminals[1]),
      );
      assert(
        !beforeLastQueueTerminal.some((message) => message?.type === "table.reload"),
        `queue timing must suppress table.reload until the queue drains: ${JSON.stringify(duringQueue.map((m) => m?.type))}`,
      );
      assert(
        duringQueue.some((message) => message?.type === "table.reload"),
        `the terminal that drained the queue must send table.reload: ${JSON.stringify(duringQueue.map((m) => m?.type))}`,
      );
      assert(
        beforeLastQueueTerminal.some((message) => message?.type === "notification.queue"),
        "notification.queue must stay live even when the table reload is suppressed",
      );
      assert(
        beforeLastQueueTerminal.some((message) => message?.type === "progressbar.clear"),
        "progressbar.clear must stay live even when the table reload is suppressed",
      );

      // every: 実行中/待機中のジョブが残っていても、各終端が table.reload を送る。
      await saveSettings({ "webui.table.reload-timing": "every" });
      const everyMark = socket.mark();
      const pacedJobE = await enqueuePlan("Update", 999999914);
      const convertD = await enqueuePlan("Convert", 999999915);
      await waitForTerminal(pacedJobE);
      await waitForTerminal(convertD);
      await waitForEvent(socket, everyMark, (message) => message?.type === "table.reload");
      const duringEvery = socket.slice(everyMark);
      const everyTerminals = terminalEvents(duringEvery, pacedJobE, convertD);
      assert.equal(
        everyTerminals.length,
        2,
        `both terminals must appear: ${JSON.stringify(duringEvery.map((m) => m?.type))}`,
      );
      const beforeLastEveryTerminal = duringEvery.slice(
        0,
        duringEvery.lastIndexOf(everyTerminals[1]),
      );
      assert(
        beforeLastEveryTerminal.some((message) => message?.type === "table.reload"),
        `every timing must send table.reload while another job remains: ${JSON.stringify(duringEvery.map((m) => m?.type))}`,
      );
      assert(
        duringEvery.some((message) => message?.type === "tag.updateCanvas"),
        "every timing must send tag.updateCanvas with the table reload",
      );
    } finally {
      socket.close();
      await saveSettings({
        "update.interval": previousInterval,
        "webui.table.reload-timing": previousTiming,
      });
    }
  });
} else {
  console.log("skip Convert job terminal-state check (set CONTRACT_QUEUE=1 to enable)");
}

// トークン未設定の Worker は 401 ではなく 500 + `authentication_not_configured` で
// 失敗する（設定漏れが「トークン違い」に見えないように）。
if (process.env.CONTRACT_EXPECT_UNCONFIGURED === "1") {
  await test("an unconfigured Worker fails closed with a distinct code", async () => {
    const response = await fetchWithAccess(new URL("/api/novels", BASE_URL).toString());
    assert.equal(response.status, 500);
    const body = await response.json();
    assert.equal(body?.error?.code, "authentication_not_configured");
  });
}
