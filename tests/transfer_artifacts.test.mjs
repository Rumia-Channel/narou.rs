// Narrow unit tests for the transfer WASM artifact pipeline
// (scripts/build-transfer-wasm.mjs). These cover the pure packaging rules:
// worker self-containment, artifact verification, manifest naming and the
// lean dependency gate. The real cargo + wasm-bindgen build is exercised by
// the `wasm` CI job; these tests must not require a toolchain.
import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  assertLeanDependencyTree,
  assertSafeOutputDirectory,
  computeBuildId,
  findWorkerDependencyViolations,
  hashedFileName,
  readLockedVersion,
  verifyArtifactDir,
} from "../scripts/build-transfer-wasm.mjs";
import { createHash } from "node:crypto";

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

test("readLockedVersion extracts the locked crate version", () => {
  const lock = [
    'name = "wasm-bindgen"',
    'version = "0.2.130"',
    "",
    'name = "wasm-bindgen-cli"',
    'version = "0.1.0"',
    "",
    'name = "serde"',
    'version = "1.0.0"',
  ].join("\n");
  assert.equal(readLockedVersion(lock, "wasm-bindgen"), "0.2.130");
  assert.equal(readLockedVersion(lock, "serde"), "1.0.0");
  assert.throws(() => readLockedVersion(lock, "missing-crate"));
});

test("hashedFileName embeds a 16-hex content hash", () => {
  const name = hashedFileName("transfer-codec", Buffer.from("hello"), ".js");
  assert.match(name, /^transfer-codec\.[0-9a-f]{16}\.js$/);
  assert.equal(
    hashedFileName("transfer-codec", Buffer.from("hello"), ".js"),
    name,
    "naming is deterministic",
  );
});

test("worker self-containment accepts manifest-provided absolute URLs", () => {
  const clean = [
    "const { glue_url, wasm_url } = self.__TRANSFER__;",
    "const codec = await import(glue_url);",
    "await codec.default(wasm_url);",
  ].join("\n");
  assert.deepEqual(findWorkerDependencyViolations(clean), []);
});

test("worker self-containment rejects relative and bundled specifiers", () => {
  const cases = [
    "import { x } from './glue.js';",
    'import { x } from "../pkg/glue.js";',
    "export { y } from './other.js';",
    "import './polyfill.js';",
    "const m = await import('./lazy.js');",
    "importScripts('./classic.js');",
    "const w = new Worker('./nested.js');",
    'import { x } from "bare-package";',
    'import "https://example.com/absolute.js";',
    'import "/assets/unlisted.js";',
    'const module = await import("data:text/javascript,export default 1");',
  ];
  for (const source of cases) {
    assert.ok(
      findWorkerDependencyViolations(source).length > 0,
      `expected violation for: ${source}`,
    );
  }
});

test("worker self-containment rejects non-literal importScripts", () => {
  const violations = findWorkerDependencyViolations("importScripts(computeName());");
  assert.equal(violations.length, 1);
});

async function makeArtifactDir() {
  const dir = await mkdtemp(join(tmpdir(), "transfer-artifacts-test-"));
  const wasm = Buffer.concat([Buffer.from([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0]), Buffer.alloc(64)]);
  const glue = Buffer.from("export default function init() {}", "utf8");
  const worker = Buffer.from("const g = await import(glueUrl);", "utf8");
  const wasmName = hashedFileName("transfer-codec", wasm, ".wasm");
  const glueName = hashedFileName("transfer-codec", glue, ".js");
  const workerName = hashedFileName("transfer-worker", worker, ".js");
  const files = { [wasmName]: sha256(wasm), [glueName]: sha256(glue), [workerName]: sha256(worker) };
  const manifest = {
    protocol_version: 2,
    abi_version: 2,
    build_id: sha256(Buffer.from("build")),
    worker_url: `/assets/transfer/${workerName}`,
    glue_url: `/assets/transfer/${glueName}`,
    wasm_url: `/assets/transfer/${wasmName}`,
    files,
  };
  await writeFile(join(dir, "manifest.json"), JSON.stringify(manifest));
  await writeFile(join(dir, wasmName), wasm);
  await writeFile(join(dir, glueName), glue);
  await writeFile(join(dir, workerName), worker);
  return { dir, manifest };
}

