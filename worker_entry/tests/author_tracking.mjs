// Build Worker first, then node tests/author_tracking.mjs. All D1 is isolated/local.
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { once } from 'node:events';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
mkdirSync(join(root, '.wrangler'), { recursive: true });
const work = mkdtempSync(join(root, '.wrangler', 'author-smoke-'));
const config = join(work, 'wrangler.toml');
const persist = join(work, 'state');
const token = 'isolated-author-smoke-token';
const windows = process.platform === 'win32';
const npx = windows ? 'npx.cmd' : 'npx';
let child, port, sourcePort, logs = '', workCount = 2, authorHits = 0, pause;
async function source(req, res) {
  const path = new URL(req.url, 'http://localhost').pathname;
  res.setHeader('Content-Type', 'text/html; charset=utf-8');
  if (path === '/authors/7/') {
    authorHits++;
    if (pause) { pause.seen(); await pause.wait; }
    res.end(Array.from({ length: workCount }, (_, i) => `<a href="http://localhost:${sourcePort}/work${i + 1}/">作品 ${i + 1}</a>`).join('\n'));
  } else if (path === '/authors/8/') {
    res.end(`<a href="http://localhost:${sourcePort}/oneshot1/">一括取得だけ</a>`);
  } else if (/^\/(?:work|oneshot)\d+\/$/.test(path)) {
    const ncode = path.split('/')[1];
    res.end(`<h1>Controlled ${ncode}</h1><span class="author">Author Fixture</span><a href="/${ncode}/1/">第一話</a>`);
  } else if (/^\/(?:work|oneshot)\d+\/1\/$/.test(path)) {
    res.end(`<div class="body"><p>${'継続追跡で取得する本文です。'.repeat(100)}</p></div>`);
  } else { res.statusCode = 404; res.end('not found'); }
}
const server = http.createServer((req, res) => { source(req, res).catch((error) => { res.statusCode = 500; res.end(String(error)); }); });
const ipv6 = http.createServer((req, res) => { source(req, res).catch((error) => { res.statusCode = 500; res.end(String(error)); }); });
function run(args) {
  const result = spawnSync(npx, ['wrangler', ...args], { cwd: root, shell: windows, encoding: 'utf8', env: { ...process.env, CI: 'true', WRANGLER_SEND_METRICS: 'false' } });
  if (result.status !== 0) throw new Error(`wrangler failed: ${result.stderr}\n${result.stdout}`);
  return result.stdout;
}
function sql(command) {
  const path = join(work, 'query.sql'); writeFileSync(path, command);
  const out = run(['d1', 'execute', 'DB', '--local', '--config', config, '--persist-to', persist, '--file', path, '--json']);
  return JSON.parse(out.slice(out.indexOf('[\n')))[0].results;
}
const count = (command) => Object.values(sql(command)[0])[0];
async function api(path, body, method = body === undefined ? 'GET' : 'POST') {
  return fetch(`http://127.0.0.1:${port}${path}`, { method, headers: { Authorization: `Bearer ${token}`, 'Content-Type': typeof body === 'string' ? 'application/yaml' : 'application/json', Connection: 'close' }, body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body) });
}
async function json(path, body, method) {
  const response = await api(path, body, method); const value = await response.json();
  assert(response.ok && value.success !== false, `${path}: HTTP ${response.status}: ${JSON.stringify(value)}`); return value;
}
async function jobs(ids, expected = 'succeeded') {
  for (const id of ids) {
    const deadline = Date.now() + 120000;
    let job;
    do {
      job = await json(`/api/jobs/${id}`);
      if (['succeeded', 'permanent', 'blocked'].includes(job.status)) break;
      await new Promise((resolve) => setTimeout(resolve, 300));
    } while (Date.now() < deadline);
    assert.equal(job.status, expected, JSON.stringify(job));
  }
}
async function settled() {
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) {
    const active = sql("SELECT job_id FROM worker_jobs WHERE status IN ('pending','running','retryable') ORDER BY created_at");
    if (active.length === 0) return;
    await jobs(active.map((row) => row.job_id));
  }
  throw new Error('author fan-out and follow-up conversions did not settle');
}
async function update() {
  const result = await json('/api/update', { update_all: true });
  await jobs([...result.job_ids, ...(result.author_job_ids ?? [])]); await settled(); return result;
}
async function setting(values, expected) { return json('/api/global_setting', { settings: values, expected }); }
async function start() {
  logs = '';
  child = spawn(npx, ['wrangler', 'dev', '--config', config, '--persist-to', persist, '--port', String(port), '--test-scheduled'], { cwd: root, shell: windows, detached: !windows, stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, CI: 'true', WRANGLER_SEND_METRICS: 'false' } });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', (chunk) => { logs = (logs + chunk).slice(-150000); });
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`worker exited: ${logs}`);
    try { if ((await fetch(`http://127.0.0.1:${port}/health/live`, { headers: { Connection: 'close' } })).ok) return; } catch {}
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`Worker not ready: ${logs}`);
}
async function stop() {
  if (!child || child.exitCode !== null) return;
  const exited = once(child, 'exit');
  if (windows) spawnSync('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
  else {
    try { process.kill(-child.pid, 'SIGTERM'); }
    catch (error) { if (error.code !== 'ESRCH') throw error; }
  }
  await exited; child = undefined;
}
try {
  server.listen(0, '127.0.0.1'); await once(server, 'listening'); sourcePort = server.address().port;
  ipv6.listen(sourcePort, '::1'); await once(ipv6, 'listening');
  const probe = http.createServer(); probe.listen(0, '127.0.0.1'); await once(probe, 'listening'); port = probe.address().port; await new Promise((resolve) => probe.close(resolve));
  let toml = readFileSync(join(root, 'wrangler.toml'), 'utf8').replace(/\[build\][^\[]*/, '');
  toml = toml.replace(/^main = .*$/m, `main = ${JSON.stringify(join(root, 'build/worker/shim.mjs').replaceAll('\\', '/'))}`)
    .replace(/^directory = "public"$/m, `directory = ${JSON.stringify(join(root, 'public').replaceAll('\\', '/'))}`)
    .replace(/^migrations_dir = "migrations"$/m, `migrations_dir = ${JSON.stringify(join(root, 'migrations').replaceAll('\\', '/'))}`);
  writeFileSync(config, toml);
  writeFileSync(join(work, '.dev.vars'), `NAROU_ADMIN_TOKEN=${token}\nNAROU_RS_LOGIN_KEY=${Buffer.alloc(32, 7).toString('base64')}\nS3_ACCESS_KEY_ID=local-test-key\nS3_SECRET_ACCESS_KEY=local-test-secret\n`);
  run(['d1', 'migrations', 'apply', 'DB', '--local', '--config', config, '--persist-to', persist]);
  // Author scenarios intentionally use legacy D1, independent of S3-default coverage.
  sql("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','content_backend','\"d1\"','d1');");
  // Old records may lack attributes or contain an ordinary tag named 漫画.
  // Classify both from effective YAML before indexed tag queries.
  sql(`INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,tags_json,ncode,ncode_fold,domain,domain_fold)
    VALUES (900,'A','a','Artwork','artwork','artwork','https://www.pixiv.net/ajax/illust/900','https://www.pixiv.net/ajax/illust/900','Pixiv','pixiv','2026-01-01T00:00:00Z','[]','a900','a900','www.pixiv.net','www.pixiv.net'),
           (901,'A','a','Novel','novel','novel','https://www.pixiv.net/ajax/novel/901','https://www.pixiv.net/ajax/novel/901','Pixiv','pixiv','2026-01-01T00:00:00Z','["漫画"]','n901','n901','www.pixiv.net','www.pixiv.net');
    INSERT INTO novel_tags(novel_id,position,tag,tag_fold) VALUES (901,0,'漫画','漫画');`);
  await start();
  assert((await api('/api/tag_list?format=json')).ok, 'legacy attribute migration must complete');
  assert.deepEqual(sql('SELECT ncode,tags_json,attribute_tag FROM novels ORDER BY id').map((row) => [row.ncode, JSON.parse(row.tags_json), row.attribute_tag]), [
    ['a900', [], '漫画'], ['n901', [], '小説'],
  ]);
  assert.deepEqual(sql('SELECT tag FROM novel_tags ORDER BY novel_id').map((row) => row.tag), []);
  let viewConfig = await json('/api/webui/config?fresh=1');
  assert.equal(viewConfig.attribute_tags_enabled, false);
  assert.deepEqual((await json('/api/tag_list?format=json')).tags, []);
  assert.equal((await json(`/api/list?filter=${encodeURIComponent('tag:漫画')}`)).records_filtered, 0);
  await setting({'tag.attributes-enabled': true}, {'tag.attributes-enabled': null});
  assert((await json('/api/tag_list?format=json')).tags.includes('漫画'));
  assert.equal((await json(`/api/list?filter=${encodeURIComponent('tag:漫画')}`)).records_filtered, 1);
  await setting({'tag.attributes-enabled': false}, {'tag.attributes-enabled': true});
  assert.equal((await json(`/api/list?filter=${encodeURIComponent('tag:漫画')}`)).records_filtered, 0);
  assert.equal(sql('SELECT attribute_tag FROM novels WHERE id=900')[0].attribute_tag, '漫画');
  assert.equal(viewConfig.show_novel_attribute, false);
  assert.equal(viewConfig.show_manga_attribute, true);
  await setting({'tag.show-novel': true, 'tag.show-manga': false}, {'tag.show-novel': null, 'tag.show-manga': null});
  viewConfig = await json('/api/webui/config?fresh=1');
  assert.equal(viewConfig.show_novel_attribute, true);
  assert.equal(viewConfig.show_manga_attribute, false);
  await setting({'tag.show-novel': null, 'tag.show-manga': null}, {'tag.show-novel': true, 'tag.show-manga': false});
  sql('DELETE FROM novels WHERE id IN (900,901)');
  const authorUrl = `http://localhost:${sourcePort}/authors/7/`;
  const storedUrl = authorUrl + '#works';
  const definition = `name: Controlled Author Source\ndomain: localhost\ntop_url: http://localhost:${sourcePort}\nurl: 'http://localhost:${sourcePort}/(?<ncode>(?:work|oneshot)\\d+)/?'\nsitename: Controlled Author Source\nversion: 1.0\nconfirm_over18: no\ntoc_url: 'http://localhost:${sourcePort}/\\k<ncode>/'\nauthor_url: '^http://(?:localhost|authors\\.example):${sourcePort}/authors/(?<user_id>\\d+)/?'\nauthor_api_url: 'http://localhost:${sourcePort}/authors/\\k<user_id>/'\nauthor_novel_pattern: '(?<novel_url>http://localhost:${sourcePort}/(?:work|oneshot)\\d+/)'\ntitle: '<h1>(?<title>.+?)</h1>'\nauthor: '<span class="author">(?<author>.+?)</span>'\nsubtitles: '<a href="(?<href>/(?:work|oneshot)\\d+/(?<index>\\d+)/)">(?<subtitle>.+?)</a>'\nbody_pattern: '<div class="body">(?<body>.+?)</div>'\nmin_interval: 0.01\n`;
  const workTypeRule = (work) => `\nwork_type_rules:\n  - field: source\n    match: '^category::comic$'\n    type: manga\npreprocess: |-\n  let kind = extract(/<h1>Controlled ([^<]+)<\\/h1>/)\n  if kind == "${work}"\n    emit "\\ncategory::comic\\n"\n    insert_at_match\n  end\n`;
  await json('/api/sites/localhost.yaml', definition + workTypeRule('work1'), 'PUT');
  assert.equal((await fetch(`http://127.0.0.1:${port}/api/authors`, { headers: { Connection: 'close' } })).status, 401);
  assert.equal((await api('/api/authors', { url: `http://localhost:${sourcePort}/work1/` })).status, 400);
  const extra = [9, 10].map((id) => `http://localhost:${sourcePort}/authors/${id}/`);
  const registered = await Promise.all(extra.map((url) => json('/api/authors', { url })));
  assert(registered.every((result) => result.changed));
  assert.deepEqual(new Set((await json('/api/authors')).authors.map((author) => author.url)), new Set(extra));
  for (const url of extra) await json('/api/authors', { url }, 'DELETE');
  assert.equal((await json('/api/authors', { url: storedUrl })).changed, true);
  assert.equal((await json('/api/authors', { url: storedUrl })).changed, false);
  assert.deepEqual((await json('/api/authors')).authors, [{ site: 'localhost', url: storedUrl }]);
  await stop(); await start();
  assert.deepEqual((await json('/api/authors')).authors, [{ site: 'localhost', url: storedUrl }]);
  const first = await update(); assert.equal(first.count, 0); assert.equal(first.author_job_ids.length, 1);
  assert.deepEqual(sql('SELECT ncode FROM novels ORDER BY ncode').map((row) => row.ncode), ['work1', 'work2']);
  let tagged = sql('SELECT id, ncode, tags_json, attribute_tag FROM novels ORDER BY ncode');
  assert.deepEqual(JSON.parse(tagged[0].tags_json), []);
  assert.equal(tagged[0].attribute_tag, '漫画');
  assert.deepEqual(JSON.parse(tagged[1].tags_json), []);
  assert.equal(tagged[1].attribute_tag, '小説');
  assert.equal((await api(`/api/novels/${tagged[0].id}/tags/remove`, { tags: ['漫画'] })).status, 400);
  assert.equal((await api(`/api/novels/${tagged[1].id}/tags`, { tags: ['漫画'] })).status, 400);
  await json(`/api/novels/${tagged[1].id}/tags`, { tags: ['手動'] });
  await json('/api/sites/localhost.yaml', definition + workTypeRule('work2'), 'PUT');
  console.log('PASS author registration persists across restart; empty-library update discovers and downloads works');
  workCount = 3; await update();
  assert.deepEqual(sql('SELECT ncode FROM novels ORDER BY ncode').map((row) => row.ncode), ['work1', 'work2', 'work3']);
  tagged = sql('SELECT ncode, tags_json, attribute_tag FROM novels ORDER BY ncode');
  assert.deepEqual(JSON.parse(tagged[0].tags_json), []);
  assert.equal(tagged[0].attribute_tag, '小説');
  assert.deepEqual(JSON.parse(tagged[1].tags_json), ['手動']);
  assert.equal(tagged[1].attribute_tag, '漫画');
  const downloads = count("SELECT COUNT(*) FROM worker_jobs WHERE kind='download'"); await update();
  assert.equal(count("SELECT COUNT(*) FROM worker_jobs WHERE kind='download'"), downloads, 'known works must not create new download jobs');
  console.log('PASS later publications are discovered automatically; known works do not create duplicate downloads');
  await setting({ 'update.disabled-author-tracking': true }, { 'update.disabled-author-tracking': null });
  workCount = 4; const hitsBefore = authorHits; const disabled = await update();
  assert.deepEqual(disabled.author_job_ids, []); assert.equal(authorHits, hitsBefore); assert.equal(count('SELECT COUNT(*) FROM novels'), 3);
  const dry = await json('/api/authors/check', { dry_run: true });
  assert.deepEqual(dry.urls, [`http://localhost:${sourcePort}/work4/`]); assert.equal(count('SELECT COUNT(*) FROM novels'), 3);
  const manual = await json('/api/authors/check', { dry_run: false }); await jobs(manual.job_ids); await settled();
  assert.equal(count('SELECT COUNT(*) FROM novels'), 4);
  console.log('PASS auto-disable is supported; dry-run does not download; explicit author check still discovers works');
  await setting({ 'update.disabled-author-tracking': false, 'update.auto-schedule.enable': true, 'update.auto-schedule': '0000', 'update.auto-schedule.timezone': 'UTC' }, { 'update.disabled-author-tracking': true, 'update.auto-schedule.enable': null, 'update.auto-schedule': null, 'update.auto-schedule.timezone': null });
  workCount = 5;
  await json('/api/sites/localhost.yaml', definition + workTypeRule('work2')
    + "\nupdate_rules:\n  - match: {ncode: '^work'}\n    automatic: skip\n", 'PUT');
  const updatesBeforeCron = count("SELECT COUNT(*) FROM worker_jobs WHERE kind='update'");
  let cron = await api('/__scheduled?cron=*+*+*+*+*');
  if (cron.status === 404) cron = await api('/cdn-cgi/handler/scheduled?cron=*+*+*+*+*');
  assert(cron.ok, await cron.text()); await settled();
  assert.equal(count('SELECT COUNT(*) FROM novels'), 5, 'cron must discover the fifth work');
  assert.equal(count("SELECT COUNT(*) FROM worker_jobs WHERE kind='update'"), updatesBeforeCron,
    'excluding every existing work must not prevent tracked authors from adding a new work');
  console.log('PASS actual scheduled handler skips excluded existing works and still downloads new tracked-author publications');
  workCount = 6;
  let release, reached; const wait = new Promise((resolve) => { release = resolve; }); const seen = new Promise((resolve) => { reached = resolve; }); pause = { wait, seen: reached, release };
  const pending = await json('/api/authors/check', { dry_run: false });
  let timer;
  try { await Promise.race([seen, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('author listing did not start')), 30000); })]); }
  finally { clearTimeout(timer); }
  const removed = await json('/api/authors', { url: storedUrl }, 'DELETE'); assert.equal(removed.changed, true);
  release(); pause = undefined; await jobs(pending.job_ids); await settled();
  assert.equal(count('SELECT COUNT(*) FROM novels'), 5); assert.deepEqual((await json('/api/authors')).authors, []);
  const afterRemoval = authorHits;
  const skipped = await json('/api/jobs', { kind: 'AuthorCheck', targets: [authorUrl], options: [] }); await jobs(skipped.ids);
  assert.equal(authorHits, afterRemoval, 'removed author check must not fetch even if a stale job is delivered');
  console.log('PASS removing an author stops in-flight fan-out and stale queued checks, preserving downloaded novels');
  // R18's author host differs from its novel host; expansion must not require them to match.
  await json('/api/sites/localhost.yaml', definition.replace('confirm_over18: no', 'confirm_over18: yes'), 'PUT');
  await setting({ over18: false }, { over18: null });
  const denied = await json('/api/download', { targets: [`http://authors.example:${sourcePort}/authors/8/`] });
  await jobs(denied.results.map((row) => row.job_id), 'blocked');
  assert.equal(count("SELECT COUNT(*) FROM novels WHERE ncode='oneshot1'"), 0);
  await setting({ over18: true }, { over18: false });
  const one = await json('/api/download', { targets: [`http://authors.example:${sourcePort}/authors/8/`] }); await jobs(one.results.map((row) => row.job_id));
  assert.deepEqual((await json('/api/authors')).authors, []); assert.equal(count("SELECT COUNT(*) FROM novels WHERE ncode='oneshot1'"), 1);
  await settled();
  console.log('PASS explicit adult consent unblocks terminal-free downloads; cross-host one-shot authors remain untracked');
  if (process.env.AUTHOR_SMOKE_KEEP === '1') {
    console.log(`AUTHOR_WORKER_READY http://127.0.0.1:${port}/settings token=${token} author=${storedUrl}`);
    process.stdin.resume(); await once(process.stdin, 'data'); process.stdin.pause();
  }
} catch (error) { console.error(logs.slice(-25000)); throw error; }
finally {
  if (pause) pause.release();
  await stop();
  await Promise.all([new Promise((resolve) => server.close(resolve)), new Promise((resolve) => ipv6.close(resolve))]);
  rmSync(work, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
