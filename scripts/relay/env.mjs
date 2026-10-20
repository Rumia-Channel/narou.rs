// リレーの起動設定をホスト非依存に解決する。
//
// Cloud Run は環境変数 (PORT / K_SERVICE / Secret Manager) を渡す。
// PteWorker (SORAHOST) は sorahost.json に環境変数を渡す項目が無いため、
// ボリューム直下の .env を使う。どちらのホストでも同じ server.mjs が動くよう、
// ホストごとの差はこのファイルだけに閉じ込める。

import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export const moduleDir = dirname(fileURLToPath(import.meta.url));

// KEY=VALUE の行だけを読む。値は前後の空白を含めてそのまま使う
// (sorahost/start.sh と同じ規則)。値そのものはログへ出さない。
export const parseEnvFile = (text) => {
  const values = new Map();
  for (const line of String(text).split(/\r?\n/)) {
    if (!line || line.startsWith("#")) continue;
    const separator = line.indexOf("=");
    if (separator <= 0) continue;
    const key = line.slice(0, separator);
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) continue;
    values.set(key, line.slice(separator + 1));
  }
  return values;
};

// 既にある環境変数は上書きしない (プラットフォームが渡した値を .env が壊さない)。
// 空文字は未設定として扱う。
export const applyEnvFile = (env, path) => {
  const applied = [];
  for (const [key, value] of parseEnvFile(readFileSync(path, "utf8"))) {
    if (env[key]) continue;
    env[key] = value;
    applied.push(key);
  }
  return applied;
};

// 読み込み先: NAROU_RELAY_ENV_FILE → モジュールの隣 → HOME 直下。
// PteWorker は配備ごとにモジュールのディレクトリを作り直すので、実際に使うのは
// ボリューム直下 (= /home/container/.env) になる。最初に見つかった 1 つだけを読む。
export const loadEnvFile = ({ env = process.env, dir = moduleDir, home = homedir() } = {}) => {
  const explicit = String(env.NAROU_RELAY_ENV_FILE ?? "").trim();
  const candidates = explicit ? [explicit] : [join(dir, ".env"), join(home, ".env")];
  for (const path of candidates) {
    if (!existsSync(path)) continue;
    try {
      return { path, applied: applyEnvFile(env, path) };
    } catch {
      // 読めない .env で起動を止めない。鍵が無ければ起動時に明確に失敗する。
      return { path, applied: [], unreadable: true };
    }
  }
  return null;
};

const firstValue = (env, names) => {
  for (const name of names) {
    const value = String(env[name] ?? "").trim();
    if (value) return value;
  }
  return "";
};

// Cloud Run は PORT、PteWorker は SERVER_PORT を渡す (実機報告)。
// 両方あるときは Cloud Run の PORT を優先する。
export const resolvePort = (env = process.env) => {
  const port = Number(firstValue(env, ["PORT", "SERVER_PORT"]) || 3000);
  if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error("invalid PORT");
  return port;
};

// Cloud Run はコンテナ外から届くよう 0.0.0.0 で受ける必要があり、
// PteWorker はループバック束縛が要件 (公式 CLI README)。明示指定が最優先。
export const resolveHost = (env = process.env) => {
  const explicit = String(env.NAROU_RELAY_HOST ?? "").trim();
  if (explicit) return explicit;
  return env.K_SERVICE ? "0.0.0.0" : "127.0.0.1";
};
