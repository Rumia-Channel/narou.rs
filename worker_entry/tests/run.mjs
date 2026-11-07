// ローカルで Worker をビルド・起動して契約テストを回す。
//
//   node tests/run.mjs            # debug build
//   node tests/run.mjs --release  # CI と同じ release build
//   node tests/run.mjs --keep     # 終了後も wrangler dev を残す
//
// 実行ごとに .wrangler/contract-* に設定・合成資格情報・D1 を隔離する。
// 開発用 .dev.vars と .wrangler/state は読み書きしない。
// --keep を指定しない限り、失敗時もプロセスと隔離ディレクトリを削除する。
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { createServer } from "node:http";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import process from "node:process";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const release = process.argv.includes("--release");
const keep = process.argv.includes("--keep");
const port = process.env.PORT ?? "8787";
const token = process.env.NAROU_ADMIN_TOKEN ?? "local-contract-token";
const isWindows = process.platform === "win32";
const npx = isWindows ? "npx.cmd" : "npx";

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: root,
    stdio: "inherit",
    shell: isWindows,
    ...options,
  });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with ${result.status}`);
  }
}

async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, "exit");
  if (isWindows) {
    spawnSync("taskkill", ["/pid", String(child.pid), "/T", "/F"], { stdio: "ignore" });
  } else {
    // npx owns Wrangler/workerd descendants; stop the complete process group.
    try {
      process.kill(-child.pid, "SIGTERM");
    } catch (error) {
      if (error.code !== "ESRCH") throw error;
    }
  }
  await exited;
}

mkdirSync(join(root, ".wrangler"), { recursive: true });
const work = mkdtempSync(join(root, ".wrangler", "contract-"));
const persist = join(work, "state");
const testConfig = join(work, "wrangler.toml");
let dev;
let exitCode = 1;
try {
// 1. 開発者の資格情報を読まず、隔離した設定だけに合成値を置く。
const devVars = join(work, ".dev.vars");
const accessClientId = "contract-access-client-id.access";
const accessClientSecret = "contract-access-client-secret";
const loginKey = Buffer.alloc(32, 7).toString("base64");
writeFileSync(
  devVars,
  `NAROU_ADMIN_TOKEN=${token}\nNAROU_RS_LOGIN_KEY=${loginKey}\n` +
    `NAROU_CF_ACCESS_CLIENT_ID=${accessClientId}\n` +
    `NAROU_CF_ACCESS_CLIENT_SECRET=${accessClientSecret}\n`,
);

// 2. 実行用の設定。ビルドはこのスクリプトが行うので [build] を外す
//    (wrangler dev が毎回 release ビルドを走らせないようにするため)。
//    deploy CI と同じく実行する wrangler は固定バージョンにする。
const WRANGLER_VERSION = process.env.NAROU_WRANGLER_VERSION || "4.151.3";
if (!/^\d+\.\d+\.\d+$/.test(WRANGLER_VERSION)) {
  throw new Error(`NAROU_WRANGLER_VERSION must be exact x.y.z: ${WRANGLER_VERSION}`);
}
const WRANGLER_PACKAGE = `wrangler@${WRANGLER_VERSION}`;
const config = readFileSync(join(root, "wrangler.toml"), "utf8")
  .replace(/\[build\][^\[]*/, "")
  .replace(/^main = .*$/m, `main = ${JSON.stringify(join(here, "virtual_race_worker.mjs").replaceAll("\\", "/"))}`)
  .replace(/^directory = .*$/m, `directory = ${JSON.stringify(join(root, "public").replaceAll("\\", "/"))}`)
  .replace(/^migrations_dir = .*$/m, `migrations_dir = ${JSON.stringify(join(root, "migrations").replaceAll("\\", "/"))}`);
writeFileSync(testConfig, config);
console.log(`wrote ${testConfig}`);

// 3. ビルド (設定から [build] を外しているのでアセットはここで作る)。
run("node", ["build_assets.mjs"]);
run("worker-build", release ? ["--release"] : []);

// 4. ローカル D1 にマイグレーションを適用。
run(npx, [
  "--yes",
  WRANGLER_PACKAGE,
  "d1",
  "migrations",
  "apply",
  "DB",
  "--local",
  "-c",
  testConfig,
  "--persist-to",
  persist,
]);

// This runner explicitly exercises legacy D1; content_storage.mjs covers S3 defaults.
const legacyContentFixture = join(work, "contract-content-backend.sql");
writeFileSync(legacyContentFixture, "INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','content_backend','\"d1\"','d1') ON CONFLICT(scope,key) DO NOTHING;");
run(npx, [
  "--yes", WRANGLER_PACKAGE, "d1", "execute", "DB", "--local",
  "-c", testConfig, "--persist-to", persist, "--file", legacyContentFixture,
]);

// 5. wrangler dev を起動して readiness を待つ。
dev = spawn(
  npx,
  [
    "--yes",
    WRANGLER_PACKAGE,
    "dev",
    "-c",
    testConfig,
    "--persist-to",
    persist,
    "--port",
    port,
    "--test-scheduled",
  ],
  { cwd: root, stdio: "inherit", shell: isWindows, detached: !isWindows },
);

  const base = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + 120_000;
  for (;;) {
    try {
      const response = await fetch(`${base}/health/live`);
      if (response.status === 200) break;
    } catch {
      // まだ起動していない
    }
    if (Date.now() > deadline) throw new Error("wrangler dev did not become ready");
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
  console.log(`wrangler dev is ready on ${base}`);

  // 6. 契約テスト本体（`node --test`）。
  const runContract = (extraEnv) =>
    spawnSync(process.execPath, ["--test", join(here, "contract.mjs")], {
      cwd: root,
      stdio: "inherit",
      env: { ...process.env, BASE_URL: base, NAROU_ADMIN_TOKEN: token, ...extraEnv },
    });
  const contract = runContract({
    CONTRACT_QUEUE: process.env.CONTRACT_QUEUE ?? "1",
    // Worker 側の Access 検査に掛からないよう、Worker と同じ資格情報を渡す
    // (ローカルでは .dev.vars に書いたテスト用の値)。
    CF_ACCESS_CLIENT_ID: accessClientId,
    CF_ACCESS_CLIENT_SECRET: accessClientSecret,
  });
  exitCode = contract.status ?? 1;

  // タグの登録順 (連番表) は作品と同じ batch で確定する。
  if (exitCode === 0) {
    const tagSequence = spawnSync(process.execPath, [join(here, "tag_sequence.mjs")], {
      cwd: root, stdio: "inherit",
      env: {
        ...process.env, BASE_URL: base, NAROU_ADMIN_TOKEN: token,
        NAROU_TEST_CONFIG: testConfig, NAROU_TEST_PERSIST: persist,
        NAROU_WRANGLER_VERSION: WRANGLER_VERSION,
      },
    });
    exitCode = tagSequence.status ?? 1;
  }
  for (const name of ["virtual_attributes.mjs", "virtual_membership.mjs", "virtual_updates.mjs"]) {
    if (exitCode !== 0) break;
    const virtual = spawnSync(process.execPath, [join(here, name)], {
      cwd: root, stdio: "inherit",
      env: {
        ...process.env, BASE_URL: base, NAROU_ADMIN_TOKEN: token,
        NAROU_TEST_CONFIG: testConfig, NAROU_TEST_PERSIST: persist,
        NAROU_WRANGLER_VERSION: WRANGLER_VERSION,
      },
    });
    exitCode = virtual.status ?? 1;
  }

  // 7. トークンを外した構成で fail-closed を確かめる（任意・既定 off）。
  if (exitCode === 0 && process.env.NAROU_CHECK_UNCONFIGURED === "1") {
    console.log("checking the unconfigured Worker (no admin token)");
    const probe = createServer().listen(0, "127.0.0.1");
    await once(probe, "listening");
    const authPort = String(probe.address().port);
    await new Promise(resolve => probe.close(resolve));
    const authRoot = join(work, "unconfigured");
    mkdirSync(authRoot);
    const authConfig = join(authRoot, "wrangler.toml");
    writeFileSync(authConfig, readFileSync(testConfig));
    const authPid = spawn(
      npx,
      [
        "--yes",
        WRANGLER_PACKAGE,
        "dev",
        "-c",
        authConfig,
        "--persist-to",
        persist,
        "--port",
        authPort,
        "--var",
        "NAROU_AUTH_REQUIRED:true",
      ],
      { cwd: root, stdio: "inherit", shell: isWindows, detached: !isWindows },
    );
    try {
      const unconfiguredBase = `http://127.0.0.1:${authPort}`;
      const deadline = Date.now() + 120_000;
      for (;;) {
        try {
          const response = await fetch(`${unconfiguredBase}/health/live`);
          if (response.status === 200) break;
        } catch {}
        if (Date.now() > deadline) throw new Error("second wrangler dev did not become ready");
        await new Promise((resolve) => setTimeout(resolve, 1000));
      }
      const unconfigured = spawnSync(
        process.execPath,
        ["--test", join(here, "contract.mjs")],
        {
          cwd: root,
          stdio: "inherit",
          env: {
            ...process.env,
            BASE_URL: unconfiguredBase,
            NAROU_ADMIN_TOKEN: "",
            CONTRACT_EXPECT_UNCONFIGURED: "1",
          },
        },
      );
      if ((unconfigured.status ?? 1) !== 0) exitCode = unconfigured.status ?? 1;
    } finally {
      await stop(authPid);
    }
  }
} finally {
  if (!keep || !dev) {
    await stop(dev);
    rmSync(work, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
  } else {
    console.log(`kept wrangler dev (pid ${dev.pid}) and isolated state at ${work}`);
  }
}

// Author tracking needs a fresh library and controlled HTTP source, not the
// developer's persistent contract-test database.
if (exitCode === 0) {
  const authors = spawnSync(process.execPath, [join(here, "author_tracking.mjs")], {
    cwd: root, stdio: "inherit", env: { ...process.env, AUTHOR_SMOKE_KEEP: "0" },
  });
  exitCode = authors.status ?? 1;
}

// Persisted progress must recover even when a crashed invocation never sent
// its terminal event; the fixture uses its own DO storage and D1.
if (exitCode === 0) {
  const progress = spawnSync(process.execPath, [join(here, "progress_replay.mjs")], {
    cwd: root, stdio: "inherit",
  });
  exitCode = progress.status ?? 1;
}

// Transfer archives are verified against two isolated databases (export from
// one, import into a fresh one), so they never touch the persistent state.
if (exitCode === 0) {
  const transfer = spawnSync(process.execPath, [join(here, "transfer.mjs")], {
    cwd: root, stdio: "inherit",
  });
  exitCode = transfer.status ?? 1;
}

process.exit(exitCode);
