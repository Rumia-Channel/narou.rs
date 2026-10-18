import { test } from "node:test";
import assert from "node:assert/strict";
import { createTargetResolver, RelayTargetError } from "../scripts/relay/security.mjs";

const publicRecord = { address: "8.8.8.8", family: 4 };
const policyFailure = (status) => (error) => error instanceof RelayTargetError && error.status === status;

test("allowed hosts use exact case-insensitive IDN and trailing-dot normalization", async () => {
  const seen = [];
  const resolve = createTargetResolver(" BÜCHER.example. , example.com ", {
    lookup: async (host) => { seen.push(host); return [publicRecord]; },
  });
  const target = await resolve("https://BÜCHER.example./chapter?part=1");
  assert.equal(target.url.hostname, "xn--bcher-kva.example");
  assert.equal(target.url.pathname, "/chapter");
  assert.equal(target.url.search, "?part=1");
  await resolve("https://example.com。/chapter");
  assert.deepEqual(seen, ["xn--bcher-kva.example", "example.com"]);
});

test("an empty or nonmatching host list rejects before DNS", async () => {
  let lookups = 0;
  const lookup = async () => { lookups += 1; return [publicRecord]; };
  for (const hosts of ["", "example.com"]) {
    const resolve = createTargetResolver(hosts, { lookup });
    for (const url of ["https://sub.example.com/", "https://notexample.com/", "https://example.com.evil/"]) {
      await assert.rejects(resolve(url), policyFailure(400));
    }
  }
  assert.equal(lookups, 0);
});

test("wildcards, ports and URLs cannot expand the host allowlist", () => {
  for (const entry of ["*.example.com", "https://example.com", "example.com:443", "user@example.com", "example.com/", "example..com"]) {
    assert.throws(() => createTargetResolver(entry), policyFailure(400));
  }
});

test("unsupported schemes and URL user information are rejected without DNS", async () => {
  const resolve = createTargetResolver("example.com", { lookup: async () => { throw new Error("unexpected lookup"); } });
  for (const url of ["file:///etc/passwd", "ftp://example.com/a", "http://user@example.com/a", "https://u:p@example.com/?private-query", "not a URL"]) {
    await assert.rejects(resolve(url), policyFailure(400));
  }
});

test("public literal addresses keep their validated destination without DNS", async () => {
  const resolve = createTargetResolver("8.8.8.8,[2606:4700:4700::1111]", {
    lookup: async () => { throw new Error("literal must not be looked up"); },
  });
  const v4 = await resolve("https://8.8.8.8/a");
  const v6 = await resolve("https://[2606:4700:4700::1111]/b");
  assert.deepEqual([v4.address, v4.family], ["8.8.8.8", 4]);
  assert.deepEqual([v6.address, v6.family], ["2606:4700:4700::1111", 6]);
});

test("nonpublic and embedded-address spellings cannot bypass the policy", async () => {
  const destinations = [
    "0.0.0.0", "10.0.0.1", "127.0.0.1", "127.1", "2130706433", "0x7f000001",
    "100.64.0.1", "169.254.169.254", "172.16.0.1", "172.31.255.255", "192.168.1.1",
    "192.0.0.1", "192.0.2.1", "198.18.0.1", "198.51.100.1", "203.0.113.1", "224.0.0.1", "255.255.255.255",
    "[::]", "[::1]", "[fc00::1]", "[fe80::1]", "[ff02::1]", "[2001:db8::1]",
    "[::ffff:127.0.0.1]", "[::ffff:10.0.0.1]", "[::127.0.0.1]", "[64:ff9b::7f00:1]", "[2002:7f00:1::]",
    "[2001::1]", "[2001:4860::5efe:7f00:1]",
  ];
  for (const destination of destinations) {
    const url = new URL(`http://${destination}/`);
    const resolve = createTargetResolver(url.hostname);
    await assert.rejects(resolve(url.href), policyFailure(400));
  }
});

test("every DNS answer is checked, not just a first public answer", async () => {
  for (const records of [
    [publicRecord, { address: "169.254.169.254", family: 4 }],
    [{ address: "::1", family: 6 }, publicRecord],
    [{ address: "8.8.8.8", family: 6 }],
  ]) {
    const resolve = createTargetResolver("example.com", { lookup: async () => records });
    await assert.rejects(resolve("https://example.com/"), policyFailure(400));
  }
});

test("a later DNS rebind is not authorized by an earlier successful lookup", async () => {
  let lookups = 0;
  const resolve = createTargetResolver("example.com", {
    lookup: async () => ++lookups === 1 ? [publicRecord] : [{ address: "127.0.0.1", family: 4 }],
  });
  const first = await resolve("https://example.com/one");
  assert.equal(first.address, "8.8.8.8");
  await assert.rejects(resolve("https://example.com/two"), policyFailure(400));
  assert.equal(lookups, 2);
});

test("lookup failures do not expose URLs, query values or raw diagnostics", async () => {
  const secret = "secret-query-cookie-authorization";
  const resolve = createTargetResolver("example.com", { lookup: async () => { throw new Error(secret); } });
  await assert.rejects(resolve(`https://example.com/?token=${secret}`), (error) => {
    assert.equal(error.status, 502);
    assert.equal(String(error).includes(secret), false);
    return true;
  });
});

test("cancellation is checked before and after DNS without starting another lookup", async () => {
  const controller = new AbortController();
  let calls = 0;
  const resolve = createTargetResolver("example.com", {
    lookup: async () => { calls += 1; controller.abort(); return [publicRecord]; },
  });
  await assert.rejects(resolve("https://example.com/", { signal: controller.signal }), policyFailure(502));
  await assert.rejects(resolve("https://example.com/", { signal: controller.signal }), policyFailure(502));
  assert.equal(calls, 1);
});
