// SORAHOST (Pterodactyl の Node.js egg) の起動エントリーポイント。
//
// SORAHOST 標準の起動コマンドは `[[ "${MAIN_FILE}" == "*.js" ]]` の右辺が
// 引用符付きのため常に偽になり、node ではなく ts-node 側へ落ちる:
//
//   /usr/local/bin/ts-node --esm "/home/container/${MAIN_FILE}"
//
// そのためこのファイルは **node でも ts-node でも実行できる構文**で書いている。
// 静的な import / require を使わず、async main の中だけで動的 import する。
// リレー本体は scripts/relay/server.mjs のまま。ここは準備と起動だけを行う。
//
// 流れ:
//   1. リポジトリのルートと /home/container/.env を確認する
//   2. scripts/relay/deploy-sorahost.mjs の prepareRuntime で relay-runtime/ を用意する
//      (実ファイルは再利用し、コードが変わったときだけ更新する)
//   3. relay-runtime/server.mjs を子プロセスで起動し、出力をそのままコンソールへ流す
//   4. SIGTERM / SIGINT を子プロセスへ伝えて終了する

const main = async () => {
  const { spawn } = await import("node:child_process");
  const { existsSync, readFileSync } = await import("node:fs");
  const { dirname, join, resolve } = await import("node:path");
  const { pathToFileURL } = await import("node:url");

  const log = (message) => console.log(`narou relay: ${message}`);
  const warn = (message) => console.error(`narou relay: ${message}`);

  // 実行位置からリポジトリのルート (scripts/relay/server.mjs を持つディレクトリ) を探す。
  const findRepoRoot = (starts) => {
    for (const start of starts) {
      let directory = start;
      for (let depth = 0; depth < 8; depth += 1) {
        if (existsSync(join(directory, "scripts", "relay", "server.mjs"))) return directory;
        const parent = dirname(directory);
        if (parent === directory) break;
        directory = parent;
      }
    }
    return "";
  };
  const starts = [];
  if (process.env.NAROU_RELAY_REPO) starts.push(resolve(process.env.NAROU_RELAY_REPO));
  starts.push(process.cwd());
  if (process.argv[1]) starts.push(dirname(resolve(process.argv[1])));
  const repoRoot = findRepoRoot(starts);
  if (!repoRoot) throw new Error("リポジトリのルートが見つかりません (scripts/relay/server.mjs がありません)");

  // 設定は /home/container/.env。優先順位 (環境変数 > .env) は env.mjs のまま変えない。
  const envFile = process.env.NAROU_RELAY_ENV_FILE
    ? resolve(process.env.NAROU_RELAY_ENV_FILE)
    : join(repoRoot, ".env");
  const hasEnvFile = existsSync(envFile);
  const envText = hasEnvFile ? readFileSync(envFile, "utf8") : "";
  if (!hasEnvFile) {
    if (!process.env.NAROU_RELAY_KEY) {
      throw new Error(`${envFile} がありません。NAROU_RELAY_KEY / NAROU_RELAY_ALLOWED_HOSTS / NAROU_RELAY_HOST=0.0.0.0 を書いてください`);
    }
    log(`${envFile} が無いため、環境変数の設定だけで起動します`);
  } else if (!process.env.NAROU_RELAY_HOST && !/^\s*NAROU_RELAY_HOST\s*=/mu.test(envText)) {
    warn(`警告: NAROU_RELAY_HOST が未設定です。Pterodactyl の Node.js egg では 0.0.0.0 が必要です (${envFile})`);
  }

  const prepUrl = pathToFileURL(join(repoRoot, "scripts", "relay", "deploy-sorahost.mjs")).href;
  let prepareRuntime;
  try {
    ({ prepareRuntime } = await import(prepUrl));
  } catch (error) {
    throw new Error(`配備スクリプトを読み込めません (${prepUrl}): ${error && error.message ? error.message : error}`);
  }
  if (typeof prepareRuntime !== "function") throw new Error("deploy-sorahost.mjs に prepareRuntime がありません");

  const runtimeDir = process.env.NAROU_RELAY_RUNTIME
    ? resolve(process.env.NAROU_RELAY_RUNTIME)
    : join(repoRoot, "relay-runtime");
  const prepared = await prepareRuntime({ repoRoot, runtimeDir, log });
  log(prepared.reused ? `実行環境は最新です (${runtimeDir})` : `実行環境を更新しました (${runtimeDir})`);

  // ポートは SERVER_PORT / PORT (Pterodactyl が渡す) を server.mjs が読む。
  // ここでは固定値にしない。
  const child = spawn(process.execPath, [join(runtimeDir, "server.mjs")], {
    cwd: runtimeDir,
    env: { ...process.env, NAROU_RELAY_ENV_FILE: envFile },
    stdio: "inherit",
  });

  let stopping = false;
  const stop = (signal) => {
    if (stopping) return;
    stopping = true;
    log(`${signal} を受信しました。リレーを停止します`);
    try { child.kill(signal); } catch { /* すでに終了している */ }
    const forced = setTimeout(() => {
      try { child.kill("SIGKILL"); } catch { /* すでに終了している */ }
      process.exit(0);
    }, 5000);
    forced.unref();
  };
  process.on("SIGINT", () => stop("SIGINT"));
  process.on("SIGTERM", () => stop("SIGTERM"));
  // 異常終了でも子プロセスを残さない。
  process.on("exit", () => {
    if (child.exitCode === null && child.signalCode === null) {
      try { child.kill("SIGKILL"); } catch { /* すでに終了している */ }
    }
  });

  child.on("error", (error) => {
    warn(`リレーを起動できません: ${error && error.message ? error.message : error}`);
    process.exit(1);
  });
  child.on("exit", (code, signal) => {
    if (stopping) process.exit(0);
    if (signal) {
      warn(`リレーが ${signal} で終了しました`);
      process.exit(1);
    }
    process.exit(code === null ? 1 : code);
  });
};

main().catch((error) => {
  console.error(`narou relay: 起動できません: ${error && error.message ? error.message : error}`);
  process.exitCode = 1;
});
