// relay.js (SORAHOST の Pterodactyl Node.js egg 用の起動エントリーポイント) の回帰。
//
// 実 SORAHOST / 実ネットワークは使わない。静的 curl のダウンロードだけを
// 差し替え、実行環境の準備・再利用・更新・失敗時の保護と、起動・停止・
// 失敗時の終了コードを確認する。
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { RUNTIME_MANIFEST, RUNTIME_SOURCES, prepareRuntime } from "../scripts/relay/deploy-sorahost.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const TOKEN = "relay-entry-test-key";
const FAKE_CURL = Buffer.from("#!/bin/sh\necho fake curl\n");
const FAKE_CA = Buffer.from("-----BEGIN CERTIFICATE-----\nFAKE\n-----END CERTIFICATE-----\n");
const sha256 = (buffer) => createHash("sha256").update(buffer).digest("hex");
const windows = process.platform === "win32";

const copySources = (repoRoot) => {
  mkdirSync(join(repoRoot, "scripts", "relay", "worker"), { recursive: true });
  for (const file of ["server.mjs", "env.mjs", "security.mjs", "transport.mjs", "deploy-sorahost.mjs", "fetch-curl.mjs"]) {
    copyFileSync(join(root, "scripts", "relay", file), join(repoRoot, "scripts", "relay", file));
  }
  copyFileSync(join(root, "scripts", "relay", "worker", "protocol.mjs"), join(repoRoot, "scripts", "relay", "worker", "protocol.mjs"));
  copyFileSync(join(root, "relay.js"), join(repoRoot, "relay.js"));
  writeFileSync(join(repoRoot, "scripts", "relay", "pins.json"), `${JSON.stringify({
    curl_version: "test-v1",
    curl_sha256: sha256(FAKE_CURL),
  }, null, 2)}\n`);
};

const cleanRepo = () => {
  const repoRoot = mkdtempSync(join(tmpdir(), "relay-entry-"));
  copySources(repoRoot);
  return repoRoot;
};

const fakeDownloader = () => {
  const calls = { curl: 0, checksum: 0, cacert: 0 };
  const download = async (url) => {
    if (url.endsWith("/curl-amd64")) { calls.curl += 1; return FAKE_CURL; }
    if (url.endsWith("/sha256sum.txt")) { calls.checksum += 1; return Buffer.from(`${sha256(FAKE_CURL)}  ./curl-amd64\n`); }
    if (url.endsWith("cacert.pem")) { calls.cacert += 1; return FAKE_CA; }
    throw new Error(`unexpected download: ${url}`);
  };
  return { download, calls };
};

const prepare = (repoRoot, downloader, extra = {}) => prepareRuntime({
  repoRoot,
  runtimeDir: join(repoRoot, "relay-runtime"),
  download: downloader.download,
  log: () => {},
  ...extra,
});

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

const stopTree = async (child) => {
  if (child.exitCode !== null || child.signalCode !== null) return;
  if (windows) {
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
  } else {
    child.kill("SIGTERM");
  }
  await Promise.race([
    new Promise((resolve) => child.once("exit", resolve)),
    new Promise((resolve) => setTimeout(resolve, 10_000)),
  ]);
};

test("prepareRuntime builds the runtime once and then reuses it", async () => {
  const repoRoot = cleanRepo();
  try {
    const downloader = fakeDownloader();
    const first = await prepare(repoRoot, downloader);
    assert.equal(first.reused, false);
    const runtime = first.runtimeDir;
    for (const relative of RUNTIME_SOURCES) assert.ok(existsSync(join(runtime, relative)), `${relative} が無い`);
    assert.ok(existsSync(join(runtime, RUNTIME_MANIFEST)));
    assert.ok(existsSync(join(runtime, "curl")));
    assert.ok(existsSync(join(runtime, "cacert.pem")));
    assert.equal(downloader.calls.curl, 1);
    assert.equal(downloader.calls.cacert, 1);
    const manifest = JSON.parse(readFileSync(join(runtime, RUNTIME_MANIFEST), "utf8"));
    assert.equal(manifest.files.curl, sha256(FAKE_CURL));
    assert.equal(manifest.files["cacert.pem"], sha256(FAKE_CA));
    if (!windows) assert.equal(statSync(join(runtime, "curl")).mode & 0o111, 0o111, "実行ビットを付ける");

    const second = await prepare(repoRoot, downloader);
    assert.equal(second.reused, true);
    assert.equal(downloader.calls.curl, 1, "再利用時にダウンロードしない");
    assert.equal(downloader.calls.cacert, 1);
  } finally {
    rmSync(repoRoot, { recursive: true, force: true });
  }
});

