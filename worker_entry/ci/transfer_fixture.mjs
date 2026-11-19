// Disposable, D1-only Cloudflare transfer fixture. This never renders the
// production/develop config and never creates routes, Access apps, or queues.
// Build the production shim/assets first. Modes: provision, exercise, run, cleanup.
import assert from 'node:assert/strict';
import {appendFileSync,existsSync,mkdirSync,readFileSync,rmSync,writeFileSync} from 'node:fs';
import {createHash,randomBytes} from 'node:crypto';
import {spawn,spawnSync} from 'node:child_process';
import {dirname,join,resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {MAX_TRACE_ATTEMPTS, PREFLIGHT_ATTEMPTS, TRACE_HEADER, TRACE_SCHEMA, missingTraceEvidence, verifyTraceReceipts} from './transfer_trace.mjs';
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
assert(['provision','exercise','run','cleanup'].includes(mode));
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
// Match the established deployment probe and downloader DEFAULT_USER_AGENT.
const probeUserAgent='Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:131.0) Gecko/20100101 Firefox/131.0';
async function api(path,{method='GET',body,missing=false,timeout=mode==='cleanup'?20000:90000}={}){
  const response=await fetch(`https://api.cloudflare.com/client/v4/accounts/${account}${path}`,{method,headers:{authorization:`Bearer ${apiToken}`,'content-type':'application/json'},body:body===undefined?undefined:JSON.stringify(body),signal:AbortSignal.timeout(timeout)});
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
  assert(worker.length<=58);return{worker,database:`${worker}-db`,tail:`${worker}-tail`};
}
function validate(state){
  assert.equal(state.version,2);assert.equal(state.accountHash,accountHash,'fixture belongs to another Cloudflare account');
  if(process.env.GITHUB_RUN_ID)assert.equal(state.runId,process.env.GITHUB_RUN_ID,'fixture belongs to another workflow run');
  const expected=names(state);assert.equal(state.workerName,expected.worker);assert.equal(state.databaseName,expected.database);
  assert.equal(state.tailName,expected.tail);
  assert(Number.isFinite(Date.parse(state.createdAt)),'missing fixture creation intent');return state;
}
function save(state){writeFileSync(statePath,JSON.stringify(state),{mode:0o600});}
function runWrangler(args,stdio='inherit'){
  const windows=process.platform==='win32';
  return spawnSync(windows?'npx.cmd':'npx',['--yes',`wrangler@${pin}`,...args],{
    cwd:root,stdio,shell:windows,encoding:'utf8',
    env:{...process.env,CLOUDFLARE_ACCOUNT_ID:account,CI:'true',WRANGLER_SEND_METRICS:'false'},timeout:15*60*1000,
  });
}
function wrangler(args){
  const result=runWrangler(args);
  assert.equal(result.status,0,`pinned Wrangler command failed: ${args[0]}`);
}
async function deployTail(config,name){
  const args=['deploy','--config',config];
  const result=runWrangler(args,['ignore','pipe','pipe']);
  process.stdout.write(result.stdout??'');
  process.stderr.write(result.stderr??'');
  if(result.status===0)return;
  const path=`/workers/workers/${name}`;
  // The upload can succeed before Wrangler's trigger sync can read the new
  // script through the Workers metadata API. Retry only that observed race.
  assert(result.status===1&&result.stdout?.includes(`Uploaded ${name} `)&&
    result.stderr?.includes(`${path})`)&&result.stderr.includes('[code: 10007]'),
    'pinned Wrangler Tail Worker deploy failed');
  for(let attempt=0;attempt<15;attempt++){
    if(await api(path,{missing:true,timeout:5000})){
      wrangler(args);
      return;
    }
    await sleep(2000);
  }
  assert.fail('uploaded Tail Worker did not become visible to the Cloudflare metadata API');
}
async function provision(){
  assert(!existsSync(statePath),'refusing to replace an existing fixture state file');
  assert(existsSync(join(root,'build/worker/shim.mjs'))&&existsSync(join(root,'public/assets/transfer/manifest.json')),'build the complete production Worker/transfer assets first');
  const subdomain=await api('/workers/subdomain');assert.match(subdomain?.subdomain??'',/^[a-z0-9-]+$/,'the account must already have a workers.dev subdomain; the fixture will not change it');
  const state={version:2,accountHash,runId:process.env.GITHUB_RUN_ID??'manual',attempt:process.env.GITHUB_RUN_ATTEMPT??'1',nonce:randomBytes(8).toString('hex'),createdAt:new Date().toISOString()};
  const expected=names(state);state.workerName=expected.worker;state.databaseName=expected.database;state.tailName=expected.tail;
  assert.equal(await databaseNamed(state.databaseName),null,'refusing to reuse any existing database');
  for(const name of [state.workerName,state.tailName]){
    assert.equal(await api(`/workers/scripts/${name}/settings`,{missing:true}),null,'refusing to replace any existing Worker');
  }
  // Publish all exact-name creation intents before the first network effect.
  // Cleanup can recover even if a create or deployment response is lost.
  state.databaseIntent=true;state.workerIntent=true;mkdirSync(dirname(statePath),{recursive:true});save(state);
  if(process.env.GITHUB_OUTPUT)appendFileSync(process.env.GITHUB_OUTPUT,`cleanup=${Buffer.from(JSON.stringify(state)).toString('base64url')}\n`);
  const database=await api('/d1/database',{method:'POST',body:{name:state.databaseName}});
  assert.equal(database.name,state.databaseName);assert.match(database.uuid,/^[a-f0-9-]{36}$/i);state.databaseId=database.uuid;save(state);
  const directory=join(dirname(statePath),state.workerName);mkdirSync(directory,{recursive:true});
  state.adminToken=randomBytes(32).toString('base64url');state.url=`https://${state.workerName}.${subdomain.subdomain}.workers.dev`;
  state.traceRun=randomBytes(8).toString('hex');
  const loginKey=randomBytes(32).toString('base64');const secrets=join(directory,'secrets.json');
  writeFileSync(secrets,JSON.stringify({NAROU_ADMIN_TOKEN:state.adminToken,NAROU_RS_LOGIN_KEY:loginKey}),{mode:0o600});
  if(process.env.GITHUB_ACTIONS){console.log(`::add-mask::${state.adminToken}`);console.log(`::add-mask::${loginKey}`);}
  const date=/^compatibility_date\s*=\s*"([\d-]+)"/m.exec(readFileSync(join(root,'wrangler.toml'),'utf8'))?.[1];assert(date);
  const config=join(directory,'wrangler.json');
  writeFileSync(config,JSON.stringify({name:state.workerName,account_id:account,main:join(root,'tests/transfer_failure_worker.mjs'),compatibility_date:date,compatibility_flags:['enable_request_signal'],workers_dev:true,preview_urls:false,limits:{cpu_ms:300000},vars:{NAROU_AUTH_REQUIRED:'true',NAROU_REQUIRE_S3:'false',NAROU_WORKER_ROLE:'writer'},assets:{directory:join(root,'public'),binding:'ASSETS',html_handling:'auto-trailing-slash',not_found_handling:'none'},d1_databases:[{binding:'DB',database_name:state.databaseName,database_id:state.databaseId,migrations_dir:join(root,'migrations')}],durable_objects:{bindings:[{name:'RATE_LIMITER',class_name:'SiteRateLimiter'},{name:'PUSH_HUB',class_name:'PushHub'}]},migrations:[{tag:'fixture-v1',new_sqlite_classes:['SiteRateLimiter','PushHub']}],tail_consumers:[{service:state.tailName}]}));
  save(state);wrangler(['d1','migrations','apply','DB','--remote','--config',config]);
  // A new library defaults to S3. This isolated fixture deliberately has no S3 binding.
  const storage=join(directory,'storage.sql');
  writeFileSync(storage,`INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES('inv','content_backend','"d1"','d1'),('inv','asset_backend','"d1"','d1');\n${TRACE_SCHEMA}`);
  wrangler(['d1','execute','DB','--remote','--file',storage,'--config',config]);
  const traceConfig=join(directory,'tail.json');
  writeFileSync(traceConfig,JSON.stringify({
    name:state.tailName,account_id:account,main:join(root,'ci/transfer_trace_worker.mjs'),
    compatibility_date:date,workers_dev:false,preview_urls:false,
    vars:{NAROU_TRACE_RUN:state.traceRun},
    d1_databases:[{binding:'TRACE_DB',database_name:state.databaseName,database_id:state.databaseId}],
  }),{mode:0o600});
  await deployTail(traceConfig,state.tailName);
  wrangler(['deploy','--config',config,'--secrets-file',secrets]);
  let readySequence,lastResponse='no HTTP response';
  for(let attempt=0;attempt<PREFLIGHT_ATTEMPTS;attempt++){
    // Use a separate sequence range from the contract driver. This preflight
    // detects a misconfigured or unavailable Tail Worker before a long case.
    const seq=MAX_TRACE_ATTEMPTS-attempt;
    try{
      const response=await fetch(`${state.url}/api/transfer/v2/capabilities`,{
        headers:{authorization:`Bearer ${state.adminToken}`,'user-agent':probeUserAgent,[TRACE_HEADER]:`${state.traceRun}:${seq}`},
        signal:AbortSignal.timeout(15000),
      });
      lastResponse=`HTTP ${response.status}; content-type=${response.headers.get('content-type')??'missing'}; cf-mitigated=${response.headers.has('cf-mitigated')}`;
      const body=await response.text();
      if(response.status===200){
        assert(response.headers.get('cf-ray'));
        assert.equal(response.headers.get('x-test-transfer-primary-only'),'true','fixture capability must run on primary');
        const caps=JSON.parse(body);
        assert.equal(caps.protocol_version,2);
        assert.equal(caps.storage.import.allowed,true,body);
        readySequence=seq;
        break;
      }
    }catch(error){if(attempt===PREFLIGHT_ATTEMPTS-1)throw error;}
    await sleep(2000);
  }
  assert(readySequence,`deployed disposable fixture did not become ready (${lastResponse})`);
  await waitForEvidence(state,readySequence,[200]);
  console.log(JSON.stringify({event:'transfer_trace_ready'}));
  if(process.env.GITHUB_ENV)appendFileSync(process.env.GITHUB_ENV,`NAROU_TRANSFER_TEST_URL=${state.url}\nNAROU_TRANSFER_TEST_TOKEN=${state.adminToken}\n`);
  return state;
}
async function readEvidence(state,startSequence,highestSequence) {
  const rows=[];
  for(let first=startSequence;first<=highestSequence;first+=500) {
    const last=Math.min(first+499,highestSequence);
    const result=await api(`/d1/database/${state.databaseId}/query`,{
      method:'POST',timeout:10000,
      body:{sql:'SELECT seq,outcome,http_status FROM fixture_transfer_traces WHERE seq BETWEEN ? AND ? ORDER BY seq,outcome,http_status',params:[String(first),String(last)]},
    });
    assert.equal(result?.length,1,'invalid D1 trace query result');
    assert.equal(result[0].success,true,'D1 trace lookup failed');
    assert(Array.isArray(result[0].results),'invalid D1 trace rows');
    rows.push(...result[0].results);
  }
  return rows;
}

async function waitForEvidence(state,firstSequence,expected) {
  const deadline=Date.now()+90000;
  const lastSequence=firstSequence+expected.length-1;
  let details;
  do {
    const rows=await readEvidence(state,firstSequence,lastSequence);
    // verifyTraceReceipts throws on contradictory or malformed receipts; an
    // edge-answered attempt can legitimately never produce one, so only the
    // dispatched gap count drives the wait.
    verifyTraceReceipts(expected,firstSequence===1?rows:rows.map(row=>({...row,seq:row.seq-firstSequence+1})));
    details=missingTraceEvidence(expected,rows,firstSequence);
    if(details.unaccounted===0)break;
    await sleep(2000);
  } while(Date.now()<deadline);
  assert.equal(details.unaccounted,0,`missing Cloudflare Tail Worker evidence for ${details.unaccounted} dispatched request(s): ${JSON.stringify(details)}`);
  console.log(JSON.stringify({event:'transfer_trace_gaps',...details}));
}

async function exercise(state){
  validate(state);
  assert.match(state.traceRun??'',/^[a-f0-9]{16}$/);
  assert.match(state.databaseId??'',/^[a-f0-9-]{36}$/i);
  const ledger=resolve(dirname(statePath),state.workerName,'requests.json');
  assert(!existsSync(ledger),'refusing to reuse a transfer request ledger');
  // The metadata and 33-source cases both exceeded 25 minutes while still
  // advancing under real D1 latency. The scenario assertions stay unchanged.
  // The in-process limit stays below the 85-minute job envelope so cleanup
  // still runs on a slow day.
  const caseGroup=process.env.NAROU_TRANSFER_CASE_GROUP??'all';
  const minutes=75;
  try{
    const signal=AbortSignal.timeout(minutes*60*1000);
    const child=spawn(process.execPath,[join(root,'tests/transfer_memory_deployed.mjs')],{
      cwd:root,stdio:'inherit',signal,env:{...process.env,
        NAROU_TRANSFER_TEST_URL:state.url,NAROU_TRANSFER_TEST_TOKEN:state.adminToken,
        NAROU_TRACE_RUN:state.traceRun,NAROU_TRACE_RECEIPTS:ledger},
    });
    let processFailure;
    const result=await new Promise(resolve=>{
      child.once('error',error=>{processFailure=error.code;});
      child.once('close',(code,signal)=>resolve({code,signal}));
    });
    assert(!signal.aborted,`deployed transfer ${caseGroup} exceeded its ${minutes}-minute limit`);
    assert(!processFailure&&result.code===0,`deployed transfer regressions failed (${JSON.stringify(result)})`);
    const expected=JSON.parse(readFileSync(ledger,'utf8'));
    assert(Array.isArray(expected)&&expected.length>0&&expected.length<=MAX_TRACE_ATTEMPTS-PREFLIGHT_ATTEMPTS,'invalid transfer request ledger');
    // The tail runs after the producer, including after response streaming.
    // Poll durable receipts; a missing attempt can never be called a pass.
    await waitForEvidence(state,1,expected);
    console.log(JSON.stringify({event:'transfer_trace_verified',case_group:caseGroup,attempts:expected.length}));
  }finally{
    rmSync(ledger,{force:true});
  }
}
async function cleanup(){
  if(!existsSync(statePath))return;
  const state=validate(JSON.parse(readFileSync(statePath,'utf8')));const failures=[];
  const remove=async(action)=>{for(let attempt=0;attempt<5;attempt++){try{await action();return;}catch(error){if(attempt===4){failures.push(error);return;}await sleep(1000*2**attempt);}}};
  // The unique script owns its DO namespaces. Deleting it (force only for this
  // exact, preflight-reserved name) removes those namespaces as well.
  if(state.workerIntent)await remove(()=>api(`/workers/scripts/${state.workerName}?force=true`,{method:'DELETE',missing:true}));
  if(state.workerIntent)await remove(()=>api(`/workers/scripts/${state.tailName}`,{method:'DELETE',missing:true}));
  if(state.databaseIntent)await remove(async()=>{const database=await databaseNamed(state.databaseName);if(!database)return;if(state.databaseId)assert.equal(database.uuid,state.databaseId,'database identity changed');if(database.created_at)assert(Date.parse(database.created_at)>=Date.parse(state.createdAt)-300000,'refusing to delete a database older than the fixture intent');await api(`/d1/database/${database.uuid}`,{method:'DELETE',missing:true});});
  assert.equal(failures.length,0,failures.map(error=>error.message).join('\n'));
  state.cleaned=true;save(state);console.log('Disposable transfer Worker, Tail Worker, private DO namespaces, and D1 database removed.');
}
if(mode==='cleanup'){
  if(process.env.NAROU_TRANSFER_FIXTURE_CLEANUP){assert(!existsSync(statePath),'refusing to replace cleanup state');const state=validate(JSON.parse(Buffer.from(process.env.NAROU_TRANSFER_FIXTURE_CLEANUP,'base64url').toString('utf8')));mkdirSync(dirname(statePath),{recursive:true});save(state);}
  await cleanup();
}else if(mode==='provision')await provision();
else if(mode==='exercise')await exercise(validate(JSON.parse(readFileSync(statePath,'utf8'))));
else{try{await exercise(await provision());}finally{await cleanup();}}