test("verifyArtifactDir accepts a coherent artifact set", async () => {
  const { dir } = await makeArtifactDir();
  try {
    assert.deepEqual(await verifyArtifactDir(dir), []);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("verifyArtifactDir fails when the manifest is missing", async () => {
  const dir = await mkdtemp(join(tmpdir(), "transfer-artifacts-test-"));
  try {
    const problems = await verifyArtifactDir(dir);
    assert.equal(problems.length, 1);
    assert.match(problems[0], /manifest\.json missing/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("verifyArtifactDir fails on hash mismatch and unsafe names", async () => {
  const { dir, manifest } = await makeArtifactDir();
  try {
    // Corrupt a listed file.
    await writeFile(join(dir, manifest.glue_url.split("/").at(-1)), "tampered");
    let problems = await verifyArtifactDir(dir);
    assert.ok(problems.some((p) => p.includes("sha256 mismatch")), problems.join("; "));

    // Reject manifest listing a path outside the artifact dir.
    await writeFile(
      join(dir, "manifest.json"),
      JSON.stringify({
        ...manifest,
        files: { ...manifest.files, "../escape.js": sha256(Buffer.from("x")) },
      }),
    );
    problems = await verifyArtifactDir(dir);
    assert.ok(problems.some((p) => p.includes("unsafe artifact name")), problems.join("; "));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("verifyArtifactDir fails on broken URLs and worker dependencies", async () => {
  const { dir, manifest } = await makeArtifactDir();
  try {
    const tamperedWorker = "import './broken.js';";
    const workerName = manifest.worker_url.split("/").at(-1);
    await writeFile(join(dir, workerName), tamperedWorker);
    await writeFile(
      join(dir, "manifest.json"),
      JSON.stringify({
        ...manifest,
        files: {
          ...manifest.files,
          [workerName]: sha256(Buffer.from(tamperedWorker)),
        },
        glue_url: manifest.glue_url.slice(1),
        protocol_version: 3,
      }),
    );
    const problems = await verifyArtifactDir(dir);
    assert.ok(problems.some((p) => p.includes("protocol_version")), problems.join("; "));
    assert.ok(problems.some((p) => p.includes("glue_url")), problems.join("; "));
    assert.ok(problems.some((p) => p.includes("self-containment")), problems.join("; "));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("lean gate fails when GPL or image crates reach the transfer graph", () => {
  const dirty = [
    "narou_transfer_wasm v0.6.0",
    "narou_rs v0.6.7",
    "aozora_epub3_lite v0.1.6",
    "image v0.25.0",
    "sha2 v0.11.0",
  ].join("\n");
  assert.throws(() => assertLeanDependencyTree(dirty), /lite|GPL/i);
  const clean = ["narou_transfer_wasm v0.6.0", "narou_rs v0.6.7", "sha2 v0.11.0"].join("\n");
  assert.doesNotThrow(() => assertLeanDependencyTree(clean));
});

test("computeBuildId is deterministic over codec inputs", async () => {
  const root = await mkdtemp(join(tmpdir(), "transfer-buildid-"));
  try {
    await writeFile(join(root, "Cargo.toml"), "[workspace]\n");
    await writeFile(join(root, "Cargo.lock"), "lock\n");
    await mkdir(join(root, "src", "transfer"), { recursive: true });
    await writeFile(join(root, "src", "transfer", "mod.rs"), "pub fn codec() {}\n");
    const versions = { rustcVersion: "rustc test", bindgenVersion: "0.0.0" };
    const first = await computeBuildId({ ...versions, root });
    const second = await computeBuildId({ ...versions, root });
    assert.equal(first, second);
    assert.match(first, /^[0-9a-f]{64}$/);
    await writeFile(join(root, "src", "transfer", "mod.rs"), "pub fn codec() { /* changed */ }\n");
    const third = await computeBuildId({ ...versions, root });
    assert.notEqual(third, first, "source changes alter the build id");
    await mkdir(join(root, "src", "db"), { recursive: true });
    await writeFile(join(root, "src", "db", "novel_record.rs"), "pub struct Record;\n");
    const fourth = await computeBuildId({ ...versions, root });
    assert.notEqual(fourth, third, "shared record code also invalidates the codec");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("artifact output refuses source trees and target ancestors", async () => {
  const root = await mkdtemp(join(tmpdir(), "transfer-output-test-"));
  try {
    for (const child of ["", "target", "src", "transfer_wasm", "worker_entry", "tests", "target/../../escape"]) {
      await assert.rejects(assertSafeOutputDirectory(join(root, child), root), /generated directory/);
    }
    await mkdir(join(root, "target"));
    await mkdir(join(root, "src"));
    await symlink(join(root, "src"), join(root, "target", "source-link"), "junction");
    await assert.rejects(
      assertSafeOutputDirectory(join(root, "target", "source-link", "artifacts"), root),
      /symlink|junction/,
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
