// Node24 + 実 workerd の契約試験。GCP/Cloudflare 配備やユーザー資格情報は使わない。
import { createServer } from "node:net";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const windows = process.platform === "win32";
const token = "relay-contract-token";
const probe = createServer();
probe.listen(0, "127.0.0.1");
await once(probe, "listening");
const port = probe.address().port;
await new Promise((resolve) => probe.close(resolve));
const args = [
  "--yes", "wrangler@4", "dev", "--local", "--config", "scripts/relay/worker/wrangler.toml",
  "--ip", "127.0.0.1", "--port", String(port), "--var", `NAROU_RELAY_KEY:${token}`,
];
// 引数は固定文字列と生成した数値ポートだけ。shell:true の未引用連結は使わない。
const dev = spawn(windows ? (process.env.ComSpec || "cmd.exe") : "npx",
  windows ? ["/d", "/s", "/c", `"npx.cmd ${args.join(" ")}"`] : args, {
  cwd: root, stdio: ["ignore", "pipe", "pipe"], detached: !windows,
  windowsVerbatimArguments: windows, windowsHide: true,
  env: { ...process.env, CI: "true", BROWSER: "none", WRANGLER_SEND_METRICS: "false" },
});
const closed = new Promise((resolve) => dev.once("close", resolve));
const stopped = new Set();
const stop = (child) => {
  if (!child?.pid || stopped.has(child.pid)) return;
  stopped.add(child.pid);
  if (windows) spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
  else {
    try { process.kill(-child.pid, "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw error; }
  }
};
let tests;
let interrupted = false;
const onSignal = () => { interrupted = true; stop(tests); stop(dev); process.exitCode = 1; };
process.once("SIGINT", onSignal);
process.once("SIGTERM", onSignal);
let timer;
try {
  await new Promise((resolve, reject) => {
    let output = "";
    timer = setTimeout(() => reject(new Error("local Worker relay startup timed out")), 120_000);
    const receive = (chunk) => {
      output = (output + chunk.toString()).slice(-8192);
      if (output.includes(`Ready on http://127.0.0.1:${port}`)) resolve();
    };
    dev.stdout.on("data", receive);
    dev.stderr.on("data", receive);
    dev.once("error", () => reject(new Error("could not launch local Worker relay")));
    dev.once("exit", () => reject(new Error("local Worker relay exited before tests finished")));
  });
  clearTimeout(timer);
  tests = spawn(process.execPath, [
    "--test", "--test-timeout=90000", "tests/relay.test.mjs", "tests/relay-policy.test.mjs", "tests/relay-server.test.mjs",
    "tests/relay-hosting.test.mjs",
  ], {
    cwd: root, stdio: "inherit", detached: !windows,
    env: { ...process.env, RELAY_WORKER_BASE_URL: `http://127.0.0.1:${port}`, RELAY_TEST_TOKEN: token },
  });
  const [code] = await once(tests, "exit");
  process.exitCode = interrupted ? 1 : code ?? 1;
} catch {
  console.error("relay contract runner failed; check Node24, npm/Wrangler availability and local ports");
  process.exitCode = 1;
} finally {
  clearTimeout(timer);
  if (tests && tests.exitCode === null) stop(tests);
  stop(dev);
  await closed;
  process.removeListener("SIGINT", onSignal);
  process.removeListener("SIGTERM", onSignal);
}
