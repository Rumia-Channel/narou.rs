// SORAHOST のリレー配備・起動を支える。
//
// 使い方は 2 つある。
//
// 1. PteWorker (`sorahost-cli`) や手動アップロード用のフォルダーを組み立てる (このファイルを直接実行)
//      node scripts/relay/deploy-sorahost.mjs
//      node scripts/relay/deploy-sorahost.mjs --deploy
//      node scripts/relay/deploy-sorahost.mjs --out DIR
// 2. Pterodactyl の Node.js egg で relay.js が呼ぶ実行環境を準備する (`prepareRuntime`)
//      node scripts/relay/deploy-sorahost.mjs --runtime DIR
//
// PteWorker はアップロードしたものをそのまま実行し、依存の取得もビルドもしない。
// コンテナに curl が無く、配備先は読み取り専用で実行ビットも落ちるため、
// ハッシュ検証済みの静的 curl と CA バンドルを同梱する (transport.mjs が
// 書き込み可能な場所へコピーしてから実行する)。
//
// `prepareRuntime` は実行環境をディレクトリごと作り直す。既に検証済みの
// ファイルは再利用し、コードが変わったときだけ更新する。準備は一時ディレクトリで
// 行い、成功したときだけ切り替えるので、途中で失敗しても前の環境は壊れない。

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, statSync, writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

const relayDir = dirname(fileURLToPath(import.meta.url));
export const repoRootDefault = resolve(relayDir, "..", "..");

export const RUNTIME_MANIFEST = "runtime.json";
export const CURL_ASSET = "curl-amd64";
export const RUNTIME_SOURCES = ["server.mjs", "env.mjs", "security.mjs", "transport.mjs", "worker/protocol.mjs"];
const SORAHOST_MANIFEST = "sorahost.json";

const sha256 = (buffer) => createHash("sha256").update(buffer).digest("hex");
const fileSha256 = (path) => sha256(readFileSync(path));
const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));

const downloadUrl = async (url) => {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) throw new Error(`${url} (HTTP ${response.status})`);
  return Buffer.from(await response.arrayBuffer());
};

export const readPins = (repoRoot = repoRootDefault) => {
  const toolsFile = join(repoRoot, "scripts", "deploy-tools.json");
  let tools;
  try {
    tools = readJson(toolsFile);
  } catch {
    throw new Error(`${toolsFile} を読めません`);
  }
  const version = String(tools.curl_version || "").trim();
  const hash = String(tools.curl_sha256 || "").trim().toLowerCase();
  if (!version) throw new Error(`${toolsFile} に curl_version がありません`);
  if (!/^[0-9a-f]{64}$/.test(hash)) throw new Error(`${toolsFile} に curl_sha256 がありません`);
  return { version, sha256: hash };
};

