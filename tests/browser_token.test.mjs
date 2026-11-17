import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const source = fs.readFileSync(new URL('../src/bin/login/browser_token.js', import.meta.url), 'utf8');
function fixture(install, existing) {
  const context = vm.createContext({});
  const document = {
    readyState: 'complete',
    cookie: '',
    createElement: () => ({}),
    head: { appendChild: script => install(context, script) },
  };
  context.document = document;
  context.location = { origin: 'https://novels.example.test', href: 'https://novels.example.test/read/1' };
  if (existing) existing(context);
  const prepare = vm.runInContext(`(${source})`, context);
  return { context, document, prepare: (path, url) => prepare(path, url, 'https://novels.example.test') };
}

function integration(context) {
  const sdk = {
    getToken() {
      assert.equal(this, sdk, 'the SDK method must retain its receiver');
      context.document.cookie = 'example-token=private-token';
      return Promise.resolve('private-token');
    },
  };
  return sdk;
}

test('loads an absent SDK namespace and issues a cookie without returning its value', async () => {
  const { context, document, prepare } = fixture((context, script) => {
    assert.equal(script.src, 'https://sdk.example.test/challenge.js');
    context.Example = { Integration: integration(context) };
    script.onload();
  });
  assert.equal(context.Example, undefined);
  const result = await prepare('Example.Integration.getToken', 'https://sdk.example.test/challenge.js');
  assert.equal(document.cookie, 'example-token=private-token');
  assert.deepEqual(JSON.parse(JSON.stringify(result)), { stage: 'ok' });
});

test('uses an existing SDK without depending on another script request', async () => {
  const { document, prepare } = fixture(
    () => { throw new Error('unexpected network request'); },
    context => { context.Example = { Integration: integration(context) }; },
  );
  const result = await prepare('Example.Integration.getToken', 'https://unavailable.example.test/sdk.js');
  assert.equal(document.cookie, 'example-token=private-token');
  assert.equal(result.stage, 'ok');
});

test('does not report issuance when the SDK fails to load or publish its method', async () => {
  for (const [load, expected] of [[script => script.onerror(), 'script_failed'], [script => script.onload(), 'missing']]) {
    const { document, prepare } = fixture((_context, script) => load(script));
    const result = await prepare('Absent.Integration.getToken', 'https://sdk.example.test/challenge.js');
    assert.equal(result.stage, expected);
    assert.equal(document.cookie, '');
  }
});

test('does not issue a token in a pending or foreign document', async () => {
  const { context, document, prepare } = fixture(() => { throw new Error('must not load a script here'); });
  context.location = { href: 'about:blank', origin: 'null' };
  assert.equal((await prepare('Example.getToken', 'https://sdk.example.test/sdk.js')).stage, 'context_pending');
  context.location = { href: 'https://other.example.test/', origin: 'https://other.example.test' };
  assert.equal((await prepare('Example.getToken', 'https://sdk.example.test/sdk.js')).stage, 'wrong_origin');
  assert.equal(document.cookie, '');
});
