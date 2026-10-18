// Node / Worker の取得リレーを実 HTTP 上流に接続し、cookie・非 follow・
// GET の UTF-8 ヘッダ・16 MiB 本文・64 KiB JSON・不完全な応答の契約を固定する。
// fetch 自体はモックしない。共通 reader のキャンセル境界だけは byte stream で検証する。

import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { gzipSync } from "node:zlib";
import {
  MAX_BODY_BYTES,
  MAX_METADATA_BYTES,
  MAX_REQUEST_BYTES,
  assertResponseMetadata,
  decodeHeaderJson,
  headerPairs,
  readBoundedBytes,
} from "../scripts/relay/worker/protocol.mjs";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const TOKEN = process.env.RELAY_TEST_TOKEN || "test-key";
const MAX_BINARY = Buffer.alloc(MAX_BODY_BYTES, 0xa5);
MAX_BINARY[0] = 0;
MAX_BINARY[65535] = 1;
MAX_BINARY[MAX_BINARY.length - 1] = 0xff;
const COMPRESSED_BODY = Buffer.from("compressed 日本語 body".repeat(100));

/// 空きポートを 1 つ確保する (listen して即座に閉じる)。
const freePort = () =>
  new Promise((resolve, reject) => {
    const probe = createServer();
    probe.on("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });

// 上流は実 HTTP サーバ。curl のない環境でも fetch は同じ契約を満たす。
const startUpstream = () =>
  new Promise((resolve) => {
    const seen = { multi: "", utf8: "", followed: 0, binaryCurl: 0 };
    const server = createServer((req, res) => {
      let path = req.url;
      const isCurl = req.headers["user-agent"]?.startsWith("curl/");
      if (path.startsWith("/fetch-only/")) {
        // curl だけを実接続で失敗させ、Node の fetch フォールバックも通す。
        if (isCurl) {
          req.socket.end();
          return;
        }
        path = path.slice("/fetch-only".length);
      }
      if (path === "/dup") {
        seen.multi = req.headers["x-multi"] ?? "";
        res.writeHead(200, [
          "Content-Type", "text/html; charset=UTF-8",
          "Set-Cookie", "sid=1; Path=/",
          "Set-Cookie", "over18=yes",
        ]);
        res.end("body");
        return;
      }
      if (path === "/utf8") {
        seen.utf8 = req.headers["x-label"] ?? "";
        seen.multi = req.headers["x-multi"] ?? "";
        res.writeHead(200, { "Content-Type": "text/plain" });
        res.end("UTF-8 header accepted");
        return;
      }
      if (path === "/account") {
        const loggedIn = req.headers.cookie === "sid=1; over18=yes";
        res.writeHead(loggedIn ? 200 : 401, { "Content-Type": "text/plain" });
        res.end(loggedIn ? "signed in" : "sign in required");
        return;
      }
      if (path === "/interim") {
        req.socket.end(
          "HTTP/1.1 100 Continue\r\nX-Interim: 1\r\n\r\n" +
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n" +
            "Set-Cookie: after-100=1\r\nContent-Length: 5\r\nConnection: close\r\n\r\nafter",
        );
        return;
      }
      if (path === "/redirect") {
        res.writeHead(302, { Location: "/followed" });
        res.end();
        return;
      }
      if (path === "/binary") {
        if (isCurl) seen.binaryCurl += 1;
        res.writeHead(200, {
          "Content-Type": "application/octet-stream",
          "Content-Length": MAX_BINARY.length,
          "X-Boundary": "h".repeat(2048),
        });
        res.end(MAX_BINARY);
        return;
      }
      if (path === "/compressed") {
        const body = gzipSync(COMPRESSED_BODY);
        res.writeHead(200, {
          "Content-Type": "text/plain; charset=UTF-8",
          "Content-Encoding": "gzip",
          "Content-Length": body.length,
        });
        res.end(body);
        return;
      }
      if (path === "/oversize-declared") {
        req.socket.end(
          `HTTP/1.1 200 OK\r\nContent-Length: ${MAX_BODY_BYTES + 1}\r\n` +
            "Connection: close\r\n\r\nno",
        );
        return;
      }
      if (path === "/oversize-chunked") {
        res.writeHead(200, { "Content-Type": "application/octet-stream" });
        res.write(MAX_BINARY);
        res.end(Buffer.from([0]));
        return;
      }
      if (path === "/truncated") {
        // FIN は確実に送るが、宣言した 64 bytes のうち 7 bytes しか送らない。
        req.socket.end(
          "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n" +
            "Content-Length: 64\r\nConnection: close\r\n\r\npartial",
        );
        return;
      }
      if (path === "/followed") seen.followed += 1;
      res.writeHead(200, { "Content-Type": "text/plain" });
      res.end("followed");
    });
    server.listen(0, "127.0.0.1", () =>
      resolve({ server, port: server.address().port, seen }),
    );
  });

const waitForHealth = async (base) => {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try {
      const response = await fetch(`${base}/health`);
      if (response.ok) return;
    } catch {
      /* まだ起動していない */
    }
    await delay(50);
  }
  throw new Error("relay did not start");
};

const startNativeRelay = async () => {
  const port = await freePort();
  const base = `http://127.0.0.1:${port}`;
  const relay = spawn(
    process.execPath,
    [join(ROOT, "scripts", "relay", "server.mjs")],
    {
      cwd: ROOT,
      env: { ...process.env, NAROU_RELAY_KEY: TOKEN, PORT: String(port) },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  const log = [];
  relay.stdout.on("data", (chunk) => log.push(chunk.toString()));
  relay.stderr.on("data", (chunk) => log.push(chunk.toString()));
  const exit = once(relay, "exit");
  const close = async () => {
    relay.kill();
    await Promise.race([exit, delay(1_000)]).catch(() => {});
  };
  try {
    await waitForHealth(base);
  } catch (error) {
    await close();
    throw new Error(`${error.message}\n${log.join("")}`);
  }
  return { request: (path, options) => fetch(`${base}${path}`, options), close };
};

const startWorkerRelay = async () => {
  const base = process.env.RELAY_WORKER_BASE_URL;
  if (!base) throw new Error("RELAY_WORKER_BASE_URL must point to the actual Worker relay runtime");
  return {
    request: (path, options) => fetch(`${base}${path}`, options),
    close: async () => {},
  };
};

const paddedJson = (value, size) => {
  const base = JSON.stringify({ ...value, padding: "" });
  return JSON.stringify({ ...value, padding: "x".repeat(size - Buffer.byteLength(base)) });
};

for (const [kind, startRelay] of [["Node", startNativeRelay], ["Worker", startWorkerRelay]]) {
  test(`${kind} relay enforces the real HTTP protocol`, async (t) => {
    const upstream = await startUpstream();
    let relay;
    try {
      relay = await startRelay();
      const target = (path) => `http://127.0.0.1:${upstream.port}${path}`;
      const post = (body, extra = {}) => relay.request("/proxy", {
        method: "POST",
        headers: { "content-type": "application/json", "x-proxy-token": TOKEN },
        body,
        ...extra,
      });
      const call = (path, headers = {}) => post(JSON.stringify({ url: target(path), headers }));
      const get = (path, rawHeaders) => {
        const query = new URLSearchParams({ url: target(path) });
        if (rawHeaders !== undefined) query.set("headers", rawHeaders);
        return relay.request(`/proxy?${query}`, { headers: { "x-proxy-token": TOKEN } });
      };

      await t.test("all cookies remain usable for login and header order is preserved", async () => {
        const dup = await call("/dup", { "x-multi": ["first", "second"] });
        assert.equal(dup.status, 200);
        const result = await dup.json();
        assert.equal(result.status, 200);
        assert.deepEqual(result.setCookie, ["sid=1; Path=/", "over18=yes"]);
        assert.equal(upstream.seen.multi, "first, second");

        const anonymous = await call("/account");
        assert.equal((await anonymous.json()).status, 401);
        const account = await call("/account", {
          cookie: result.setCookie.map((value) => value.split(";")[0]).join("; "),
        });
        assert.equal(account.status, 200);
        const loggedIn = await account.json();
        assert.equal(loggedIn.status, 200);
        assert.equal(Buffer.from(loggedIn.body, "base64").toString("utf8"), "signed in");
      });

      await t.test("redirects are returned without a second upstream request", async () => {
        const response = await call("/redirect");
        assert.equal(response.status, 200);
        const result = await response.json();
        assert.equal(result.status, 302);
        assert.equal(result.location, "/followed");
        assert.equal(upstream.seen.followed, 0);
      });

      await t.test("an informational response cannot masquerade as a completed fetch", async () => {
        const response = await call("/interim");
        const result = await response.json();
        if (response.status === 502) {
          assert.equal(Object.hasOwn(result, "body"), false);
          assert.equal(Object.hasOwn(result, "status"), false);
        } else {
          assert.equal(response.status, 200);
          assert.equal(result.status, 200);
          assert.deepEqual(result.setCookie, ["after-100=1"]);
          assert.equal(Buffer.from(result.body, "base64").toString("utf8"), "after");
        }
      });

      await t.test("GET decodes UTF-8 JSON before sending headers", async () => {
        // Latin-1 の HTTP 値でも、JSON を base64 化する前は UTF-8 である。
        // HTTP の生ヘッダを UTF-8 として復元し、JSON の二重変換を検出する。
        const raw = Buffer.from(JSON.stringify({
          "x-label": "café",
          "x-multi": ["première", "deuxième"],
        }), "utf8").toString("base64");
        const response = await get("/fetch-only/utf8", raw);
        assert.equal(response.status, 200);
        assert.equal((await response.json()).status, 200);
        assert.equal(Buffer.from(upstream.seen.utf8, "latin1").toString("utf8"), "café");
        assert.equal(Buffer.from(upstream.seen.multi, "latin1").toString("utf8"), "première, deuxième");
      });

      await t.test("malformed GET header payloads return 400", async () => {
        const invalidUtf8 = Buffer.concat([
          Buffer.from('{"x-label":"'), Buffer.from([0xc3, 0x28]), Buffer.from('"}'),
        ]).toString("base64");
        const malformed = [
          "", "%%%", "a", "e30=!", invalidUtf8,
          ...["null", "[]", '"text"', "42", "{"].map((value) => Buffer.from(value).toString("base64")),
        ];
        for (const raw of malformed) {
          const response = await get("/dup", raw);
          assert.equal(response.status, 400, `headers=${raw}`);
          assert.match((await response.json()).error, /bad request/);
        }
        const missing = await get("/dup");
        assert.equal(missing.status, 200);
        assert.deepEqual((await missing.json()).setCookie, ["sid=1; Path=/", "over18=yes"]);
      });

      await t.test("request JSON is bounded in bytes at exactly 64 KiB", async () => {
        const value = { url: target("/dup") };
        const boundary = paddedJson(value, MAX_REQUEST_BYTES);
        assert.equal(Buffer.byteLength(boundary), MAX_REQUEST_BYTES);
        assert.equal((await post(boundary)).status, 200);
        assert.equal((await post(paddedJson(value, MAX_REQUEST_BYTES + 1))).status, 400);
        const multiByte = JSON.stringify({ ...value, padding: "日".repeat(MAX_REQUEST_BYTES / 2) });
        assert.ok(multiByte.length < MAX_REQUEST_BYTES);
        assert.ok(Buffer.byteLength(multiByte) > MAX_REQUEST_BYTES);
        assert.equal((await post(multiByte)).status, 400);

        const chunks = [Buffer.from(boundary), Buffer.from(" ")];
        const stream = new ReadableStream({
          pull(controller) {
            const chunk = chunks.shift();
            if (chunk) controller.enqueue(chunk);
            else controller.close();
          },
        });
        assert.equal((await post(stream, { duplex: "half" })).status, 400);
      });

      await t.test("malformed or non-object POST JSON returns 400, not 500", async () => {
        const invalidUtf8 = Buffer.concat([
          Buffer.from(`{"url":${JSON.stringify(target("/dup"))},"padding":"`),
          Buffer.from([0xc3, 0x28]), Buffer.from('"}'),
        ]);
        for (const body of ["null", "[]", '"text"', "42", "{", invalidUtf8]) {
          const response = await post(body);
          assert.equal(response.status, 400);
          assert.match((await response.json()).error, /bad request/);
        }
      });

      if (kind === "Node") {
        await t.test("curl fallback diagnostics never expose request credentials", async () => {
          const authorization = "Bearer relay-private-credential";
          const cookie = "sid=relay-private-session";
          const response = await call("/fetch-only/dup", { authorization, cookie });
          assert.equal(response.status, 200);
          const result = await response.json();
          assert.match(result.via, /^fetch /);
          const publicJson = JSON.stringify(result);
          assert.equal(publicJson.includes(authorization), false);
          assert.equal(publicJson.includes(cookie), false);
        });
      }

      await t.test("a full 16 MiB binary body is accepted with its headers", async () => {
        const response = await call("/binary");
        assert.equal(response.status, 200);
        const result = await response.json();
        assert.equal(result.status, 200);
        assert.ok(Buffer.from(result.body, "base64").equals(MAX_BINARY));
        assert.ok(
          Buffer.byteLength(JSON.stringify({ ...result, body: "" })) <= MAX_METADATA_BYTES,
          "the complete JSON envelope excluding base64 characters must fit the shared wire budget",
        );
        if (kind === "Node" && upstream.seen.binaryCurl > 0) {
          assert.equal(result.via, undefined, "legal maximum body plus headers must fit curl maxBuffer");
        }
        if (kind === "Node") {
          const fallback = await call("/fetch-only/binary");
          assert.equal(fallback.status, 200);
          const fetched = await fallback.json();
          assert.match(fetched.via, /^fetch /);
          assert.ok(Buffer.from(fetched.body, "base64").equals(MAX_BINARY));
        }
      });

      await t.test("decoded compressed bodies are not compared to wire Content-Length", async () => {
        const response = await call("/fetch-only/compressed");
        assert.equal(response.status, 200);
        assert.ok(Buffer.from((await response.json()).body, "base64").equals(COMPRESSED_BODY));
      });

      await t.test("declared and streamed oversized responses are rejected", async () => {
        for (const path of ["/oversize-declared", "/oversize-chunked"]) {
          const response = await call(path);
          assert.equal(response.status, 502, path);
          const result = await response.json();
          assert.match(result.error, /response too large/);
          assert.equal(result.body, undefined);
        }
      });

      await t.test("a truncated Content-Length response can never return partial success", async () => {
        const response = await call("/truncated");
        assert.equal(response.status, 502);
        const result = await response.json();
        assert.match(result.error, /fetch failed/);
        assert.equal(result.body, undefined);
      });

    } finally {
      if (relay) await relay.close();
      await new Promise((resolve) => {
        upstream.server.close(resolve);
        upstream.server.closeAllConnections();
      });
    }
  });
}

test("the shared header codec keeps UTF-8 text and repeated-value order", () => {
  const raw = Buffer.from(JSON.stringify({
    "x-first": ["日本語", "café"],
    "x-second": "last",
  })).toString("base64");
  assert.deepEqual(headerPairs(decodeHeaderJson(raw)), [
    ["x-first", "日本語"], ["x-first", "café"], ["x-second", "last"],
  ]);
});

test("the metadata allowance includes JSON escapes, UTF-8 bytes and the empty body envelope", () => {
  assert.equal(MAX_METADATA_BYTES, 458752);
  const base = { status: 200, padding: "" };
  const slack = MAX_METADATA_BYTES - Buffer.byteLength(JSON.stringify({ ...base, body: "" }));
  for (const [character, cost] of [["x", 1], ["é", 2], ["\u0001", 6]]) {
    const padding = character.repeat(Math.floor(slack / cost)) + "x".repeat(slack % cost);
    const metadata = { ...base, padding };
    assert.equal(Buffer.byteLength(JSON.stringify({ ...metadata, body: "" })), MAX_METADATA_BYTES);
    assert.doesNotThrow(() => assertResponseMetadata(metadata));
    assert.throws(() => assertResponseMetadata({ ...metadata, padding: `${padding}x` }), {
      message: `response metadata too large: limit ${MAX_METADATA_BYTES}`,
    });
  }
});

test("the shared reader cancels oversize without reading further and always unlocks", async () => {
  let reads = 0;
  let cancelled = false;
  const stream = new ReadableStream({
    pull(controller) {
      reads += 1;
      controller.enqueue(new Uint8Array(3));
    },
    cancel() { cancelled = true; },
  }, { highWaterMark: 0 });
  await assert.rejects(readBoundedBytes(stream, { limit: 4 }), /too large/);
  assert.equal(reads, 2);
  assert.equal(cancelled, true);
  assert.equal(stream.locked, false);

  let declaredReads = 0;
  let declaredCancelled = false;
  const declared = new ReadableStream({
    pull() { declaredReads += 1; },
    cancel() { declaredCancelled = true; },
  }, { highWaterMark: 0 });
  await assert.rejects(
    readBoundedBytes(declared, { limit: 4, contentLength: "5" }),
    /too large/,
  );
  assert.equal(declaredReads, 0);
  assert.equal(declaredCancelled, true);
  assert.equal(declared.locked, false);
});

test("the shared reader accepts an exact boundary and unlocks on stream errors or truncation", async () => {
  const exact = new ReadableStream({
    start(controller) {
      controller.enqueue(Uint8Array.of(0, 0xff));
      controller.enqueue(Uint8Array.of(1, 2));
      controller.close();
    },
  });
  assert.deepEqual(
    await readBoundedBytes(exact, { limit: 4, contentLength: "4" }),
    Uint8Array.of(0, 0xff, 1, 2),
  );
  assert.equal(exact.locked, false);

  const broken = new ReadableStream({ start(controller) { controller.error(new Error("broken")); } });
  await assert.rejects(readBoundedBytes(broken, { limit: 4 }), /broken/);
  assert.equal(broken.locked, false);

  const short = new ReadableStream({
    start(controller) {
      controller.enqueue(Uint8Array.of(1));
      controller.close();
    },
  });
  await assert.rejects(
    readBoundedBytes(short, { limit: 4, contentLength: "2" }),
    /truncated/,
  );
  assert.equal(short.locked, false);
});
