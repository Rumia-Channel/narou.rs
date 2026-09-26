// ローカルで Worker をビルド・起動して契約テストを回す。
//
//   node tests/run.mjs            # debug build
//   node tests/run.mjs --release  # CI と同じ release build
//   node tests/run.mjs --keep     # 終了後も wrangler dev を残す
//
// 生成物は gitignore 済み:
//   .dev.vars           … ローカル用の NAROU_ADMIN_TOKEN (無ければ作る)
//   wrangler.test.toml  … wrangler.toml から [build] を外した実行用設定
//   .wrangler/state     … ローカルの D1 (miniflare SQLite)
import { spawn, spawnSync } from "node:child_process";
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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

// 1. ローカル用の secret。既にあれば触らない (開発者の値を壊さない)。
const devVars = join(root, ".dev.vars");
if (!existsSync(devVars)) {
  // 資格情報は at-rest で暗号化されるので、テスト用の鍵も要る (32 バイトの base64)。
  // 32 バイトちょうどでなければ復号鍵として拒否される。
  const loginKey = Buffer.alloc(32, 7).toString("base64");
  writeFileSync(devVars, `NAROU_ADMIN_TOKEN=${token}\nNAROU_RS_LOGIN_KEY=${loginKey}\n`);
  console.log(`wrote ${devVars}`);
} else {
  console.log(
    `${devVars} exists; using it as-is (NAROU_ADMIN_TOKEN and NAROU_RS_LOGIN_KEY must be set)`,
  );
}

// 2. 実行用の設定。ビルドはこのスクリプトが行うので [build] を外す
//    (wrangler dev が毎回 release ビルドを走らせないようにするため)。
const config = readFileSync(join(root, "wrangler.toml"), "utf8").replace(
  /\[build\][^\[]*/,
  "",
);
const testConfig = join(root, "wrangler.test.toml");
writeFileSync(testConfig, config);
console.log(`wrote ${testConfig}`);

// 3. ビルド (設定から [build] を外しているのでアセットはここで作る)。
run("node", ["build_assets.mjs"]);
run("worker-build", release ? ["--release"] : []);

// 4. ローカル D1 にマイグレーションを適用。
run(npx, [
  "--yes",
  "wrangler@4",
  "d1",
  "migrations",
  "apply",
  "DB",
  "--local",
  "-c",
  "wrangler.test.toml",
]);

// 5. wrangler dev を起動して readiness を待つ。
const dev = spawn(
  npx,
  [
    "--yes",
    "wrangler@4",
    "dev",
    "-c",
    "wrangler.test.toml",
    "--port",
    port,
    "--test-scheduled",
  ],
  { cwd: root, stdio: "inherit", shell: isWindows },
);

let exitCode = 1;
try {
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

  // 6. 契約テスト本体。
  const contract = spawnSync(process.execPath, [join(here, "contract.mjs")], {
    cwd: root,
    stdio: "inherit",
    env: {
      ...process.env,
      BASE_URL: base,
      NAROU_ADMIN_TOKEN: token,
      CONTRACT_QUEUE: process.env.CONTRACT_QUEUE ?? "1",
    },
  });
  exitCode = contract.status ?? 1;
} finally {
  if (!keep) {
    if (isWindows) {
      spawnSync("taskkill", ["/pid", String(dev.pid), "/T", "/F"], { stdio: "ignore" });
    } else {
      dev.kill("SIGTERM");
    }
    rmSync(testConfig, { force: true });
  } else {
    console.log(`kept wrangler dev (pid ${dev.pid}) and ${testConfig}`);
  }
}

process.exit(exitCode);
