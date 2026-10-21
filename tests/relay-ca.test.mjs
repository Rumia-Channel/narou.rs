// CA バンドルの検証 (2026-10-20 の eMudhra 中間 CA 侵害対応) の回帰。
//
// リレーは取得先の TLS を CA バンドルだけで検証する。侵害されたルート CA を
// 含むバンドルを受け入れると、任意ホストへの MITM を許すため、取得時と
// 起動時の両方で既知の侵害ルートを拒否する。
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  CACERT_URL,
  CompromisedCaError,
  fetchCacert,
  verifyCacert,
  verifyCacertFile,
} from "../scripts/relay/fetch-curl.mjs";
import { startRelay } from "../scripts/relay/server.mjs";
import { BLOCKED_BUNDLE } from "./relay-blocked-ca-fixture.mjs";
import { cert } from "./relay-tls-fixture.mjs";

test("a bundle without compromised roots passes validation", () => {
  assert.deepEqual(verifyCacert(Buffer.from(cert)), { certificates: 1 });
});

test("a bundle with compromised roots is rejected with the root names", () => {
  assert.throws(
    () => verifyCacert(BLOCKED_BUNDLE),
    (error) => {
      assert.ok(error instanceof CompromisedCaError);
      assert.ok(error.roots.includes("AAA Certificate Services"));
      assert.ok(error.roots.includes("emSign ECC Root CA - G3"));
      return /更新された CA バンドル/.test(error.message);
    },
  );
});

test("unparseable CA bundles are rejected", () => {
  assert.throws(() => verifyCacert(Buffer.from("not a certificate")), /証明書がありません/);
  assert.throws(
    () => verifyCacert(Buffer.from("-----BEGIN CERTIFICATE-----\nFAKE\n-----END CERTIFICATE-----\n")),
    /解析できない証明書/,
  );
});

test("fetchCacert verifies the downloaded bundle", async () => {
  const requested = [];
  const accepted = await fetchCacert({
    download: async (url) => { requested.push(url); return Buffer.from(cert); },
  });
  assert.equal(requested.length, 1);
  assert.equal(requested[0], CACERT_URL);
  assert.deepEqual(accepted, Buffer.from(cert));

  await assert.rejects(
    fetchCacert({ download: async () => BLOCKED_BUNDLE }),
    (error) => error instanceof CompromisedCaError,
  );
});

test("verifyCacertFile reads and validates a bundle", () => {
  const directory = mkdtempSync(join(tmpdir(), "relay-ca-file-"));
  try {
    const clean = join(directory, "clean.pem");
    const blocked = join(directory, "blocked.pem");
    writeFileSync(clean, cert);
    writeFileSync(blocked, BLOCKED_BUNDLE);
    assert.equal(verifyCacertFile(clean).certificates, 1);
    assert.throws(() => verifyCacertFile(blocked), (error) => error instanceof CompromisedCaError);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("startRelay refuses to boot with a compromised NAROU_RELAY_CACERT", () => {
  const directory = mkdtempSync(join(tmpdir(), "relay-ca-start-"));
  const cacert = join(directory, "cacert.pem");
  writeFileSync(cacert, BLOCKED_BUNDLE);
  try {
    assert.throws(
      () => startRelay({ env: {
        SERVER_PORT: "0",
        NAROU_RELAY_KEY: "relay-ca-test-key",
        NAROU_RELAY_ENV_FILE: join(directory, "missing.env"),
        NAROU_RELAY_CACERT: cacert,
        NAROU_RELAY_CURL: "",
      } }),
      /CA バンドルを検証できません/,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("startRelay accepts a verified CA bundle and shares it with every transport", async () => {
  // 検証処理は起動時の全経路 (curl / wget / Node) で共有する。import 漏れや
  // 検証の未実行はここで起動失敗として検出する。
  const directory = mkdtempSync(join(tmpdir(), "relay-ca-start-ok-"));
  const cacert = join(directory, "cacert.pem");
  writeFileSync(cacert, cert);
  const server = startRelay({
    env: {
      SERVER_PORT: "0",
      NAROU_RELAY_KEY: "relay-ca-test-key",
      NAROU_RELAY_ENV_FILE: join(directory, "missing.env"),
      NAROU_RELAY_CURL: "",
    },
    curl: { bin: "curl", cacert, source: "test" },
  });
  try {
    assert.ok(server.fetchOrder.length > 0);
  } finally {
    await new Promise((resolve) => server.close(resolve));
    rmSync(directory, { recursive: true, force: true });
  }
});
