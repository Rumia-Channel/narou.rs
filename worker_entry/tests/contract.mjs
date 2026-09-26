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

await check("GET /api/story returns 404 for an unknown novel", async () => {
  const response = await request("/api/story?id=999999999", auth());
  assert(response.status === 404, `status ${response.status}`);
});

await check("GET /api/taginfo.json returns an array", async () => {
  const response = await request("/api/taginfo.json", auth());
  assert(response.status === 200, `status ${response.status}`);
  assert(Array.isArray(await json(response)), "taginfo must be an array");
});

await check("GET /api/history returns the empty history", async () => {
  const response = await request("/api/history", auth());
  assert(response.status === 200, `status ${response.status}`);
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
    body: JSON.stringify({ host: "example.com", order: [0, 1] }),
  });
  assert(response.status === 200, `status ${response.status}`);
  assert((await json(response)).success === false, "an unknown host must be refused");
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
  assert(body.mode === "sqlite", `the worker stores in D1: ${JSON.stringify(body)}`);
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

await check("POST /api/login/set stores a credential without echoing it", async () => {
  const response = await request("/api/login/set", {
    method: "POST",
    ...auth(),
    body: JSON.stringify({
      host: "example.com",
      cookie: "session=contract-secret",
      label: "contract",
    }),
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
  const hosts = body.data?.hosts ?? [];
  const entry = hosts.find((host) => host.host === "example.com");
  assert(entry, `host should be listed: ${JSON.stringify(body)}`);
  assert(entry.encrypted === true, "the stored value must be encrypted at rest");
  const raw = JSON.stringify(body);
  assert(!raw.includes("contract-secret"), "the cookie value must never be returned");
  assert(entry.credentials[0].cookies.includes("session=…"), "the value must be masked");
});

await check("GET /api/login lists hosts without values", async () => {
  const response = await request("/api/login", auth());
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(typeof body.data?.count === "number", "count must be a number");
  assert(
    ["NAROU_RS_LOGIN_KEY", "none"].includes(body.data?.key_source),
    `unexpected key_source=${body.data?.key_source}`,
  );
  assert(!JSON.stringify(body).includes("contract-secret"), "values must stay hidden");
});

await check("DELETE /api/login/{host} clears the credential", async () => {
  const response = await request("/api/login/example.com", {
    method: "DELETE",
    ...auth(),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  const hosts = body.data?.hosts ?? [];
  assert(
    !hosts.some((host) => host.host === "example.com"),
    `host should be gone: ${JSON.stringify(body)}`,
  );
});

await check("POST /api/login/set rejects an empty cookie", async () => {
  const response = await request("/api/login/set", {
    method: "POST",
    ...auth(),
    body: JSON.stringify({ host: "example.com", cookie: "  " }),
  });
  const body = await json(response);
  assert(response.status === 200, `status ${response.status}`);
  assert(body.success === false, `unexpected body: ${JSON.stringify(body)}`);
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
} else {
  console.log("skip Convert job terminal-state check (set CONTRACT_QUEUE=1 to enable)");
}

// トークン未設定の Worker は 401 ではなく 500 + `authentication_not_configured` で
// 失敗する（設定漏れが「トークン違い」に見えないように）。
if (process.env.CONTRACT_EXPECT_UNCONFIGURED === "1") {
  await check("an unconfigured Worker fails closed with a distinct code", async () => {
    const response = await fetchWithAccess(new URL("/api/novels", BASE_URL).toString());
    assert.equal(response.status, 500);
    const body = await response.json();
    assert.equal(body?.error?.code, "authentication_not_configured");
  });
}
