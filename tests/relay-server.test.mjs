import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { createServer as createHttpsServer } from "node:https";
import { once } from "node:events";
import { setTimeout as delay } from "node:timers/promises";
import { createRelayServer, startRelay } from "../scripts/relay/server.mjs";
import { resolveLocalTarget } from "./relay-fixture.mjs";
import { key, cert } from "./relay-tls-fixture.mjs";

const TOKEN = "relay-private-test-key";
const listen = async (server) => {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return server.address().port;
};
const close = (server) => new Promise((resolve) => { server.close(resolve); server.closeAllConnections(); });
const post = (base, target, headers = {}, options = {}) => fetch(`${base}/proxy`, {
  method: "POST", headers: { "content-type": "application/json", "x-proxy-token": TOKEN },
  body: JSON.stringify({ url: target, headers }), ...options,
});
const fixture = async (handler, options = {}, secure = false) => {
  const upstream = secure ? createHttpsServer({ key, cert }, handler) : createServer(handler);
  const upstreamPort = await listen(upstream);
  const logs = [];
  const relay = createRelayServer({ token: TOKEN, resolveTarget: resolveLocalTarget, logger: (line) => logs.push(line), ...options });
  const relayPort = await listen(relay);
  return {
    upstream, relay, logs, base: `http://127.0.0.1:${relayPort}`,
    target: `${secure ? "https" : "http"}://relay.invalid:${upstreamPort}/`,
    close: async () => { await close(relay); await close(upstream); },
  };
};
const until = async (event) => {
  const timeout = new AbortController();
  try {
    await Promise.race([event, delay(3000, null, { signal: timeout.signal }).then(() => { throw new Error("expected socket lifecycle event"); })]);
  } finally { timeout.abort(); }
};

test("K_SERVICE selects the public Cloud Run listener without overriding PORT", async () => {
  for (const [service, expectedHost] of [["narou-relay", "0.0.0.0"], ["", "127.0.0.1"]]) {
    const server = startRelay({ env: { K_SERVICE: service, PORT: "0", NAROU_RELAY_KEY: TOKEN } });
    try {
      await once(server, "listening");
      assert.equal(server.address().address, expectedHost);
      const response = await fetch(`http://127.0.0.1:${server.address().port}/health`);
      assert.equal(response.status, 200);
      assert.deepEqual(await response.json(), { ok: true });
    } finally { await close(server); }
  }
});

test("production startup and policy do not inherit localhost test exceptions", async () => {
  assert.throws(() => startRelay({ env: { K_SERVICE: "narou-relay", PORT: "0" } }));
  for (const hosts of ["", "localhost,127.0.0.1"]) {
    const server = startRelay({ env: {
      K_SERVICE: "narou-relay", PORT: "0", NAROU_RELAY_KEY: TOKEN, NAROU_RELAY_ALLOWED_HOSTS: hosts,
      NAROU_RELAY_ALLOW_PRIVATE: "1", // 意図的な無効設定。production にこの機能は無い。
    } });
    try {
      await once(server, "listening");
      const response = await post(`http://127.0.0.1:${server.address().port}`, "http://127.0.0.1:1/");
      assert.equal(response.status, 400);
      assert.equal(Object.hasOwn(await response.json(), "body"), false);
    } finally { await close(server); }
  }
});

test("route, method, authentication and malformed input keep distinct relay statuses", async () => {
  const f = await fixture((_req, res) => res.end("ok"));
  try {
    assert.equal((await fetch(`${f.base}/missing`)).status, 404);
    assert.equal((await fetch(`${f.base}/health`, { method: "POST" })).status, 405);
    assert.equal((await fetch(`${f.base}/proxy`, { method: "PATCH", headers: { "x-proxy-token": TOKEN } })).status, 405);
    for (const token of ["", TOKEN.slice(0, -1), `${TOKEN}x`, `${TOKEN},${TOKEN}`]) {
      assert.equal((await fetch(`${f.base}/proxy`, { headers: { "x-proxy-token": token } })).status, 403);
    }
    const bad = await fetch(`${f.base}/proxy`, {
      method: "POST", headers: { "x-proxy-token": TOKEN }, body: '{"private":"do-not-echo",',
    });
    assert.equal(bad.status, 400);
    assert.equal((await bad.text()).includes("do-not-echo"), false);
    assert.equal((await post(f.base, f.target, { "x-invalid\r\n": "header" })).status, 400);
    assert.equal((await post(f.base, f.target, { Host: "metadata.google.internal" })).status, 400);
  } finally { await f.close(); }
});

test("a valid curl HTTP403 is not retried using a different backend", async () => {
  let requests = 0;
  const f = await fixture((req, res) => {
    requests += 1;
    const curl = req.headers["user-agent"]?.startsWith("curl/");
    res.writeHead(curl ? 403 : 200, { "content-type": "text/plain" });
    res.end(curl ? "permission denied" : "unwanted fallback");
  });
  try {
    const response = await post(f.base, f.target);
    assert.equal(response.status, 200);
    const result = await response.json();
    assert.equal(result.status, 403);
    assert.equal(result.via, "curl");
    assert.equal(Buffer.from(result.body, "base64").toString(), "permission denied");
    assert.equal(requests, 1);
  } finally { await f.close(); }
});

