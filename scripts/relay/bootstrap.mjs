// Cloud Run (Buildpacks) の起動エントリーポイント。
//
// Google の Node.js ランタイムイメージには curl が入っていないことがある。
// リレーは「同じ回線でも Node の TLS クライアントだけ Cloudflare に 403 にされる」
// 実測があるため curl を使いたいが、Cloud Run では PATH に無いと
// そのまま Node 実装 (viaPinnedHttp) へ落ちてしまう。
//
// そこで起動時に、ピン留めした SHA-256 で検証した静的 curl と CA バンドルを
// /tmp へ用意し、server.mjs へ NAROU_RELAY_CURL / NAROU_RELAY_CACERT として渡す。
// 用意できなかった場合は従来どおり Node 実装で続行する (起動は止めない)。
//
// curl が既に PATH にある場合や NAROU_RELAY_CURL が設定済みの場合は何もしない。
// ダウンロードはインスタンスの起動時に 1 回だけ (約 6.5 MB)。

import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { fetchCacert, fetchCurl, readCurlPins, CompromisedCaError } from "./fetch-curl.mjs";

const commandExists = (command) => {
  const probe = spawnSync(command, ["--version"], { stdio: "ignore", timeout: 5000, windowsHide: true });
  return !probe.error && probe.status === 0;
};

const defaultLog = (message) => console.log(`narou relay: ${message}`);

// `commandExists` / `download` はテストで差し替える。
export const prepareCurl = async ({
  env = process.env,
  pins,
  download,
  commandExists: hasCommand = commandExists,
  log = defaultLog,
} = {}) => {
  if (String(env.NAROU_RELAY_CURL ?? "").trim()) return { prepared: false, reason: "env" };
  if (hasCommand("curl")) return { prepared: false, reason: "path" };
  try {
    const resolvedPins = pins || readCurlPins();
    // transport.mjs は同梱 curl を `<tmpdir>/narou-relay-curl-<hash>` にコピーする
    // (ファイル)。こちらはディレクトリなので、名前に接尾辞を付けて衝突を避ける。
    const directory = join(tmpdir(), "narou-relay-curl-runtime", resolvedPins.sha256.slice(0, 16));
    mkdirSync(directory, { recursive: true });
    const curl = join(directory, "curl");
    const cacert = join(directory, "cacert.pem");
    writeFileSync(curl, await fetchCurl({ pins: resolvedPins, download }));
    chmodSync(curl, 0o755);
    writeFileSync(cacert, await fetchCacert({ download }));
    env.NAROU_RELAY_CURL = curl;
    env.NAROU_RELAY_CACERT = cacert;
    log(`静的 curl ${resolvedPins.version} を用意しました (${curl})`);
    return { prepared: true, reason: "downloaded", curl, cacert };
  } catch (error) {
    // 侵害されたルート CA を含むバンドルでの続行は、Node 実装でも同じ信頼を
    // 使うため危険。起動を止めて更新を促す。通信失敗などは従来どおり
    // Node 実装へフォールバックする。
    if (error instanceof CompromisedCaError) throw error;
    log(`curl を用意できませんでした (${error && error.message ? error.message : error})。Node 実装で続行します`);
    return { prepared: false, reason: "unavailable" };
  }
};

const main = async () => {
  await prepareCurl();
  const { startRelay } = await import("./server.mjs");
  startRelay();
};

const entry = process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href;
if (entry) {
  main().catch((error) => {
    console.error(`narou relay: 起動できません: ${error && error.message ? error.message : error}`);
    process.exitCode = 1;
  });
}
