// Disposable, D1-only Cloudflare transfer fixture. This never renders the
// production/develop config and never creates routes, Access apps, or queues.
// Build the production shim/assets first. Modes: provision, run, cleanup.
import assert from 'node:assert/strict';
import {appendFileSync,existsSync,mkdirSync,readFileSync,writeFileSync} from 'node:fs';
import {createHash,randomBytes} from 'node:crypto';
import {spawnSync} from 'node:child_process';
import {dirname,join,resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
const root=resolve(dirname(fileURLToPath(import.meta.url)),'..');
const account=process.env.CLOUDFLARE_ACCOUNT_ID??process.env.CLOUDFLARE_ACCOUT_ID;
const apiToken=process.env.CLOUDFLARE_API_TOKEN;
assert.match(account??'',/^[a-f0-9]{32}$/i,'Cloudflare account secret is required');
assert(apiToken,'CLOUDFLARE_API_TOKEN is required; this verification must not silently skip');
const accountHash=createHash('sha256').update(account).digest('hex');
const statePath=process.env.NAROU_TRANSFER_FIXTURE_STATE;
assert(statePath,'NAROU_TRANSFER_FIXTURE_STATE must name a private temporary state file');
const pin=process.env.NAROU_WRANGLER_VERSION??'4.151.3';
assert.match(pin,/^\d+\.\d+\.\d+$/,'Wrangler must be exactly pinned');
const mode=process.argv[2]??'run';
assert(['provision','run','cleanup'].includes(mode));
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
async function api(path,{method='GET',body,missing=false}={}){
  const response=await fetch(`https://api.cloudflare.com/client/v4/accounts/${account}${path}`,{method,headers:{authorization:`Bearer ${apiToken}`,'content-type':'application/json'},body:body===undefined?undefined:JSON.stringify(body),signal:AbortSignal.timeout(mode==='cleanup'?20000:90000)});
  if(missing&&response.status===404)return null;
  const text=await response.text();let result;
  try{result=text?JSON.parse(text):{success:response.ok};}catch{throw new Error(`Cloudflare ${method} ${path}: HTTP ${response.status}`);}
  assert(response.ok&&result.success!==false,`Cloudflare ${method} ${path}: HTTP ${response.status}; ${JSON.stringify(result.errors??[])}`);
  return result.result??null;
}
async function databaseNamed(name){
  const rows=await api(`/d1/database?name=${encodeURIComponent(name)}&per_page=100`);
  assert(Array.isArray(rows));const exact=rows.filter(row=>row.name===name);assert(exact.length<=1,'ambiguous fixture database identity');return exact[0]??null;
}
function names(state){
  assert.match(state.runId,/^(?:\d{1,20}|manual)$/);assert.match(state.attempt,/^\d{1,5}$/);assert.match(state.nonce,/^[a-f0-9]{16}$/);
  const worker=`narou-xfer-ci-${state.runId}-${state.attempt}-${state.nonce}`;
  assert(worker.length<=60);return{worker,database:`${worker}-db`};
}
function validate(state){
  assert.equal(state.version,1);assert.equal(state.accountHash,accountHash,'fixture belongs to another Cloudflare account');
  if(process.env.GITHUB_RUN_ID)assert.equal(state.runId,process.env.GITHUB_RUN_ID,'fixture belongs to another workflow run');
  const expected=names(state);assert.equal(state.workerName,expected.worker);assert.equal(state.databaseName,expected.database);
  assert(Number.isFinite(Date.parse(state.createdAt)),'missing fixture creation intent');return state;
}
function save(state){writeFileSync(statePath,JSON.stringify(state),{mode:0o600});}
function wrangler(args){
  const windows=process.platform==='win32';const result=spawnSync(windows?'npx.cmd':'npx',['--yes',`wrangler@${pin}`,...args],{cwd:root,stdio:'inherit',shell:windows,env:{...process.env,CLOUDFLARE_ACCOUNT_ID:account,CI:'true',WRANGLER_SEND_METRICS:'false'},timeout:15*60*1000});
  assert.equal(result.status,0,`pinned Wrangler command failed: ${args[0]}`);
}
async function provision(){
  assert(!existsSync(statePath),'refusing to replace an existing fixture state file');
  assert(existsSync(join(root,'build/worker/shim.mjs'))&&existsSync(join(root,'public/assets/transfer/manifest.json')),'build the complete production Worker/transfer assets first');
  const subdomain=await api('/workers/subdomain');assert.match(subdomain?.subdomain??'',/^[a-z0-9-]+$/,'the account must already have a workers.dev subdomain; the fixture will not change it');
  const state={version:1,accountHash,runId:process.env.GITHUB_RUN_ID??'manual',attempt:process.env.GITHUB_RUN_ATTEMPT??'1',nonce:randomBytes(8).toString('hex'),createdAt:new Date().toISOString()};
  const expected=names(state);state.workerName=expected.worker;state.databaseName=expected.database;
  assert.equal(await databaseNamed(state.databaseName),null,'refusing to reuse any existing database');
  assert.equal(await api(`/workers/scripts/${state.workerName}/settings`,{missing:true}),null,'refusing to replace any existing Worker');
  // Persist and publish BOTH exact-name creation intents before the first
  // network effect. Cleanup can recover even a lost create/deploy response.
  state.databaseIntent=true;state.workerIntent=true;mkdirSync(dirname(statePath),{recursive:true});save(state);
  if(process.env.GITHUB_OUTPUT)appendFileSync(process.env.GITHUB_OUTPUT,`cleanup=${Buffer.from(JSON.stringify(state)).toString('base64url')}\n`);
  const database=await api('/d1/database',{method:'POST',body:{name:state.databaseName}});
  assert.equal(database.name,state.databaseName);assert.match(database.uuid,/^[a-f0-9-]{36}$/i);state.databaseId=database.uuid;save(state);
  const directory=join(dirname(statePath),state.workerName);mkdirSync(directory,{recursive:true});
  state.adminToken=randomBytes(32).toString('base64url');state.url=`https://${state.workerName}.${subdomain.subdomain}.workers.dev`;
  const loginKey=randomBytes(32).toString('base64');const secrets=join(directory,'secrets.json');
  writeFileSync(secrets,JSON.stringify({NAROU_ADMIN_TOKEN:state.adminToken,NAROU_RS_LOGIN_KEY:loginKey}),{mode:0o600});
  if(process.env.GITHUB_ACTIONS){console.log(`::add-mask::${state.adminToken}`);console.log(`::add-mask::${loginKey}`);}
  const date=/^compatibility_date\s*=\s*"([\d-]+)"/m.exec(readFileSync(join(root,'wrangler.toml'),'utf8'))?.[1];assert(date);
  const config=join(directory,'wrangler.json');
  writeFileSync(config,JSON.stringify({name:state.workerName,account_id:account,main:join(root,'tests/transfer_failure_worker.mjs'),compatibility_date:date,workers_dev:true,preview_urls:false,limits:{cpu_ms:300000},vars:{NAROU_AUTH_REQUIRED:'true',NAROU_REQUIRE_S3:'false',NAROU_WORKER_ROLE:'writer'},assets:{directory:join(root,'public'),binding:'ASSETS',html_handling:'auto-trailing-slash',not_found_handling:'none'},d1_databases:[{binding:'DB',database_name:state.databaseName,database_id:state.databaseId,migrations_dir:join(root,'migrations')}],durable_objects:{bindings:[{name:'RATE_LIMITER',class_name:'SiteRateLimiter'},{name:'PUSH_HUB',class_name:'PushHub'}]},migrations:[{tag:'fixture-v1',new_sqlite_classes:['SiteRateLimiter','PushHub']}]}));
  save(state);wrangler(['d1','migrations','apply','DB','--remote','--config',config]);wrangler(['deploy','--config',config,'--secrets-file',secrets]);
  let ready=false;
  for(let attempt=0;attempt<90;attempt++){
    try{const response=await fetch(`${state.url}/api/transfer/v2/capabilities`,{headers:{authorization:`Bearer ${state.adminToken}`},signal:AbortSignal.timeout(15000)});const body=await response.text();if(response.status===200){assert(response.headers.get('cf-ray'));const caps=JSON.parse(body);assert.equal(caps.protocol_version,2);assert.equal(caps.storage.import.allowed,true,body);ready=true;break;}}catch(error){if(attempt===89)throw error;}
    await sleep(2000);
  }
  assert(ready,'deployed disposable fixture did not become ready');
  if(process.env.GITHUB_ENV)appendFileSync(process.env.GITHUB_ENV,`NAROU_TRANSFER_TEST_URL=${state.url}\nNAROU_TRANSFER_TEST_TOKEN=${state.adminToken}\n`);
  return state;
}
async function cleanup(){
  if(!existsSync(statePath))return;
  const state=validate(JSON.parse(readFileSync(statePath,'utf8')));const failures=[];
  const remove=async(action)=>{for(let attempt=0;attempt<5;attempt++){try{await action();return;}catch(error){if(attempt===4){failures.push(error);return;}await sleep(1000*2**attempt);}}};
  // The unique script owns its DO namespaces. Deleting it (force only for this
  // exact, preflight-reserved name) removes those namespaces as well.
  if(state.workerIntent)await remove(()=>api(`/workers/scripts/${state.workerName}?force=true`,{method:'DELETE',missing:true}));
  if(state.databaseIntent)await remove(async()=>{const database=await databaseNamed(state.databaseName);if(!database)return;if(state.databaseId)assert.equal(database.uuid,state.databaseId,'database identity changed');if(database.created_at)assert(Date.parse(database.created_at)>=Date.parse(state.createdAt)-300000,'refusing to delete a database older than the fixture intent');await api(`/d1/database/${database.uuid}`,{method:'DELETE',missing:true});});
  assert.equal(failures.length,0,failures.map(error=>error.message).join('\n'));
  state.cleaned=true;save(state);console.log('Disposable transfer Worker, private DO namespaces, and D1 database removed.');
}
if(mode==='cleanup'){
  if(process.env.NAROU_TRANSFER_FIXTURE_CLEANUP){assert(!existsSync(statePath),'refusing to replace cleanup state');const state=validate(JSON.parse(Buffer.from(process.env.NAROU_TRANSFER_FIXTURE_CLEANUP,'base64url').toString('utf8')));mkdirSync(dirname(statePath),{recursive:true});save(state);}
  await cleanup();
}else if(mode==='provision')await provision();
else{try{const state=await provision();const result=spawnSync(process.execPath,[join(root,'tests/transfer_memory_deployed.mjs')],{cwd:root,stdio:'inherit',env:{...process.env,NAROU_TRANSFER_TEST_URL:state.url,NAROU_TRANSFER_TEST_TOKEN:state.adminToken},timeout:25*60*1000});assert.equal(result.status,0,'deployed transfer regressions failed');}finally{await cleanup();}}