test("curl unavailability uses only the same pinned HTTP destination without leaking diagnostics", async () => {
  const secret = "private-query-cookie-authorization";
  let received;
  const f = await fixture((req, res) => {
    received = { cookie: req.headers.cookie, authorization: req.headers.authorization, host: req.headers.host };
    res.end("safe body");
  }, { curlBin: `missing-curl-${secret}` });
  try {
    const response = await post(f.base, `${f.target}?token=${secret}`, { cookie: secret, authorization: secret });
    assert.equal(response.status, 200);
    const result = await response.json();
    assert.equal(result.via, "node-http");
    assert.equal(Buffer.from(result.body, "base64").toString(), "safe body");
    assert.equal(received.cookie, secret);
    assert.equal(received.authorization, secret);
    assert.ok(received.host.startsWith("relay.invalid:"));
    assert.equal(JSON.stringify(result).includes(secret), false);
    for (const value of [secret, TOKEN, f.target]) assert.equal(f.logs.join("\n").includes(value), false);
  } finally { await f.close(); }
});

test("a response beginning with an HTTP status line is still preserved as body", async () => {
  const body = "HTTP/1.1 403 this is novel text\r\n\r\nbody after that";
  const f = await fixture((_req, res) => res.end(body));
  try {
    const response = await post(f.base, f.target);
    assert.equal(Buffer.from((await response.json()).body, "base64").toString(), body);
  } finally { await f.close(); }
});

test("internal failures return500 without exposing raw exception, query or credentials", async () => {
  const secret = "private-internal-diagnostic";
  const f = await fixture((_req, res) => res.end("must not arrive"), {
    resolveTarget: async () => { throw new Error(secret); },
  });
  try {
    const response = await post(f.base, `${f.target}?${secret}`, { cookie: secret, authorization: secret });
    assert.equal(response.status, 500);
    assert.equal((await response.text()).includes(secret), false);
    assert.equal(f.logs.join("\n").includes(secret), false);
  } finally { await f.close(); }
});

for (const backend of ["curl", "node-http"]) {
  test(`${backend} rejects untrusted TLS before sending HTTP credentials`, async () => {
    let requests = 0;
    const f = await fixture((_req, res) => { requests += 1; res.end("must not arrive"); },
      backend === "node-http" ? { curlBin: "missing-relay-test-curl" } : {}, true);
    try {
      const secret = "private-tls-cookie";
      const response = await post(f.base, `${f.target}?token=${secret}`, { cookie: secret });
      assert.equal(response.status, 502);
      assert.equal((await response.text()).includes(secret), false);
      assert.equal(requests, 0);
      assert.ok(f.logs.some((line) => line.includes("tls-verification")));
      assert.equal(f.logs.join("\n").includes(secret), false);
    } finally { await f.close(); }
  });
}

test("both backend timeouts close their upstream sockets and return no partial success", { timeout: 5000 }, async () => {
  let requests = 0;
  let closed = 0;
  let allClosed;
  const closedEvent = new Promise((resolve) => { allClosed = resolve; });
  const f = await fixture((req) => {
    requests += 1;
    req.socket.once("close", () => { if (++closed === 2) allClosed(); });
  }, { curlTimeoutMs: 300, fallbackTimeoutMs: 300, requestTimeoutMs: 2000 });
  try {
    const response = await post(f.base, f.target);
    assert.equal(response.status, 502);
    assert.equal(Object.hasOwn(await response.json(), "body"), false);
    await until(closedEvent);
    assert.equal(requests, 2);
  } finally { await f.close(); }
});

test("the overall deadline kills an active curl without starting fallback", { timeout: 5000 }, async () => {
  let requests = 0;
  let onClosed;
  const closed = new Promise((resolve) => { onClosed = resolve; });
  const f = await fixture((req) => { requests += 1; req.socket.once("close", onClosed); },
    { curlTimeoutMs: 5000, fallbackTimeoutMs: 5000, requestTimeoutMs: 500 });
  try {
    assert.equal((await post(f.base, f.target)).status, 502);
    await until(closed);
    assert.equal(requests, 1);
  } finally { await f.close(); }
});

test("client disconnect cancels active curl and cannot cause a later retry", { timeout: 5000 }, async () => {
  let ready;
  let onClosed;
  let requests = 0;
  const connected = new Promise((resolve) => { ready = resolve; });
  const closed = new Promise((resolve) => { onClosed = resolve; });
  const f = await fixture((req) => { requests += 1; req.socket.once("close", onClosed); ready(); });
  const controller = new AbortController();
  try {
    const request = post(f.base, f.target, {}, { signal: controller.signal });
    await until(connected);
    controller.abort();
    await assert.rejects(request);
    await until(closed);
    assert.equal(requests, 1);
  } finally { controller.abort(); await f.close(); }
});

test("a DNS result arriving after the deadline cannot initiate a new connection", { timeout: 5000 }, async () => {
  let release;
  let requests = 0;
  const f = await fixture((_req, res) => { requests += 1; res.end("must not arrive"); }, {
    requestTimeoutMs: 100,
    resolveTarget: (raw) => new Promise((resolve) => { release = async () => resolve(await resolveLocalTarget(raw)); }),
  });
  try {
    assert.equal((await post(f.base, f.target)).status, 502);
    await release();
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(requests, 0);
  } finally { await f.close(); }
});