test("a source change refreshes the code but keeps the verified curl", async () => {
  const repoRoot = cleanRepo();
  try {
    const downloader = fakeDownloader();
    const first = await prepare(repoRoot, downloader);
    const runtime = first.runtimeDir;
    const source = join(repoRoot, "scripts", "relay", "env.mjs");
    writeFileSync(source, `${readFileSync(source, "utf8")}\n// changed\n`);
    const second = await prepare(repoRoot, downloader);
    assert.equal(second.reused, false);
    assert.ok(readFileSync(join(runtime, "env.mjs"), "utf8").includes("// changed"));
    assert.equal(downloader.calls.curl, 1, "ハッシュが一致する curl は再取得しない");
  } finally {
    rmSync(repoRoot, { recursive: true, force: true });
  }
});

test("a corrupted curl is replaced only after hash verification", async () => {
  const repoRoot = cleanRepo();
  try {
    const downloader = fakeDownloader();
    const first = await prepare(repoRoot, downloader);
    const runtime = first.runtimeDir;
    writeFileSync(join(runtime, "curl"), "corrupted\n");
    const second = await prepare(repoRoot, downloader);
    assert.equal(second.reused, false);
    assert.equal(downloader.calls.curl, 2);
    assert.equal(sha256(readFileSync(join(runtime, "curl"))), sha256(FAKE_CURL));
  } finally {
    rmSync(repoRoot, { recursive: true, force: true });
  }
});

test("a failed preparation keeps the previous runtime usable", async () => {
  const repoRoot = cleanRepo();
  try {
    const downloader = fakeDownloader();
    const first = await prepare(repoRoot, downloader);
    const runtime = first.runtimeDir;
    writeFileSync(join(runtime, "curl"), "corrupted\n");
    // 壊れた curl を直すためのダウンロードを失敗させる。
    downloader.download = async (url) => {
      if (url.endsWith("/curl-amd64")) throw new Error("network down");
      return fakeDownloader().download(url);
    };
    await assert.rejects(() => prepare(repoRoot, downloader), /network down/);
    assert.ok(existsSync(join(runtime, "server.mjs")), "前の実行環境が残っている");
    assert.ok(existsSync(join(runtime, RUNTIME_MANIFEST)), "前のマニフェストが残っている");
    assert.equal(readFileSync(join(runtime, "curl"), "utf8"), "corrupted\n", "壊れたまま放置せず差し替えもしない");
  } finally {
    rmSync(repoRoot, { recursive: true, force: true });
  }
});