// 公開されている sha256sum.txt の行 (`<hash>  ./curl-amd64`) を読む。
const publishedChecksum = (text, asset) => {
  for (const line of String(text).split(/\r?\n/)) {
    const [hash, name] = line.trim().split(/\s+/);
    if (!hash || !name) continue;
    if (name.replace(/^\.\//, "").replace(/^\*/, "") === asset) return hash.toLowerCase();
  }
  return "";
};

const fetchCurl = async ({ pins, download }) => {
  const base = `https://github.com/moparisthebest/static-curl/releases/download/${pins.version}`;
  const [asset, sums] = await Promise.all([download(`${base}/${CURL_ASSET}`), download(`${base}/sha256sum.txt`)]);
  const published = publishedChecksum(String(sums), CURL_ASSET);
  if (published !== pins.sha256) {
    throw new Error(`静的 curl の公開ハッシュがピンと一致しません (公開 ${published || "不明"} / ピン ${pins.sha256})`);
  }
  const actual = sha256(asset);
  if (actual !== pins.sha256) throw new Error(`静的 curl のハッシュが一致しません (期待 ${pins.sha256} / 実際 ${actual})`);
  return asset;
};

const fetchCacert = async ({ download }) => {
  const buffer = await download("https://curl.se/ca/cacert.pem");
  if (!buffer.includes("BEGIN CERTIFICATE")) throw new Error("CA バンドルの内容が不正です");
  return buffer;
};

const readRuntimeManifest = (directory) => {
  try {
    const manifest = readJson(join(directory, RUNTIME_MANIFEST));
    return manifest && manifest.files && typeof manifest.files === "object" ? manifest : null;
  } catch {
    return null;
  }
};

// リポジトリのリレー本体から、実行環境に入れるべきファイルのハッシュを作る。
export const sourceHashes = (repoRoot = repoRootDefault) => {
  const sourceDir = join(repoRoot, "scripts", "relay");
  const hashes = {};
  for (const relative of RUNTIME_SOURCES) {
    const path = join(sourceDir, relative);
    if (!existsSync(path)) throw new Error(`${path} がありません`);
    hashes[relative] = fileSha256(path);
  }
  return hashes;
};

// 相対 import の解決漏れを配備前に検出する。
const checkImports = (directory, entry) => {
  const localImports = (file) => {
    const text = readFileSync(file, "utf8");
    return [...text.matchAll(/from\s+"(\.\/[^"]+)"/g)].map((match) => match[1]);
  };
  const seen = new Set();
  const queue = [entry];
  while (queue.length) {
    const relative = queue.pop();
    if (seen.has(relative)) continue;
    seen.add(relative);
    const file = join(directory, relative);
    if (!existsSync(file)) throw new Error(`配備フォルダーに ${relative} がありません (include を確認してください)`);
    for (const specifier of localImports(file)) queue.push(specifier.replace(/^\.\//, ""));
  }
  return [...seen];
};

// 既存の実行環境がそのまま使えるか。コードはリポジトリと一致し、curl は
// ピン留めしたハッシュ、CA は前回のマニフェストと一致することを要求する。
const inspectRuntime = (directory, { code, pins }) => {
  if (!existsSync(directory)) return false;
  const manifest = readRuntimeManifest(directory);
  if (!manifest) return false;
  for (const [relative, hash] of Object.entries(code)) {
    const path = join(directory, relative);
    if (!existsSync(path) || fileSha256(path) !== hash) return false;
  }
  const curl = join(directory, "curl");
  if (!existsSync(curl) || fileSha256(curl) !== pins.sha256) return false;
  const cacert = join(directory, "cacert.pem");
  const cacertHash = manifest.files["cacert.pem"];
  if (typeof cacertHash !== "string" || !existsSync(cacert) || fileSha256(cacert) !== cacertHash) return false;
  return true;
};

const buildRuntime = async ({ repoRoot, directory, code, pins, download, log, existing, writeManifest = true }) => {
  const sourceDir = join(repoRoot, "scripts", "relay");
  mkdirSync(join(directory, "worker"), { recursive: true });
  for (const relative of RUNTIME_SOURCES) {
    copyFileSync(join(sourceDir, relative), join(directory, relative));
  }

  // curl: 既存がピンと一致していれば再ダウンロードしない。
  const existingCurl = existing ? join(existing, "curl") : "";
  if (existingCurl && existsSync(existingCurl) && fileSha256(existingCurl) === pins.sha256) {
    copyFileSync(existingCurl, join(directory, "curl"));
  } else {
    log(`静的 curl ${pins.version} を取得しています…`);
    writeFileSync(join(directory, "curl"), await fetchCurl({ pins, download }));
  }
  chmodSync(join(directory, "curl"), 0o755);

  // CA: 前回のマニフェストでハッシュを確認できれば再利用する。
  const existingCacert = existing ? join(existing, "cacert.pem") : "";
  const existingManifest = existing ? readRuntimeManifest(existing) : null;
  const reusableCacert = existingCacert && existingManifest
    && typeof existingManifest.files["cacert.pem"] === "string"
    && existsSync(existingCacert)
    && fileSha256(existingCacert) === existingManifest.files["cacert.pem"];
  if (reusableCacert) {
    copyFileSync(existingCacert, join(directory, "cacert.pem"));
  } else {
    log("CA バンドルを取得しています…");
    writeFileSync(join(directory, "cacert.pem"), await fetchCacert({ download }));
  }

  const files = {
    ...code,
    curl: fileSha256(join(directory, "curl")),
    "cacert.pem": fileSha256(join(directory, "cacert.pem")),
  };
  if (writeManifest) {
    writeFileSync(join(directory, RUNTIME_MANIFEST), `${JSON.stringify({
      version: 1,
      generatedAt: new Date().toISOString(),
      curlVersion: pins.version,
      files,
    }, null, 2)}\n`);
  }
  checkImports(directory, "server.mjs");
  return files;
};

// 準備中のディレクトリを本番の場所へ切り替える。前の環境は成功するまで残す。
const swapDirectory = (source, target) => {
  const previous = `${target}.old-${process.pid}-${Date.now()}`;
  if (existsSync(target)) renameSync(target, previous);
  try {
    renameSync(source, target);
  } catch (error) {
    if (existsSync(previous) && !existsSync(target)) renameSync(previous, target);
    throw error;
  }
  if (existsSync(previous)) rmSync(previous, { recursive: true, force: true });
};

// relay.js が毎回呼ぶ。検証済みなら何もせず、更新が必要なときだけ作り直す。
export const prepareRuntime = async ({
  repoRoot = repoRootDefault,
  runtimeDir,
  pins,
  download = downloadUrl,
  log = () => {},
} = {}) => {
  const root = resolve(repoRoot);
  const target = resolve(runtimeDir || join(root, "relay-runtime"));
  const resolvedPins = pins || readPins(root);
  const code = sourceHashes(root);
  if (inspectRuntime(target, { code, pins: resolvedPins })) {
    return { runtimeDir: target, reused: true };
  }
  const temporary = `${target}.tmp-${process.pid}-${Date.now()}`;
  log(`実行環境を準備しています (${target})`);
  try {
    mkdirSync(temporary, { recursive: true });
    await buildRuntime({
      repoRoot: root, directory: temporary, code, pins: resolvedPins, download, log,
      existing: existsSync(target) ? target : null,
    });
    swapDirectory(temporary, target);
  } catch (error) {
    rmSync(temporary, { recursive: true, force: true });
    throw error;
  }
  return { runtimeDir: target, reused: false };
};

const main = async () => {
  const { values } = parseArgs({
    options: {
      deploy: { type: "boolean", default: false },
      keep: { type: "boolean", default: false },
      out: { type: "string" },
      runtime: { type: "string" },
      help: { type: "boolean", short: "h", default: false },
    },
    allowPositionals: false,
  });
  if (values.help) {
    console.log(readFileSync(fileURLToPath(import.meta.url), "utf8").split("\n").slice(0, 17).join("\n"));
    return;
  }

  if (values.runtime) {
    const prepared = await prepareRuntime({ runtimeDir: resolve(values.runtime), log: (line) => console.log(line) });
    console.log(`実行環境: ${prepared.runtimeDir} (${prepared.reused ? "再利用" : "更新"})`);
    return;
  }

  const pins = readPins(repoRootDefault);
  const code = sourceHashes(repoRootDefault);
  const directory = values.out ? resolve(values.out) : mkdtempSync(join(tmpdir(), "narou-relay-sorahost-"));
  const temporary = !values.out;
  try {
    const files = await buildRuntime({
      repoRoot: repoRootDefault, directory, code, pins, download: downloadUrl,
      log: (line) => process.stdout.write(`${line} `), existing: null, writeManifest: false,
    });
    copyFileSync(join(relayDir, SORAHOST_MANIFEST), join(directory, SORAHOST_MANIFEST));
    const listed = [...new Set([...RUNTIME_SOURCES, SORAHOST_MANIFEST, "curl", "cacert.pem"])];
    const total = listed.reduce((sum, file) => sum + statSync(join(directory, file)).size, 0);
    console.log("");
    console.log(`配備フォルダー: ${directory}`);
    console.log(`  ${listed.join(", ")}`);
    console.log(`  ${(total / 1048576).toFixed(1)} MB (アップロード上限は既定 256 MB)`);
    console.log(`  (ハッシュ: curl ${files.curl.slice(0, 16)}… / ca ${files["cacert.pem"].slice(0, 16)}…)`);

    if (values.deploy) {
      console.log("sorahost-cli deploy を実行します…");
      const result = spawnSync("npx", ["--yes", "sorahost-cli", "deploy"], { cwd: directory, stdio: "inherit", shell: process.platform === "win32" });
      if (result.status !== 0) throw new Error("sorahost-cli deploy が失敗しました");
    } else {
      console.log("");
      console.log("次の手順 (PteWorker / sorahost-cli):");
      console.log(`  cd ${directory}`);
      console.log("  npx --yes sorahost-cli deploy");
      console.log("  (または --deploy を付けて実行する)");
      console.log("");
      console.log("Pterodactyl の Node.js egg (Nano / Micro など) は sorahost-cli を使わない:");
      console.log("  リポジトリを clone し、MAIN FILE を relay.js にして再起動する (relay.js が実行環境を用意する)");
      console.log("  https://github.com/Rumia-Channel/narou.rs の docs/sorahost-relay.md §4.2 を参照");
      console.log("");
      console.log("配備後、パネルのファイルマネージャか SFTP で /home/container/.env に置く:");
      console.log("  NAROU_RELAY_KEY=<Worker の NAROU_RELAY_KEY と同じ値>");
      console.log("  NAROU_RELAY_ALLOWED_HOSTS=<取得先ホストを完全一致のカンマ区切りで>");
      console.log("  NAROU_RELAY_HOST=0.0.0.0   # Node.js egg のときだけ必要");
      console.log("確認: GET <公開 URL>/health → {\"ok\":true} / python3 scripts/relay/verify_auth.py");
    }
    if (temporary && (values.keep || values.deploy)) console.log(`(残したフォルダー: ${directory})`);
  } finally {
    if (temporary && !values.keep && !values.deploy) rmSync(directory, { recursive: true, force: true });
  }
};

const entry = process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href;
if (entry) {
  await main().catch((error) => {
    console.error(`error: ${error && error.message ? error.message : error}`);
    process.exitCode = 1;
  });
}
