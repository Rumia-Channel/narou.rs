// SORAHOST (PteWorker) へリレー単体を配備するためのフォルダーを組み立てる。
//
// Cloud Run は Buildpacks が npm start で server.mjs を起動するが、PteWorker は
// アップロードしたものをそのまま実行し、依存の取得もビルドもしない。さらに
// コンテナに curl が無く、配備先は読み取り専用で実行ビットも落ちるため、
// ハッシュ検証済みの静的 curl と CA バンドルを同梱する (transport.mjs が
// 書き込み可能な場所へコピーしてから実行する)。
//
// 使い方:
//   node scripts/relay/deploy-sorahost.mjs            組み立てて、次の手順を表示する
//   node scripts/relay/deploy-sorahost.mjs --deploy   組み立てて sorahost-cli deploy を実行する
//   node scripts/relay/deploy-sorahost.mjs --out DIR  出力先を固定する (残す)
//
// 認証情報は環境変数か .sorahost.json から CLI が読む (SORAHOST_ENDPOINT /
// SORAHOST_TOKEN)。サーバー側の NAROU_RELAY_KEY / NAROU_RELAY_ALLOWED_HOSTS は
// CLI では送れない (.env は常に除外される) ので、パネルのファイルマネージャか
// SFTP で /home/container/.env に置く。

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const relayDir = dirname(fileURLToPath(import.meta.url));
const toolsFile = join(relayDir, "..", "deploy-tools.json");
const MANIFEST = "sorahost.json";
const SOURCES = ["server.mjs", "env.mjs", "security.mjs", "transport.mjs", "worker/protocol.mjs"];
const CURL_ASSET = "curl-amd64";

const fail = (message) => {
  console.error(`error: ${message}`);
  process.exit(1);
};

const download = async (url) => {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) throw new Error(`${url} (HTTP ${response.status})`);
  return Buffer.from(await response.arrayBuffer());
};

// include に書き漏らしたファイルを配備してから気付かないよう、相対 import を
// 実際に解決して確かめる。
const localImports = (file) => {
  const text = readFileSync(file, "utf8");
  return [...text.matchAll(/from\s+"(\.\/[^"]+)"/g)].map((match) => match[1]);
};

const checkImports = (dir, entry) => {
  const seen = new Set();
  const queue = [entry];
  while (queue.length) {
    const relative = queue.pop();
    if (seen.has(relative)) continue;
    seen.add(relative);
    const file = join(dir, relative);
    if (!existsSync(file)) fail(`配備フォルダーに ${relative} がありません (include を確認してください)`);
    for (const specifier of localImports(file)) queue.push(specifier.replace(/^\.\//, ""));
  }
  return [...seen];
};

const sha256 = (buffer) => createHash("sha256").update(buffer).digest("hex");

const main = async () => {
  const { values } = parseArgs({
    options: {
      deploy: { type: "boolean", default: false },
      keep: { type: "boolean", default: false },
      out: { type: "string" },
      help: { type: "boolean", short: "h", default: false },
    },
    allowPositionals: false,
  });
  if (values.help) {
    console.log(readFileSync(fileURLToPath(import.meta.url), "utf8").split("\n").slice(0, 15).join("\n"));
    return;
  }

  const tools = JSON.parse(readFileSync(toolsFile, "utf8"));
  const tag = String(tools.curl_version || "").trim();
  if (!tag) fail(`${toolsFile} に curl_version がありません`);

  const directory = values.out ? resolve(values.out) : mkdtempSync(join(tmpdir(), "narou-relay-sorahost-"));
  const temporary = !values.out;
  try {
    mkdirSync(join(directory, "worker"), { recursive: true });
    for (const file of SOURCES) {
      if (!existsSync(join(relayDir, file))) fail(`${file} がありません`);
      copyFileSync(join(relayDir, file), join(directory, file));
    }
    copyFileSync(join(relayDir, MANIFEST), join(directory, MANIFEST));

    const base = `https://github.com/moparisthebest/static-curl/releases/download/${tag}`;
    process.stdout.write(`静的 curl ${tag} を取得しています… `);
    const [curl, sums, cacert] = await Promise.all([
      download(`${base}/${CURL_ASSET}`),
      download(`${base}/sha256sum.txt`),
      download("https://curl.se/ca/cacert.pem"),
    ]);
    const line = String(sums).split(/\r?\n/)
      .map((row) => row.trim().split(/\s+/))
      .find(([, name]) => name?.replace(/^\.\//, "").replace(/^\*/, "") === CURL_ASSET);
    const want = line ? line[0].toLowerCase() : "";
    const got = sha256(curl);
    if (!want || want !== got) fail(`静的 curl のハッシュが一致しません (期待 ${want || "不明"} / 実際 ${got})`);
    console.log(`ハッシュ検証 OK (${got.slice(0, 16)}…)`);

    writeFileSync(join(directory, "curl"), curl);
    chmodSync(join(directory, "curl"), 0o755);
    writeFileSync(join(directory, "cacert.pem"), cacert);

    const imported = checkImports(directory, "server.mjs");
    const files = [...new Set([...SOURCES, ...imported.map((file) => file.replace(/^\.\//, "")), MANIFEST, "curl", "cacert.pem"])];
    const total = files.reduce((sum, file) => sum + statSync(join(directory, file)).size, 0);
    console.log(`配備フォルダー: ${directory}`);
    console.log(`  ${files.join(", ")}`);
    console.log(`  ${(total / 1048576).toFixed(1)} MB (アップロード上限は既定 256 MB)`);

    if (values.deploy) {
      console.log("sorahost-cli deploy を実行します…");
      const result = spawnSync("npx", ["--yes", "sorahost-cli", "deploy"], { cwd: directory, stdio: "inherit", shell: process.platform === "win32" });
      if (result.status !== 0) fail("sorahost-cli deploy が失敗しました");
    } else {
      console.log("");
      console.log("次の手順 (PteWorker / sorahost-cli):");
      console.log(`  cd ${directory}`);
      console.log("  npx --yes sorahost-cli deploy");
      console.log("  (または --deploy を付けて実行する)");
      console.log("");
      console.log("Pterodactyl の Node.js egg (Nano / Micro など) は sorahost-cli を使わない:");
      console.log("  このフォルダーの中身を SFTP / ファイルマネージャで /home/container へ置き、");
      console.log("  起動コマンドを `cd /home/container && exec /usr/local/bin/node ./server.mjs` に変える");
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

await main();
