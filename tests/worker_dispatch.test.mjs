import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = fs.readFileSync(new URL('../.github/workflows/platform.yml', import.meta.url), 'utf8');
const lines = source.split('\n');
const checks = ['wasm', 'worker', 'worker-contract'];
const deploys = ['relay-deploy', 'worker-deploy-develop', 'worker-deploy-production'];

function condition(job) {
  const start = lines.indexOf(`  ${job}:`);
  assert(start >= 0, `missing job ${job}`);
  let index = start + 1;
  while (index < lines.length && !lines[index].startsWith('    if: ')) {
    assert(!/^  [\w-]+:/.test(lines[index]), `missing condition for ${job}`);
    index++;
  }
  const first = lines[index].slice('    if: '.length);
  if (first !== '>-') return first;
  const expression = [];
  while (lines[++index]?.startsWith('      ')) expression.push(lines[index].trim());
  return expression.join(' ');
}

function enabled(job, event, target, override, sorahost = 'T', ref = 'refs/heads/develop') {
  return vm.runInNewContext(condition(job), {
    vars: { SORAHOST: sorahost },
    github: { event_name: event, ref, ref_type: ref.startsWith('refs/tags/') ? 'tag' : 'branch' },
    inputs: { target, run_workers: override },
  });
}

test('manual Workers override is an opt-in boolean', () => {
  assert.match(source, /      run_workers:\n(?:        .*\n)*?        type: boolean\n        default: false/);
});

test('SORAHOST ordinary pushes and pull requests keep Workers disabled', () => {
  for (const event of ['push', 'pull_request']) {
    for (const job of [...checks, ...deploys]) {
      assert.equal(enabled(job, event, 'develop', true), false, `${event} ${job}`);
    }
  }
});

test('manual dispatch without opt-in keeps SORAHOST routing', () => {
  for (const job of [...checks, ...deploys]) {
    assert.equal(enabled(job, 'workflow_dispatch', 'develop', false), false, job);
  }
});

test('manual develop opt-in runs required checks and relay but not production', () => {
  for (const job of [...checks, 'relay-deploy', 'worker-deploy-develop']) {
    assert.equal(enabled(job, 'workflow_dispatch', 'develop', true), true, job);
  }
  assert.equal(enabled('worker-deploy-production', 'workflow_dispatch', 'develop', true), false);
});

test('manual production opt-in does not deploy develop', () => {
  for (const job of [...checks, 'relay-deploy', 'worker-deploy-production']) {
    assert.equal(enabled(job, 'workflow_dispatch', 'production', true), true, job);
  }
  assert.equal(enabled('worker-deploy-develop', 'workflow_dispatch', 'production', true), false);
});

test('normal Workers push routing is preserved when SORAHOST is disabled', () => {
  for (const job of [...checks, 'relay-deploy', 'worker-deploy-develop']) {
    assert.equal(enabled(job, 'push', undefined, undefined, ''), true, job);
  }
  assert.equal(enabled('worker-deploy-production', 'push', undefined, undefined, ''), false);
  assert.equal(enabled('worker-deploy-production', 'push', undefined, undefined, '', 'refs/tags/v1.0.0'), true);
});
