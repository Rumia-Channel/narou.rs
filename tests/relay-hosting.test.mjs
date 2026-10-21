// **非推奨: SORAHOST / Cloud Run (GCP) へのリレー配備は 2026-10-21 に非推奨。**
// CI では実行しない (tests/run-relay.mjs から除外)。既存デプロイの参照用。
//
// Cloud Run と PteWorker (SORAHOST) で同じリレーが動くことを固定する回帰。
// 実 HTTP 取得は relay-server.test.mjs / relay.test.mjs が担当し、ここは
// ホストごとの差 (PORT/SERVER_PORT・待受アドレス・.env・同梱 curl) だけを見る。
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { chmodSync, copyFileSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { loadEnvFile, parseEnvFile, resolveHost, resolvePort } from "../scripts/relay/env.mjs";
import { resolveCurl } from "../scripts/relay/transport.mjs";
import { createRelayServer, startRelay } from "../scripts/relay/server.mjs";
import { resolveLocalTarget } from "./relay-fixture.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const relayDir = join(root, "scripts", "relay");
const TOKEN = "relay-hosting-test-key";
const absent = () => join(tmpdir(), `narou-relay-absent-${process.pid}-${Math.random().toString(16).slice(2)}`);
const close = (server) => new Promise((resolve) => { server.close(resolve); server.closeAllConnections(); });
const freePort = async () => {
  const probe = createNetServer();
  probe.listen(0, "127.0.0.1");
  await once(probe, "listening");
  const { port } = probe.address();
  await new Promise((resolve) => probe.close(resolve));
  return port;
};

test("PORT wins over SERVER_PORT and defaults to 3000", () => {
  assert.equal(resolvePort({ PORT: "18080", SERVER_PORT: "9000" }), 18080);
  assert.equal(resolvePort({ SERVER_PORT: "18080" }), 18080, "PteWorker は SERVER_PORT を渡す");
  assert.equal(resolvePort({ PORT: " 8080 " }), 8080);
  assert.equal(resolvePort({ PORT: "", SERVER_PORT: "8080" }), 8080);
  assert.equal(resolvePort({}), 3000);
  for (const value of ["abc", "70000", "-1", "1.5"]) assert.throws(() => resolvePort({ PORT: value }));
});

test("listener host follows the platform and can be overridden", () => {
  assert.equal(resolveHost({ K_SERVICE: "narou-relay" }), "0.0.0.0", "Cloud Run はコンテナ外から届く");
  assert.equal(resolveHost({}), "127.0.0.1", "PteWorker はループバック束縛が要件");
  assert.equal(resolveHost({ K_SERVICE: "narou-relay", NAROU_RELAY_HOST: "127.0.0.1" }), "127.0.0.1");
  assert.equal(resolveHost({ NAROU_RELAY_HOST: "0.0.0.0" }), "0.0.0.0");
});

test("a .env fills missing values only and keeps the platform environment", () => {
  const dir = mkdtempSync(join(tmpdir(), "relay-env-"));
  try {
    writeFileSync(join(dir, ".env"), [
      "# コメント", "", "NAROU_RELAY_KEY=from-file",
      "NAROU_RELAY_ALLOWED_HOSTS=example.com,example.org",
      "EMPTY=", "BROKEN", "=x", "1BAD=y", "SPACED = value",
    ].join("\n"));
    const env = { NAROU_RELAY_KEY: "from-env" };
    const loaded = loadEnvFile({ env, dir, home: absent() });
    assert.equal(loaded.path, join(dir, ".env"));
    assert.deepEqual(loaded.applied, ["NAROU_RELAY_ALLOWED_HOSTS", "EMPTY"]);
    assert.equal(env.NAROU_RELAY_KEY, "from-env", "プラットフォームの値を上書きしない");
    assert.equal(env.NAROU_RELAY_ALLOWED_HOSTS, "example.com,example.org");
    assert.equal(env.EMPTY, "", "空の値も読む (キーが無いのとは別)");
    assert.equal(env.BROKEN, undefined);
    assert.equal(env["1BAD"], undefined);
    assert.equal(env.SPACED, undefined, "キーに空白を含む行は読まない");
    assert.equal(parseEnvFile("A=1\r\nB=2").get("B"), "2", "CRLF も読む");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  assert.equal(loadEnvFile({ env: {}, dir: absent(), home: absent() }), null);
  assert.equal(loadEnvFile({ env: { NAROU_RELAY_ENV_FILE: absent() }, dir: relayDir, home: absent() }), null,
    "NAROU_RELAY_ENV_FILE を指定したら他の候補は見ない");
});

test(".env falls back to the module directory and then HOME", () => {
  const dir = mkdtempSync(join(tmpdir(), "relay-env-dir-"));
  const home = mkdtempSync(join(tmpdir(), "relay-env-home-"));
  try {
    writeFileSync(join(dir, ".env"), "WHERE=module\n");
    writeFileSync(join(home, ".env"), "WHERE=home\nFROM_HOME=1\n");
    const first = {};
    assert.equal(loadEnvFile({ env: first, dir, home }).path, join(dir, ".env"), "モジュールの隣を優先する");
    assert.equal(first.WHERE, "module");
    rmSync(join(dir, ".env"));
    const second = {};
    assert.equal(loadEnvFile({ env: second, dir, home }).path, join(home, ".env"), "無ければ HOME 直下 (PteWorker の /home/container)");
    assert.equal(second.WHERE, "home");
    assert.equal(second.FROM_HOME, "1");
  } finally {
    rmSync(dir, { recursive: true, force: true });
    rmSync(home, { recursive: true, force: true });
  }
});

test("curl resolves as env, then bundled, then PATH", () => {
  assert.deepEqual(resolveCurl({ env: { NAROU_RELAY_CURL: "/opt/curl" }, dir: absent() }),
    { bin: "/opt/curl", cacert: null, source: "env" });
  assert.deepEqual(resolveCurl({ env: { NAROU_RELAY_CURL: "/opt/curl", NAROU_RELAY_CACERT: "/opt/ca.pem" }, dir: absent() }),
    { bin: "/opt/curl", cacert: "/opt/ca.pem", source: "env" });
  assert.equal(resolveCurl({ env: {}, dir: relayDir }).source, "path", "リポジトリには同梱 curl が無い");

  const dir = mkdtempSync(join(tmpdir(), "relay-curl-"));
  let prepared;
  try {
    writeFileSync(join(dir, "curl"), "#!/bin/sh\nexit 0\n");
    chmodSync(join(dir, "curl"), 0o755);
    writeFileSync(join(dir, "cacert.pem"), "ca\n");
    const resolved = resolveCurl({ env: {}, dir, verify: false });
    prepared = resolved.bin;
    assert.equal(resolved.source, "bundled");
    assert.equal(resolved.cacert, join(dir, "cacert.pem"), "同梱 curl には同梱 CA を渡す");
    assert.notEqual(resolved.bin, join(dir, "curl"), "書き込み可能な場所へコピーして実行する");
    assert.equal(readFileSync(resolved.bin, "utf8"), "#!/bin/sh\nexit 0\n");
    if (process.platform !== "win32") assert.equal(statSync(resolved.bin).mode & 0o111, 0o111);
    assert.equal(resolveCurl({ env: {}, dir: absent() }).source, "path", "同梱が無ければ PATH の curl");
  } finally {
    if (prepared) rmSync(prepared, { force: true });
    rmSync(dir, { recursive: true, force: true });
  }
});

test("SERVER_PORT alone starts the PteWorker shape on loopback", async () => {
  const env = { SERVER_PORT: "0", NAROU_RELAY_KEY: TOKEN, NAROU_RELAY_ENV_FILE: absent() };
  const server = startRelay({ env });
  try {
    await once(server, "listening");
    assert.equal(server.address().address, "127.0.0.1");
    const response = await fetch(`http://127.0.0.1:${server.address().port}/health`);
    assert.equal(response.status, 200);
    assert.deepEqual(await response.json(), { ok: true, fetch: "curl,node" });
  } finally {
    await close(server);
  }
});

test("a bundled curl is copied, made executable and actually used", { timeout: 30_000 }, async (t) => {
  const found = spawnSync(process.platform === "win32" ? "where" : "which", ["curl"], { encoding: "utf8" });
  const systemCurl = found.status === 0 ? found.stdout.split(/\r?\n/)[0].trim() : "";
  if (!systemCurl) return t.skip("PATH に curl が無い");
  const dir = mkdtempSync(join(tmpdir(), "relay-bundled-"));
  const upstream = createServer((_req, res) => res.end("bundled curl body"));
  const upstreamPort = await new Promise((resolve) => { upstream.listen(0, "127.0.0.1", () => resolve(upstream.address().port)); });
  let prepared;
  let relay;
  try {
    const name = process.platform === "win32" ? "curl.exe" : "curl";
    copyFileSync(systemCurl, join(dir, name));
    chmodSync(join(dir, name), 0o755);
    const curl = resolveCurl({ env: {}, dir });
    prepared = curl.bin;
    assert.equal(curl.source, "bundled", "同梱 curl を実行可能な場所へ用意する");
    relay = createRelayServer({ token: TOKEN, resolveTarget: resolveLocalTarget, curl });
    const port = await new Promise((resolve) => { relay.listen(0, "127.0.0.1", () => resolve(relay.address().port)); });
    const response = await fetch(`http://127.0.0.1:${port}/proxy`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-proxy-token": TOKEN },
      body: JSON.stringify({ url: `http://relay.invalid:${upstreamPort}/` }),
    });
    const payload = await response.json();
    assert.equal(response.status, 200);
    assert.equal(payload.via, "curl", "Node 実装へ落ちずに同梱 curl を使う");
    assert.equal(Buffer.from(payload.body, "base64").toString(), "bundled curl body");
  } finally {
    if (relay) await close(relay);
    await new Promise((resolve) => upstream.close(resolve));
    if (prepared) rmSync(prepared, { force: true });
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the process entry reads .env and enforces its allowlist", { timeout: 20_000 }, async () => {
  const dir = mkdtempSync(join(tmpdir(), "relay-entry-"));
  const envFile = join(dir, ".env");
  const port = await freePort();
  writeFileSync(envFile, [
    `NAROU_RELAY_KEY=${TOKEN}`,
    "NAROU_RELAY_ALLOWED_HOSTS=example.com",
  ].join("\n"));
  const child = spawn(process.execPath, [join(relayDir, "server.mjs")], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, NAROU_RELAY_ENV_FILE: envFile, SERVER_PORT: String(port), K_SERVICE: "", NAROU_RELAY_CURL: "" },
  });
  let output = "";
  const receive = (chunk) => { output += chunk.toString(); };
  child.stdout.on("data", receive);
  child.stderr.on("data", receive);
  try {
    const deadline = Date.now() + 15_000;
    while (!output.includes("narou relay listening on 127.0.0.1:") && Date.now() < deadline) {
      if (child.exitCode !== null) break;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    assert.match(output, /を読み込みました/, "起動時に .env を読む");
    assert.equal(output.includes("example.com"), false, ".env の値そのものはログに出さない");
    assert.equal(output.includes(TOKEN), false, "鍵の値もログに出さない");
    assert.ok(output.includes("NAROU_RELAY_ALLOWED_HOSTS"), "読み込んだキー名だけを出す");
    assert.ok(output.includes(`narou relay listening on 127.0.0.1:${port}`), `待受ログが出ない: ${output}`);
    const healthy = await fetch(`http://127.0.0.1:${port}/health`);
    assert.equal(healthy.status, 200);
    const denied = await fetch(`http://127.0.0.1:${port}/proxy`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-proxy-token": TOKEN },
      body: JSON.stringify({ url: "http://not-allowed.invalid/" }),
    });
    assert.equal(denied.status, 400, ".env の許可リストが効いている");
  } finally {
    if (child.exitCode === null) child.kill();
    await once(child, "exit").catch(() => {});
    rmSync(dir, { recursive: true, force: true });
  }
});
