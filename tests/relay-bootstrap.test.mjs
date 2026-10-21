// **非推奨: SORAHOST / Cloud Run (GCP) へのリレー配備は 2026-10-21 に非推奨。**
// CI では実行しない (tests/run-relay.mjs から除外)。既存デプロイの参照用。
//
// Cloud Run (Buildpacks) の起動エントリーポイント (bootstrap.mjs) の回帰。
//
// 実ネットワークは使わない。ダウンロードと commandExists を差し替えて、
// 静的 curl の準備・再利用条件・失敗時のフォールバックを確認する。
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { prepareCurl } from "../scripts/relay/bootstrap.mjs";
import { sha256 } from "../scripts/relay/fetch-curl.mjs";
import { BLOCKED_BUNDLE } from "./relay-blocked-ca-fixture.mjs";
import { cert } from "./relay-tls-fixture.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const FAKE_CURL = Buffer.from("#!/bin/sh\necho fake curl\n");
const FAKE_CA = Buffer.from(cert);
const PINS = { version: "test-v1", sha256: sha256(FAKE_CURL) };
const windows = process.platform === "win32";

const findCurl = () => {
  const probe = spawnSync(windows ? "where" : "which", ["curl"], { encoding: "utf8" });
  return probe.status === 0 ? probe.stdout.split(/\r?\n/)[0].trim() : "";
};

const freePort = async () => {
  const probe = createServer();
  probe.listen(0, "127.0.0.1");
  await new Promise((resolve) => probe.once("listening", resolve));
  const { port } = probe.address();
  await new Promise((resolve) => probe.close(resolve));
  return port;
};

const waitFor = async (predicate, label, timeoutMs = 20_000) => {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`${label} を待っていますが変わりませんでした`);
};

const fakeDownload = (calls) => async (url) => {
  if (url.endsWith("/curl-amd64")) { calls.curl += 1; return FAKE_CURL; }
  if (url.endsWith("/sha256sum.txt")) { calls.summary += 1; return Buffer.from(`${sha256(FAKE_CURL)}  ./curl-amd64\n`); }
  if (url.endsWith("cacert.pem")) { calls.cacert += 1; return FAKE_CA; }
  throw new Error(`unexpected download: ${url}`);
};

test("uses an explicit NAROU_RELAY_CURL without downloading", async () => {
  const env = { NAROU_RELAY_CURL: "/opt/curl" };
  const result = await prepareCurl({ env, pins: PINS, commandExists: () => false, download: async () => { throw new Error("no"); } });
  assert.deepEqual(result, { prepared: false, reason: "env" });
  assert.equal(env.NAROU_RELAY_CACERT, undefined);
});

test("keeps the PATH curl when one is available", async () => {
  const env = {};
  const result = await prepareCurl({ env, pins: PINS, commandExists: () => true, download: async () => { throw new Error("no"); } });
  assert.deepEqual(result, { prepared: false, reason: "path" });
  assert.equal(env.NAROU_RELAY_CURL, undefined);
});

test("downloads and verifies a static curl when PATH has none", async () => {
  const calls = { curl: 0, summary: 0, cacert: 0 };
  const env = {};
  const result = await prepareCurl({
    env, pins: PINS, commandExists: () => false, download: fakeDownload(calls), log: () => {},
  });
  assert.equal(result.reason, "downloaded");
  assert.equal(env.NAROU_RELAY_CURL, result.curl);
  assert.equal(env.NAROU_RELAY_CACERT, result.cacert);
  assert.equal(readFileSync(result.curl, "utf8"), FAKE_CURL.toString());
  assert.equal(readFileSync(result.cacert, "utf8"), FAKE_CA.toString());
  assert.equal(sha256(readFileSync(result.curl)), PINS.sha256);
  assert.equal(calls.curl, 1);
  assert.equal(calls.cacert, 1);
  if (!windows) assert.equal(statSync(result.curl).mode & 0o111, 0o111, "実行ビットを付ける");

  // 用意済みの環境でもう一度呼んでも、既存の指定を尊重して再取得しない。
  const again = await prepareCurl({ env, pins: PINS, commandExists: () => false, download: fakeDownload(calls), log: () => {} });
  assert.equal(again.reason, "env");
  assert.equal(calls.curl, 1);
  rmSync(result.curl, { force: true });
  rmSync(result.cacert, { force: true });
});

test("falls back to the Node implementation when the download fails", async () => {
  const env = {};
  const logs = [];
  const result = await prepareCurl({
    env, pins: PINS, commandExists: () => false, log: (line) => logs.push(line),
    download: async () => { throw new Error("network down"); },
  });
  assert.deepEqual(result, { prepared: false, reason: "unavailable" });
  assert.equal(env.NAROU_RELAY_CURL, undefined);
  assert.ok(logs.some((line) => line.includes("Node 実装で続行します")));
});

test("a CA bundle with a compromised root stops bootstrapping", async () => {
  const env = {};
  const base = fakeDownload({ curl: 0, summary: 0, cacert: 0 });
  await assert.rejects(
    prepareCurl({
      env, pins: PINS, commandExists: () => false, log: () => {},
      download: async (url) => (url.endsWith("cacert.pem") ? BLOCKED_BUNDLE : base(url)),
    }),
    (error) => error && error.name === "CompromisedCaError",
    "Node 実装へ落とさず起動を止める",
  );
  assert.equal(env.NAROU_RELAY_CURL, undefined);
});

test("an invalid pin hash is rejected by fetchCurl", async () => {
  const env = {};
  const result = await prepareCurl({
    env, pins: { version: "test-v1", sha256: "0".repeat(64) }, commandExists: () => false,
    download: fakeDownload({ curl: 0, summary: 0, cacert: 0 }), log: () => {},
  });
  assert.equal(result.reason, "unavailable");
  assert.equal(env.NAROU_RELAY_CURL, undefined);
});

test("bootstrap starts server.mjs with the configured curl", { timeout: 60_000 }, async (t) => {
  const curlPath = findCurl();
  if (!curlPath) return t.skip("PATH に curl が無い");
  const directory = mkdtempSync(join(tmpdir(), "relay-bootstrap-"));
  let child;
  try {
    const envFile = join(directory, ".env");
    writeFileSync(envFile, [
      "NAROU_RELAY_KEY=bootstrap-test-key",
      "NAROU_RELAY_ALLOWED_HOSTS=example.com",
      "NAROU_RELAY_HOST=0.0.0.0",
    ].join("\n"));
    const port = await freePort();
    let output = "";
    child = spawn(process.execPath, [join(root, "scripts", "relay", "bootstrap.mjs")], {
      cwd: root,
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        SERVER_PORT: String(port),
        NAROU_RELAY_ENV_FILE: envFile,
        NAROU_RELAY_CURL: curlPath,
        NAROU_RELAY_CACERT: "",
        K_SERVICE: "",
      },
    });
    child.stdout.on("data", (chunk) => { output += chunk.toString(); });
    child.stderr.on("data", (chunk) => { output += chunk.toString(); });
    await waitFor(() => output.includes(`listening on 0.0.0.0:${port}`), `待受ログ (${output.slice(-300)})`);
    const health = await fetch(`http://127.0.0.1:${port}/health`);
    assert.equal(health.status, 200);
    assert.deepEqual(await health.json(), { ok: true, fetch: "curl,node" });
    const denied = await fetch(`http://127.0.0.1:${port}/proxy`, { method: "POST" });
    assert.equal(denied.status, 403);
  } finally {
    if (child && child.exitCode === null) child.kill();
    await new Promise((resolve) => setTimeout(resolve, 300));
    rmSync(directory, { recursive: true, force: true });
  }
});
