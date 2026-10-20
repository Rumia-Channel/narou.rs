// 静的 curl と CA バンドルの取得・検証。
//
// Cloud Run の起動時 (bootstrap.mjs) と SORAHOST 配備 (deploy-sorahost.mjs) が
// 同じ検証処理を使う。ピンは scripts/relay/pins.json に置く (Cloud Run の
// ビルド文脈は scripts/relay だけなので、その外のファイルは置けない)。
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export const CURL_ASSET = "curl-amd64";
export const PINS_FILE = "pins.json";

const moduleDir = dirname(fileURLToPath(import.meta.url));

export const sha256 = (buffer) => createHash("sha256").update(buffer).digest("hex");

export const downloadUrl = async (url) => {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) throw new Error(`${url} (HTTP ${response.status})`);
  return Buffer.from(await response.arrayBuffer());
};

// ピン留めした静的 curl の版とハッシュ。壊れた設定は起動時に落とす。
export const readCurlPins = (dir = moduleDir) => {
  const file = join(dir, PINS_FILE);
  let pins;
  try {
    pins = JSON.parse(readFileSync(file, "utf8"));
  } catch {
    throw new Error(`${file} を読めません`);
  }
  const version = String(pins.curl_version || "").trim();
  const hash = String(pins.curl_sha256 || "").trim().toLowerCase();
  if (!version) throw new Error(`${file} に curl_version がありません`);
  if (!/^[0-9a-f]{64}$/.test(hash)) throw new Error(`${file} に curl_sha256 がありません`);
  return { version, sha256: hash };
};

// 公開されている sha256sum.txt の行 (`<hash>  ./curl-amd64`) を読む。
export const publishedChecksum = (text, asset = CURL_ASSET) => {
  for (const line of String(text).split(/\r?\n/)) {
    const [hash, name] = line.trim().split(/\s+/);
    if (!hash || !name) continue;
    if (name.replace(/^\.\//, "").replace(/^\*/, "") === asset) return hash.toLowerCase();
  }
  return "";
};

// 取得した curl を、公開されているチェックサムとピンの両方で検証する。
export const fetchCurl = async ({ pins, download = downloadUrl } = {}) => {
  const base = `https://github.com/moparisthebest/static-curl/releases/download/${pins.version}`;
  const [asset, sums] = await Promise.all([
    download(`${base}/${CURL_ASSET}`),
    download(`${base}/sha256sum.txt`),
  ]);
  const published = publishedChecksum(String(sums));
  if (published !== pins.sha256) {
    throw new Error(`静的 curl の公開ハッシュがピンと一致しません (公開 ${published || "不明"} / ピン ${pins.sha256})`);
  }
  const actual = sha256(asset);
  if (actual !== pins.sha256) throw new Error(`静的 curl のハッシュが一致しません (期待 ${pins.sha256} / 実際 ${actual})`);
  return asset;
};

export const fetchCacert = async ({ download = downloadUrl } = {}) => {
  const buffer = await download("https://curl.se/ca/cacert.pem");
  if (!buffer.includes("BEGIN CERTIFICATE")) throw new Error("CA バンドルの内容が不正です");
  return buffer;
};