test("relay.js starts server.mjs for the prepared runtime", { timeout: 60_000 }, async () => {
  const repoRoot = cleanRepo();
  let child;
  try {
    const downloader = fakeDownloader();
    const runtime = (await prepare(repoRoot, downloader)).runtimeDir;
    const port = await freePort();
    writeFileSync(join(repoRoot, ".env"), [
      `NAROU_RELAY_KEY=${TOKEN}`,
      "NAROU_RELAY_ALLOWED_HOSTS=example.com",
      "NAROU_RELAY_HOST=0.0.0.0",
    ].join("\n"));

    let output = "";
    child = spawn(process.execPath, [join(repoRoot, "relay.js")], {
      cwd: repoRoot,
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        SERVER_PORT: String(port),
        NAROU_RELAY_ENV_FILE: "",
        NAROU_RELAY_REPO: "",
        NAROU_RELAY_RUNTIME: "",
        NAROU_RELAY_KEY: "",
        NAROU_RELAY_ALLOWED_HOSTS: "",
        NAROU_RELAY_HOST: "",
      },
    });
    child.stdout.on("data", (chunk) => { output += chunk.toString(); });
    child.stderr.on("data", (chunk) => { output += chunk.toString(); });

    await waitFor(() => output.includes(`listening on 0.0.0.0:${port}`), `待受ログ (${output.slice(-400)})`);
    assert.equal(downloader.calls.curl, 1, "準備済みの実行環境を再利用している");

    const health = await fetch(`http://127.0.0.1:${port}/health`);
    assert.equal(health.status, 200);
    assert.deepEqual(await health.json(), { ok: true });

    const denied = await fetch(`http://127.0.0.1:${port}/proxy`, { method: "POST" });
    assert.equal(denied.status, 403, "キー無しの /proxy は 403");

    if (!windows) {
      child.kill("SIGINT");
      const [code] = await Promise.race([
        new Promise((resolve) => child.once("exit", (...args) => resolve(args))),
        new Promise((resolve) => setTimeout(() => resolve(["timeout"]), 10_000)),
      ]);
      assert.equal(code, 0, `SIGINT で正常終了する (${output.slice(-400)})`);
      // 子プロセスが残っていないこと (ポートが解放されていること) を確かめる。
      const probe = await fetch(`http://127.0.0.1:${port}/health`).then(() => "alive", () => "closed");
      assert.equal(probe, "closed", "リレーが残っていない");
    }
  } finally {
    if (child) await stopTree(child);
    rmSync(repoRoot, { recursive: true, force: true });
  }
});

test("relay.js reports a preparation failure with a non-zero exit", { timeout: 30_000 }, async () => {
  const repoRoot = cleanRepo();
  try {
    writeFileSync(join(repoRoot, ".env"), `NAROU_RELAY_KEY=${TOKEN}\nNAROU_RELAY_ALLOWED_HOSTS=example.com\n`);
    // ピンが無い pins.json は prepareRuntime が拒否する。
    writeFileSync(join(repoRoot, "scripts", "relay", "pins.json"), '{"curl_version":"test-v1"}\n');
    const child = spawn(process.execPath, [join(repoRoot, "relay.js")], {
      cwd: repoRoot,
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, NAROU_RELAY_ENV_FILE: "", NAROU_RELAY_REPO: "", NAROU_RELAY_RUNTIME: "" },
    });
    let output = "";
    child.stdout.on("data", (chunk) => { output += chunk.toString(); });
    child.stderr.on("data", (chunk) => { output += chunk.toString(); });
    const [code] = await Promise.race([
      new Promise((resolve) => child.once("exit", (...args) => resolve(args))),
      new Promise((resolve) => setTimeout(() => resolve(["timeout"]), 15_000)),
    ]);
    assert.equal(code, 1, "準備の失敗は非ゼロで終了する");
    assert.match(output, /curl_sha256/);
    assert.equal(existsSync(join(repoRoot, "relay-runtime")), false, "失敗した準備は残さない");
  } finally {
    rmSync(repoRoot, { recursive: true, force: true });
  }
});

test("relay.js fails fast without .env and without a key", { timeout: 30_000 }, async () => {
  const repoRoot = cleanRepo();
  try {
    const child = spawn(process.execPath, [join(repoRoot, "relay.js")], {
      cwd: repoRoot,
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, NAROU_RELAY_KEY: "", NAROU_RELAY_ENV_FILE: "", NAROU_RELAY_REPO: "", NAROU_RELAY_RUNTIME: "" },
    });
    let output = "";
    child.stdout.on("data", (chunk) => { output += chunk.toString(); });
    child.stderr.on("data", (chunk) => { output += chunk.toString(); });
    const [code] = await Promise.race([
      new Promise((resolve) => child.once("exit", (...args) => resolve(args))),
      new Promise((resolve) => setTimeout(() => resolve(["timeout"]), 15_000)),
    ]);
    assert.equal(code, 1, "非ゼロで終了する");
    assert.match(output, /\.env/);
    assert.equal(existsSync(join(repoRoot, "relay-runtime")), false, "設定不足では実行環境を作らない");
  } finally {
    rmSync(repoRoot, { recursive: true, force: true });
  }
});
