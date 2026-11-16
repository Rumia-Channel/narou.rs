// Builds the browser transfer WASM artifact set.
//
//   node scripts/build-transfer-wasm.mjs [--install-bindgen]
//   node scripts/build-transfer-wasm.mjs --verify
//   node scripts/build-transfer-wasm.mjs --assert-lean
//
// Pipeline (docs/client-transfer-design.md §14.1):
//   1. fingerprint the codec inputs -> build_id (pre-compile, deterministic)
//   2. cargo build narou_transfer_wasm --target wasm32-unknown-unknown --release
//      (NAROU_TRANSFER_BUILD_ID is passed so codec_info().build_id matches)
//   3. wasm-bindgen --target web -> glue + wasm
//   4. content-hash glue/wasm/worker into target/transfer-artifacts/
//   5. probe the compiled glue in Node: codec_info() must report the same
//      protocol_version / abi_version / build_id that we publish
//   6. emit manifest.json (stable URL) with the frozen contract fields
//
// The artifact dir is consumed by build.rs (native OUT_DIR embedding) and by
// worker_entry/build_assets.mjs (public/assets/transfer). Nothing generated is
// committed under src/web/assets.
//
// Modes:
//   (default)           build the full artifact set
//   --verify            only validate an existing artifact directory
//   --assert-lean       only check the dependency graph (no build); combined
//                       with other flags it also runs after a build
// Flags:
//   --install-bindgen   cargo-install wasm-bindgen-cli at the Cargo.lock
//                       version when the CLI is missing or mismatched
//   --out <dir>         artifact output dir (default: target/transfer-artifacts)
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  lstat,
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { existsSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import process from "node:process";

const scriptsDir = dirname(fileURLToPath(import.meta.url));
const projectDir = resolve(scriptsDir, "..");
const defaultWorkerSource = join(
  projectDir,
  "src",
  "web",
  "assets",
  "js",
  "ui",
  "transfer-worker.js",
);
const defaultArtifactDir = join(projectDir, "target", "transfer-artifacts");
const wasmBinName = "narou_transfer_wasm.wasm";
const wasmGlueBase = "narou_transfer_wasm";
const urlPrefix = "/assets/transfer";
const hashLength = 16;
const expectedProtocolVersion = 2;
const requiredExports = [
  "codec_info",
  "open_archive",
  "validate_archive",
  "catalog_page",
  "write_transfer_manifest",
  "stream_entry",
  "close_archive",
  "build_zip",
];
// Crates that must never reach the standard (non-GPL) browser artifact. The
// Lite EPUB engine is GPL-3.0-only; image codecs belong to the Worker runtime,
// not to a ZIP codec that preserves bytes verbatim.
const bannedTransferCrates = [
  "aozora_epub3_lite",
  "image-webp",
  "image",
  "gif",
  "png",
];

function fail(message) {
  throw new Error(`build-transfer-wasm: ${message}`);
}

function sha256Hex(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function hashShort(bytes) {
  return sha256Hex(bytes).slice(0, hashLength);
}

function run(command, args, { env } = {}) {
  const result = spawnSync(command, args, {
    cwd: projectDir,
    env: env ? { ...process.env, ...env } : process.env,
    stdio: "inherit",
    shell: false,
  });
  if (result.error) fail(`${command} failed to start: ${result.error.message}`);
  if (result.status !== 0) {
    fail(`${command} ${args.join(" ")} exited with ${result.status}`);
  }
}

function capture(command, args) {
  const result = spawnSync(command, args, {
    cwd: projectDir,
    stdio: ["ignore", "pipe", "inherit"],
    shell: false,
    encoding: "utf8",
  });
  if (result.error || result.status !== 0) return null;
  return result.stdout.trim();
}

/** Extracts the locked version of a crate from Cargo.lock text. */
export function readLockedVersion(lockText, packageName) {
  const pattern = new RegExp(
    `name = "${packageName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}"\\s*\\nversion = "([^"]+)"`,
    "m",
  );
  const match = lockText.match(pattern);
  if (!match) fail(`crate ${packageName} not found in Cargo.lock`);
  return match[1];
}

let lockCache = null;
function lockText() {
  if (lockCache === null) {
    const lockPath = join(projectDir, "Cargo.lock");
    if (!existsSync(lockPath)) {
      fail("Cargo.lock is missing; build artifacts require a locked graph");
    }
    lockCache = readFileSync(lockPath, "utf8");
  }
  return lockCache;
}

/** `transfer-codec.<hash16>.js` style immutable file name. */
export function hashedFileName(base, bytes, extension) {
  return `${base}.${hashShort(bytes)}${extension}`;
}

function isSafeArtifactName(name) {
  return (
    typeof name === "string" &&
    name.length > 0 &&
    !name.startsWith("/") &&
    !name.includes("\\") &&
    name.split("/").every((part) => part && part !== "." && part !== "..") &&
    /^[A-Za-z0-9._\-/]+$/.test(name)
  );
}


/**
 * The generated browser worker is self-contained. Its only module dependency
 * is the computed, manifest-validated glue URL; literal imports and nested
 * workers must not reach a release artifact.
 */
export function findWorkerDependencyViolations(source) {
  const violations = [];
  const report = (kind, specifier, index) => {
    violations.push(
      `${kind} references a dependency outside the manifest loader: ${JSON.stringify(specifier)} (offset ${index})`,
    );
  };
  // import ... from 'spec' / export ... from 'spec'
  for (const match of source.matchAll(/\bfrom\s*(['"])(.*?)\1/gs)) {
    report("module specifier", match[2], match.index);
  }
  // bare import 'spec'
  for (const match of source.matchAll(/(^|[;\n\r])\s*import\s*(['"])(.*?)\2/gm)) {
    report("side-effect import", match[3], match.index);
  }
  // dynamic import('spec') — only literal specifiers can be checked; computed
  // expressions (manifest URLs) are allowed.
  for (const match of source.matchAll(/\bimport\s*\(\s*(['"])(.*?)\1/gs)) {
    report("dynamic import", match[2], match.index);
  }
  for (const match of source.matchAll(/\bimportScripts\s*\(([^)]*)\)/gs)) {
    let matchedLiteral = false;
    for (const literal of match[1].matchAll(/(['"])(.*?)\1/g)) {
      matchedLiteral = true;
      report("importScripts", literal[2], match.index);
    }
    if (!matchedLiteral) {
      violations.push(`importScripts() with non-literal argument at offset ${match.index}`);
    }
  }
  for (const match of source.matchAll(/\bnew\s+(?:Shared)?Worker\s*\(\s*(['"])(.*?)\1/gs)) {
    report("worker constructor", match[2], match.index);
  }
  return violations;
}

/** Files whose contents define the codec build identity (pre-compile). */
async function codecInputFiles(root = projectDir) {
  const files = [];
  const addTree = async (relativeDir) => {
    const base = join(root, ...relativeDir.split("/"));
    if (!existsSync(base)) return;
    const walk = async (dir) => {
      for (const entry of await readdir(dir, { withFileTypes: true })) {
        const path = join(dir, entry.name);
        if (entry.isDirectory()) await walk(path);
        else if (entry.isFile()) files.push(path);
      }
    };
    await walk(base);
  };
  files.push(join(root, "Cargo.toml"), join(root, "Cargo.lock"));
  for (const name of ["build.rs", ".cargo/config.toml", "rust-toolchain", "rust-toolchain.toml", "scripts/transfer-artifacts.rs"]) {
    const path = join(root, name);
    if (existsSync(path)) files.push(path);
  }
  await addTree("transfer_wasm");
  // Transfer shares record/path/platform code outside src/transfer. Changes
  // there must invalidate the artifact cache as well.
  const transferInputs = files.length;
  await addTree("src");
  files.splice(transferInputs, files.length - transferInputs, ...files.slice(transferInputs).filter((path) => /\.(rs|pest)$/.test(path)));
  await addTree("vendor/async_zip");
  return files.sort();
}

/**
 * Deterministic build identity: hash of codec sources, manifests, lockfile and
 * toolchain versions. Computed BEFORE compilation so it can be passed into the
 * build (codec_info().build_id) and written to the manifest unchanged. The
 * artifact hashes of the produced wasm/glue/worker stay separate fields.
 */
export async function computeBuildId({ rustcVersion, bindgenVersion, root = projectDir } = {}) {
  const hasher = createHash("sha256");
  hasher.update("narou-rs transfer codec build-id v1\n");
  hasher.update(`rustc=${rustcVersion ?? "unknown"}\n`);
  hasher.update(`wasm-bindgen=${bindgenVersion ?? "unknown"}\n`);
  hasher.update(`rustflags=${process.env.CARGO_ENCODED_RUSTFLAGS ?? process.env.RUSTFLAGS ?? ""}\n`);
  for (const file of await codecInputFiles(root)) {
    hasher.update(`${relative(root, file).split(sep).join("/")}\0`);
    hasher.update(await readFile(file));
    hasher.update("\0");
  }
  return hasher.digest("hex");
}

function cargoTargetDir() {
  const configured = process.env.CARGO_TARGET_DIR;
  return configured ? resolve(projectDir, configured) : join(projectDir, "target");
}

function ensureWasmTarget() {
  const installed = capture("rustup", ["target", "list", "--installed"]);
  if (installed === null) {
    // Non-rustup toolchain: the cargo invocation below surfaces the error.
    return;
  }
  if (!installed.split(/\r?\n/).includes("wasm32-unknown-unknown")) {
    run("rustup", ["target", "add", "wasm32-unknown-unknown"]);
  }
}

function ensureWasmBindgen(installAllowed) {
  const expected = readLockedVersion(lockText(), "wasm-bindgen");
  let found = capture("wasm-bindgen", ["--version"]);
  let version = found ? (found.match(/(\d+\.\d+\.\d+)/) ?? [])[1] : null;
  if (version === expected) return expected;
  if (!installAllowed) {
    fail(
      `wasm-bindgen CLI ${version ?? "(missing)"} does not match the locked ` +
        `wasm-bindgen crate ${expected}. Install it with ` +
        `cargo install wasm-bindgen-cli --version ${expected} --locked ` +
        `or pass --install-bindgen.`,
    );
  }
  run("cargo", [
    "install",
    "wasm-bindgen-cli",
    "--version",
    expected,
    "--locked",
  ]);
  found = capture("wasm-bindgen", ["--version"]);
  version = found ? (found.match(/(\d+\.\d+\.\d+)/) ?? [])[1] : null;
  if (version !== expected) {
    fail(
      `wasm-bindgen CLI is still ${version ?? "(missing)"} after install (expected ${expected})`,
    );
  }
  return expected;
}

async function buildWasm(buildId) {
  run(
    "cargo",
    [
      "build",
      "--manifest-path",
      "transfer_wasm/Cargo.toml",
      "--target",
      "wasm32-unknown-unknown",
      "--release",
      "--locked",
    ],
    { env: { NAROU_TRANSFER_BUILD_ID: buildId } },
  );
  const wasmPath = join(
    cargoTargetDir(),
    "wasm32-unknown-unknown",
    "release",
    wasmBinName,
  );
  if (!existsSync(wasmPath)) fail(`expected wasm output is missing: ${wasmPath}`);
  return wasmPath;
}

async function bindgenWeb(wasmPath, outDir) {
  await mkdir(outDir, { recursive: true });
  run("wasm-bindgen", [wasmPath, "--target", "web", "--out-dir", outDir]);
}

/** Initialize the generated glue in Node and read codec_info(). */
async function probeCodecInfo(gluePath, wasmBytes) {
  const glue = await import(pathToFileURL(gluePath).href);
  if (typeof glue.default !== "function") {
    fail("wasm-bindgen glue has no default init export");
  }
  await glue.default({ module_or_path: wasmBytes });
  const missing = requiredExports.filter((name) => typeof glue[name] !== "function");
  if (missing.length > 0) {
    fail(`generated glue is missing required exports: ${missing.join(", ")}`);
  }
  const raw = await glue.codec_info();
  const info = typeof raw === "string" ? JSON.parse(raw) : raw;
  if (!info || typeof info !== "object") {
    fail("codec_info() did not return a JSON object");
  }
  return info;
}

function checkCodecInfo(info, buildId) {
  if (info.protocol_version !== expectedProtocolVersion) {
    fail(
      `codec_info().protocol_version is ${info.protocol_version}, expected ${expectedProtocolVersion}`,
    );
  }
  if (info.abi_version !== 2) {
    fail(`codec_info().abi_version is ${info.abi_version}, expected 2`);
  }
  if (info.build_id !== buildId) {
    fail(
      `codec_info().build_id (${info.build_id}) does not match the computed ` +
        `build id (${buildId}). NAROU_TRANSFER_BUILD_ID did not reach the ` +
        `compiled artifact (transfer_wasm needs cargo:rerun-if-env-changed, ` +
        `or the build was stale).`,
    );
  }
  return info.abi_version;
}

/**
 * Validates an emitted artifact directory against the loader contract.
 * Returns an array of problem strings; empty means the set is servable.
 */
export async function verifyArtifactDir(dir) {
  const problems = [];
  const manifestPath = join(dir, "manifest.json");
  if (!existsSync(manifestPath)) return [`manifest.json missing in ${dir}`];
  let manifest;
  try {
    manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  } catch (error) {
    return [`manifest.json is not valid JSON: ${error.message}`];
  }
  if (!manifest || typeof manifest !== "object" || Array.isArray(manifest)) {
    return ["manifest.json must contain an object"];
  }
  if (manifest.protocol_version !== expectedProtocolVersion) {
    problems.push(
      `protocol_version must be ${expectedProtocolVersion}, got ${manifest.protocol_version}`,
    );
  }
  if (manifest.abi_version !== 2) {
    problems.push(`abi_version must be 2, got ${manifest.abi_version}`);
  }
  if (typeof manifest.build_id !== "string" || !/^[0-9a-f]{64}$/.test(manifest.build_id)) {
    problems.push(`build_id must be a lowercase hex string, got ${manifest.build_id}`);
  }
  const files = manifest.files;
  if (!files || typeof files !== "object" || Array.isArray(files)) {
    problems.push("files must be an object mapping artifact names to sha256");
  }
  const names = new Set(files ? Object.keys(files) : []);
  for (const name of names) {
    if (!isSafeArtifactName(name)) problems.push(`unsafe artifact name ${JSON.stringify(name)}`);
    if (typeof files[name] !== "string" || !/^[0-9a-f]{64}$/.test(files[name])) {
      problems.push(`invalid SHA-256 for ${name}`);
    }
    if (name === "manifest.json") problems.push("files must not list manifest.json itself");
  }
  for (const field of ["worker_url", "glue_url", "wasm_url"]) {
    const url = manifest[field];
    if (typeof url !== "string" || !url.startsWith(`${urlPrefix}/`)) {
      problems.push(`${field} must start with ${urlPrefix}/, got ${url}`);
      continue;
    }
    const name = url.slice(urlPrefix.length + 1);
    if (!names.has(name)) problems.push(`${field} target ${name} is not listed in files`);
    const prefix = field === "worker_url" ? "transfer-worker" : "transfer-codec";
    const extension = field === "wasm_url" ? ".wasm" : ".js";
    if (typeof files?.[name] === "string"
        && name !== `${prefix}.${files[name].slice(0, hashLength)}${extension}`) {
      problems.push(`${field} target is not named by its content hash`);
    }
  }
  for (const [name, expectedSha] of Object.entries(files ?? {})) {
    if (!isSafeArtifactName(name)) continue;
    const path = join(dir, ...name.split("/"));
    if (!existsSync(path)) {
      problems.push(`listed artifact is missing: ${name}`);
      continue;
    }
    const bytes = await readFile(path);
    if (bytes.length === 0) problems.push(`artifact ${name} is empty`);
    const actual = sha256Hex(bytes);
    if (actual !== expectedSha) {
      problems.push(
        `artifact ${name} sha256 mismatch (manifest ${expectedSha}, actual ${actual})`,
      );
    }
    if (
      name.endsWith(".wasm") &&
      !(bytes.length > 8 && bytes[0] === 0 && bytes[1] === 0x61 && bytes[2] === 0x73 && bytes[3] === 0x6d)
    ) {
      problems.push(`artifact ${name} lacks the wasm magic header`);
    }
  }
  const workerName =
    typeof manifest.worker_url === "string" && manifest.worker_url.startsWith(`${urlPrefix}/`)
      ? manifest.worker_url.slice(urlPrefix.length + 1)
      : null;
  if (workerName && names.has(workerName) && isSafeArtifactName(workerName)) {
    const workerSource = await readFile(join(dir, ...workerName.split("/")), "utf8");
    for (const violation of findWorkerDependencyViolations(workerSource)) {
      problems.push(`worker self-containment: ${violation}`);
    }
  }
  return problems;
}

/** cargo tree output must not contain banned crates anywhere in the graph. */
export function assertLeanDependencyTree(treeText) {
  const offenders = bannedTransferCrates.filter(
    (crate) =>
      new RegExp(`^${crate.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?:\\s|$)`, "m").test(treeText),
  );
  if (offenders.length > 0) {
    fail(
      `transfer WASM graph contains crates excluded from the standard artifact: ` +
        `${offenders.join(", ")}. The narou_rs dependency of narou_transfer_wasm ` +
        `must use the lean transfer-runtime feature, never worker-runtime/lite.`,
    );
  }
}

function checkLeanGraph() {
  const tree = capture("cargo", [
    "tree",
    "-p",
    "narou_transfer_wasm",
    "--target",
    "wasm32-unknown-unknown",
    "--locked",
    "--prefix",
    "none",
  ]);
  if (tree === null) fail("cargo tree -p narou_transfer_wasm failed");
  assertLeanDependencyTree(tree);
  console.log("build-transfer-wasm: dependency graph is free of lite/GPL and image crates");
}

async function listSnippetFiles(dir) {
  const snippetsDir = join(dir, "snippets");
  if (!existsSync(snippetsDir)) return [];
  const found = [];
  const walk = async (current, prefix) => {
    for (const entry of await readdir(current, { withFileTypes: true })) {
      const rel = `${prefix}${entry.name}`;
      const path = join(current, entry.name);
      if (entry.isDirectory()) await walk(path, `${rel}/`);
      else if (entry.isFile()) found.push(rel);
    }
  };
  await walk(snippetsDir, "snippets/");
  return found.sort();
}

function parseArgs(argv) {
  const options = {
    installBindgen: false,
    verify: false,
    assertLean: false,
    out: defaultArtifactDir,
    workerSrc: defaultWorkerSource,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    switch (arg) {
      case "--install-bindgen":
        options.installBindgen = true;
        break;
      case "--verify":
        options.verify = true;
        break;
      case "--assert-lean":
        options.assertLean = true;
        break;
      case "--out":
        options.out = resolve(argv[++index] ?? fail("--out needs a value"));
        break;
      case "--worker-src":
        options.workerSrc = resolve(argv[++index] ?? fail("--worker-src needs a value"));
        break;
      default:
        fail(`unknown argument: ${arg}`);
    }
  }
  return options;
}

export async function assertSafeOutputDirectory(directory, root = projectDir) {
  const relativeName = relative(root, resolve(directory)).split(sep).join("/");
  if (!relativeName.startsWith("target/")) {
    fail("--out must be a generated directory below target/, never a source tree or target/ itself");
  }
  // A symlink/junction in an ancestor could turn a harmless-looking target/
  // child into a source directory outside that tree.
  for (let path = resolve(directory); path !== resolve(root); path = dirname(path)) {
    try {
      if ((await lstat(path)).isSymbolicLink()) {
        fail("--out must not traverse a symlink or junction");
      }
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
  }
}

async function verifyCompiledCodec(directory) {
  const manifest = JSON.parse(await readFile(join(directory, "manifest.json"), "utf8"));
  const glueName = manifest.glue_url.slice(urlPrefix.length + 1);
  const wasmName = manifest.wasm_url.slice(urlPrefix.length + 1);
  const info = await probeCodecInfo(
    join(directory, glueName),
    await readFile(join(directory, wasmName)),
  );
  checkCodecInfo(info, manifest.build_id);
}

async function main() {
  const argv = process.argv.slice(2);
  const options = parseArgs(argv);

  if (options.verify) {
    const problems = await verifyArtifactDir(options.out);
    if (problems.length > 0) {
      fail(`artifact verification failed:\n  - ${problems.join("\n  - ")}`);
    }
    await verifyCompiledCodec(options.out);
    console.log(`build-transfer-wasm: verified ${options.out}`);
    return;
  }
  // `--assert-lean` alone is a check-only mode; combined with other flags the
  // same check also gates the emitted artifacts at the end of the build.
  const leanOnly = options.assertLean && argv.every((arg) => arg === "--assert-lean");
  if (leanOnly) {
    checkLeanGraph();
    return;
  }

  await assertSafeOutputDirectory(options.out);

  // 1. Browser worker source (frozen path produced by the frontend slice).
  if (!existsSync(options.workerSrc)) {
    fail(
      `browser transfer worker source is missing: ${options.workerSrc}. ` +
        `The dedicated worker module is required; no placeholder is emitted.`,
    );
  }
  const workerSource = await readFile(options.workerSrc);
  const workerViolations = findWorkerDependencyViolations(workerSource.toString("utf8"));
  if (workerViolations.length > 0) {
    fail(`browser worker is not self-contained:\n  - ${workerViolations.join("\n  - ")}`);
  }

  ensureWasmTarget();
  const bindgenVersion = ensureWasmBindgen(options.installBindgen);
  const rustcVersion = capture("rustc", ["--version"]) ?? "unknown";
  const buildId = await computeBuildId({ rustcVersion, bindgenVersion });

  // 2. Skip the compile when codec inputs, the worker source and this script
  //    are all unchanged AND the emitted set still verifies; keeps
  //    `wrangler dev` / contract reruns fast without weakening the contract.
  const skipInputs = createHash("sha256")
    .update(buildId)
    .update(workerSource)
    .update(await readFile(fileURLToPath(import.meta.url)))
    .digest("hex");
  const markerPath = join(options.out, ".inputs");
  if (existsSync(markerPath) && (await readFile(markerPath, "utf8")).trim() === skipInputs) {
    if ((await verifyArtifactDir(options.out)).length === 0) {
      await verifyCompiledCodec(options.out);
      console.log(`build-transfer-wasm: artifacts up to date (build_id ${buildId.slice(0, 12)})`);
      if (options.assertLean) checkLeanGraph();
      return;
    }
  }

  // 3. Compile + bindgen into a staging dir, then publish hashed names.
  const staging = await mkdtemp(join(tmpdir(), "narou-transfer-wasm-"));
  try {
    const gluePath = join(staging, `${wasmGlueBase}.js`);
    const wasmStagePath = join(staging, `${wasmGlueBase}_bg.wasm`);
    const wasmPath = await buildWasm(buildId);
    await bindgenWeb(wasmPath, staging);
    await writeFile(join(staging, "package.json"), '{"type":"module"}\n');
    if (!existsSync(gluePath) || !existsSync(wasmStagePath)) {
      fail("wasm-bindgen did not emit the expected glue/wasm pair");
    }
    const wasmBytes = await readFile(wasmStagePath);
    const info = await probeCodecInfo(gluePath, wasmBytes);
    const abiVersion = checkCodecInfo(info, buildId);

    // 4. Publish: content-hashed immutable names under the artifact dir.
    let glueSource = (await readFile(gluePath)).toString("utf8");
    const wasmName = hashedFileName("transfer-codec", wasmBytes, ".wasm");
    // init() without an explicit wasm URL would look up the bindgen default
    // name; point it at the hashed artifact so standalone init still works.
    glueSource = glueSource.split(`${wasmGlueBase}_bg.wasm`).join(wasmName);
    const glueBytes = Buffer.from(glueSource, "utf8");
    const glueName = hashedFileName("transfer-codec", glueBytes, ".js");
    const workerName = hashedFileName("transfer-worker", workerSource, ".js");

    const files = {};
    const emit = async (name, bytes) => {
      const dest = join(options.out, ...name.split("/"));
      await mkdir(dirname(dest), { recursive: true });
      await writeFile(dest, bytes);
      files[name] = sha256Hex(bytes);
    };

    await rm(options.out, { recursive: true, force: true });
    await mkdir(options.out, { recursive: true });
    await writeFile(join(options.out, "package.json"), '{"type":"module"}\n');
    await emit(wasmName, wasmBytes);
    await emit(glueName, glueBytes);
    await emit(workerName, workerSource);
    for (const rel of await listSnippetFiles(staging)) {
      await emit(rel, await readFile(join(staging, ...rel.split("/"))));
    }

    const manifest = {
      protocol_version: expectedProtocolVersion,
      abi_version: abiVersion,
      build_id: buildId,
      worker_url: `${urlPrefix}/${workerName}`,
      glue_url: `${urlPrefix}/${glueName}`,
      wasm_url: `${urlPrefix}/${wasmName}`,
      files,
    };
    await writeFile(join(options.out, "manifest.json"), `${JSON.stringify(manifest)}\n`);
    await writeFile(markerPath, `${skipInputs}\n`);

    const problems = await verifyArtifactDir(options.out);
    if (problems.length > 0) {
      fail(`artifact verification failed:\n  - ${problems.join("\n  - ")}`);
    }
    if (options.assertLean) checkLeanGraph();
    console.log(
      `build-transfer-wasm: emitted ${Object.keys(files).length} artifacts ` +
        `(build_id ${buildId.slice(0, 12)}, abi ${abiVersion})`,
    );
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}

const invokedAsScript =
  process.argv[1] &&
  pathToFileURL(resolve(process.argv[1])).href === import.meta.url;
if (invokedAsScript) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.message : error);
    process.exit(1);
  });
}
