// Cloudflare Workers の契約テスト。
//
// HTTP だけを叩くので、ローカルの `wrangler dev` でもデプロイ済みの環境でも
// 同じものを使える。
//
//   NAROU_ADMIN_TOKEN=... BASE_URL=http://127.0.0.1:8787 node tests/contract.mjs
//
// CONTRACT_QUEUE=1 のときだけ queue consumer が回る前提の検査 (Convert ジョブが
// 終端状態になるまで待つ) を行う。ローカルでも `wrangler dev` は queue を
// 処理するが、環境によっては配送されないので既定では無効にする。
import process from "node:process";

const BASE_URL = process.env.BASE_URL ?? "http://127.0.0.1:8787";
const TOKEN = process.env.NAROU_ADMIN_TOKEN;
const QUEUE_CHECKS = process.env.CONTRACT_QUEUE === "1";
const TERMINAL_STATUSES = new Set(["succeeded", "blocked", "permanent"]);

if (!TOKEN) {
  console.error("NAROU_ADMIN_TOKEN is required");
  process.exit(2);
}

let failures = 0;

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

async function check(name, fn) {
  try {
    await fn();
    console.log(`ok   ${name}`);
  } catch (error) {
    failures += 1;
    console.error(`FAIL ${name}: ${error.message}`);
  }
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
  return fetch(url(path), init);
}

await check("GET /health/live returns 200 without auth", async () => {
  const response = await request("/health/live");
  assert(response.status === 200, `status ${response.status}`);
});

await check("GET /health/ready returns 200 (D1 + queue + DO + composition)", async () => {
  const response = await request("/health/ready");
  const body = await response.text();
  assert(response.status === 200, `status ${response.status}: ${body.slice(0, 200)}`);
});

await check("GET /api/novels is closed without a token", async () => {
  const response = await request("/api/novels");
  assert(response.status === 401, `status ${response.status}`);
});

await check("GET /api/novels rejects a wrong token", async () => {
  const response = await request("/api/novels", auth("definitely-not-the-token"));
  assert(response.status === 401, `status ${response.status}`);
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

await check("POST /api/jobs requires auth", async () => {
  const response = await request("/api/jobs", { method: "POST", body: "{}" });
  assert(response.status === 401, `status ${response.status}`);
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

if (failures > 0) {
  console.error(`${failures} contract check(s) failed`);
  process.exit(1);
}
console.log("all contract checks passed");
