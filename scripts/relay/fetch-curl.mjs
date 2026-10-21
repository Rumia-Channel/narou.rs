// 静的 curl と CA バンドルの取得・検証。
//
// Cloud Run の起動時 (bootstrap.mjs) と SORAHOST 配備 (deploy-sorahost.mjs) が
// 同じ検証処理を使う。ピンは scripts/relay/pins.json に置く (Cloud Run の
// ビルド文脈は scripts/relay だけなので、その外のファイルは置けない)。
//
// CA バンドルは Mozilla 由来 (curl.se/ca/cacert.pem) を取得する。TLS の
// 検証はこのバンドルに完全に依存するため、既知の侵害されたルート証明書が
// 含まれていたら取得を失敗させる (fail closed)。
import { createHash, X509Certificate } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export const CURL_ASSET = "curl-amd64";
export const PINS_FILE = "pins.json";
export const CACERT_URL = "https://curl.se/ca/cacert.pem";

// 2026-10-20 に公表された eMudhra 中間 CA 侵害で失効したルート CA。
// 中間 CA の秘密鍵が証明書から復元可能だったため、これらを信頼するクライアントは
// 任意ホストへの証明書偽造を受け入れてしまう。ルート CA バンドルは中間 CA の
// 失効情報を持たないので、該当ルートを除外したバンドルだけを使う。
// SHA-256 は DER 証明書全体の指紋 (https://emudhra.we-have-a-problem.com/roots.json)。
export const COMPROMISED_CA_ROOTS = Object.freeze([
  { name: "AAA Certificate Services", sha256: "d7a7a0fb5d7e2731d771e9484ebcdef71d5f0c3e0a2948782bc83ee0ea699ef4" },
  { name: "emSign ECC Root CA - G3", sha256: "86a1ecba089c4a8d3bbe2734c612ba341d813e043cf9e8a862cd5c57a36bbe6b" },
  { name: "emSign Root TLS CA - G3", sha256: "7dd78d5f4f13459a83dff9abbba62edbaf6f2d102bf257fd712f4d9f2746ed8d" },
  { name: "emSign Root CS CA - G3", sha256: "f2edaaff3d9b701fb4d14c9f285f8c0bad90fc3bcc1fa07d47f4718edaba34a5" },
]);

export class CompromisedCaError extends Error {
  constructor(roots) {
    super(`CA バンドルに侵害されたルート証明書が含まれています: ${roots.join(", ")} — 更新された CA バンドルを用意してください`);
    this.name = "CompromisedCaError";
    this.roots = roots;
  }
}

const certificatePattern = /-----BEGIN CERTIFICATE-----[\s\S]*?-----END CERTIFICATE-----/g;

// CA バンドル (PEM) を解析し、既知の侵害ルートが含まれていないことを確かめる。
// 解析できない証明書があるバンドルも拒否する (検証をすり抜けさせない)。
export const verifyCacert = (buffer) => {
  const text = Buffer.isBuffer(buffer) ? buffer.toString("utf8") : String(buffer);
  const blocks = text.match(certificatePattern) || [];
  if (blocks.length === 0) throw new Error("CA バンドルに証明書がありません");
  const compromised = [];
  for (const pem of blocks) {
    let certificate;
    try {
      certificate = new X509Certificate(pem);
    } catch {
      throw new Error("CA バンドルに解析できない証明書があります");
    }
    const fingerprint = certificate.fingerprint256.replace(/:/g, "").toLowerCase();
    const root = COMPROMISED_CA_ROOTS.find((candidate) => candidate.sha256 === fingerprint);
    if (root) compromised.push(root.name);
  }
  if (compromised.length > 0) throw new CompromisedCaError(compromised);
  return { certificates: blocks.length };
};

export const verifyCacertFile = (file) => verifyCacert(readFileSync(file));

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
  const buffer = await download(CACERT_URL);
  // 侵害ルートを含むバンドルは使わない (CompromisedCaError は呼び出し側で致命的扱い)。
  verifyCacert(buffer);
  return buffer;
};
