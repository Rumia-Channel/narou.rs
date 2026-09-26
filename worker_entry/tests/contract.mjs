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
if (!ACCESS_CLIENT_ID && process.env.CONTRACT_SKIP_ACCESS_PROBE !== "1") {
  const probe = await fetchWithAccess(url("/health/live"), { redirect: "manual" });
  const location = probe.headers.get("location") ?? "";
  if ([301, 302, 303, 307, 308].includes(probe.status) && location.includes("cloudflareaccess.com")) {
    console.log(`Cloudflare Access is in front of ${BASE_URL}; skipping the remote smoke`);
    process.exit(3);
  }
}

/** 検査を 1 件定義する（`node --test` で走る）。 */
function check(name, fn) {
  if (process.env.CONTRACT_EXPECT_UNCONFIGURED === "1") {
    return;
  }
  test(name, fn);
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

await check("the scheduled handler runs (cron planner)", async () => {
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
  assert(body.success === true, `unexpected body: ${JSON.stringify(body)}`);
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
    body.data?.key_source === "NAROU_RS_LOGIN_KEY",
    `the login key must be configured (key_source=${body.data?.key_source})`,
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
