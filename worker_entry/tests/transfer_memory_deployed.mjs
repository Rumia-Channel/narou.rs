// A real Cloudflare Workers isolate enforces 128 MiB. Local OSS workerd
// does not enforce this limit; transfer.mjs separately measures its highwater.
// Deploy transfer_failure_worker.mjs with the production shim/assets to an
// isolated, disposable test D1 library before invoking this driver.
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {writeFileSync} from 'node:fs';
import {isAbsolute} from 'node:path';
import {brotliCompressSync,crc32} from 'node:zlib';
import {DISPATCH_HEADER,MAX_TRACE_ATTEMPTS,PREFLIGHT_ATTEMPTS,TRACE_HEADER} from '../ci/transfer_trace.mjs';
import {fixtureNovel,selectionDigest,sourceNamespaceContracts,transferClient,waitForDeployedTransfer} from './transfer.mjs';

const base=new URL(process.env.NAROU_TRANSFER_TEST_URL??'https://missing.invalid');
assert.equal(base.protocol,'https:','NAROU_TRANSFER_TEST_URL must be a deployed HTTPS Worker');
assert(base.hostname.endsWith('.workers.dev'),'this test requires real Cloudflare Workers, not local workerd');
const token=process.env.NAROU_TRANSFER_TEST_TOKEN;
assert(token,'NAROU_TRANSFER_TEST_TOKEN is required for the disposable test fixture');
const traceRun=process.env.NAROU_TRACE_RUN;
assert.match(traceRun??'',/^[0-9a-f]{16}$/,'NAROU_TRACE_RUN must be a fixture nonce');
const receiptsPath=process.env.NAROU_TRACE_RECEIPTS;
assert(receiptsPath&&isAbsolute(receiptsPath),'NAROU_TRACE_RECEIPTS must be an absolute private path');
const ledger=[];
async function tracedFetch(url,init={}){
  const seq=ledger.length+1;
  assert(seq<=MAX_TRACE_ATTEMPTS-PREFLIGHT_ATTEMPTS,'transfer trace attempt limit exceeded');
  const headers=new Headers(init.headers);
  headers.set(TRACE_HEADER,`${traceRun}:${seq}`);
  ledger.push({status:0,dispatched:false});
  const response=await fetch(url,{...init,headers,redirect:'manual'});
  ledger[seq-1]={status:response.status,dispatched:response.headers.has(DISPATCH_HEADER)};
  return response;
}
const caseGroups={
  metadata:['bounded_metadata_fragments','50000_entry_aliased_index_ack_reuse','16MiB_legacy_initializer_ack_reuse'],
  publication:['17MiB_bounded_wire','runtime_complete','overwrite_skip','create_control_replay','partial_batch_sha_receipts','raw_forbidden','canonical_commit_ack_loss','cancel_before_commit','virtual_members_frozen_order'],
  gates:['33_source_gate_and_private_cursor_replay','legacy_source_namespace_isolation'],
};
const caseGroup=process.env.NAROU_TRANSFER_CASE_GROUP??'all';
assert(caseGroup==='all'||Object.hasOwn(caseGroups,caseGroup),'unknown transfer case group');
const groups=caseGroup==='all'?Object.keys(caseGroups):[caseGroup];
const client=transferClient(base.origin,token,{verifyPrimary:true,fetcher:tracedFetch});
// Keep diagnostics bounded and omit credentials, payload text and raw backend errors.
const request=client.request;
let requestNumber=0,lastFailure;
client.request=async(path,init={})=>{
  const ordinal=++requestNumber,periodic=ordinal<=20||ordinal%100===0;
  if(periodic)console.log(JSON.stringify({event:'transfer_request',ordinal,path,method:init.method??'GET'}));
  let response;
  try{response=await request(path,init);}
  catch(error){const code=['ECONNRESET','ETIMEDOUT','UND_ERR_CONNECT_TIMEOUT','UND_ERR_HEADERS_TIMEOUT','UND_ERR_SOCKET'].includes(error?.cause?.code)?error.cause.code:'transport_failure';console.log(JSON.stringify({event:'transfer_transport_error',ordinal,path,code}));throw error;}
  if(periodic||response.status>=400){
    const value=response.headers.get('content-type')?.includes('application/json')?await response.clone().json():null;
    const category=String(value?.error?.message??'').match(/D1_[A-Z_]+|SQLITE_[A-Z_]+|too many SQL variables|out of memory|memory limit|too large|exceeded maximum/gi)?.join(',')??null;
    const failure=`${path}:${response.status}:${value?.error?.code}:${category}`;
    if(periodic||response.status>=400&&failure!==lastFailure)console.log(JSON.stringify({event:'transfer_response',ordinal,path,status:response.status,state:value?.state,revision:value?.revision,next_action:value?.next_action,error_code:value?.error?.code,retryable:value?.error?.retryable,error_category:category}));
    if(response.status>=400)lastFailure=failure;
  }
  return response;
};
await waitForDeployedTransfer(client);
// Readiness tolerates edge routing convergence. Exercise the actual primary
// capability route again, now requiring a receipt like every other case.
await client.caps();
const slug=`memory-${randomUUID()}`;
const observe=async session=>(await client.json(`/_fixture/transfer-observe?session=${session.status.session_id}`)).body;
async function select(session,profile='runtime',roles=[]){const catalog=await session.pages('catalog');await session.op('select',{profile,extra_roles:roles,selection_digest:selectionDigest(session.status.catalog_revision,profile,roles,catalog)});}
async function faultStep(session,mode){
  const init={method:'POST',headers:session.headers({'content-type':'application/json',...(mode?{'x-narou-test-transfer-fault':mode}:{})}),body:JSON.stringify({operation_id:session.status.operation_id})};
  const response=await client.request(`${session.path}/advance`,init);const text=await response.text();const fired=response.headers.get('x-test-transfer-fault-fired')==='true';
  if(fired)assert([423,500,503].includes(response.status),text);else{assert.equal(response.status,200,text);session.status=JSON.parse(text);}return{init,fired};
}
async function replayStep(session,attempt){const replay=await client.json(`${session.path}/advance`,{...attempt.init,headers:session.headers({'content-type':'application/json'})});assert.equal(replay.response.status,200,JSON.stringify(replay.body));session.status=replay.body;}
const unauth=await tracedFetch(`${base.origin}/api/transfer/v2/capabilities`);
// Drain the small body before asserting. A discarded body can cancel the
// request and change the outcome the tail records for this attempt.
await unauth.arrayBuffer();
assert.equal(unauth.status,401);
const retiredExport=await client.request('/api/transfer/export?ids=1');
await retiredExport.arrayBuffer();
assert.equal(retiredExport.status,404);
const retiredImport=await client.request('/api/transfer/import',{method:'POST',body:'retired'});
await retiredImport.arrayBuffer();
assert.equal(retiredImport.status,404);
if(groups.includes('metadata')){
const story='\x01'.repeat(3*1024*1024);
const novel=fixtureNovel(slug,[],{subtitle:story});
novel.files[0].bytes=Buffer.concat([novel.files[0].bytes,Buffer.from(`story: "${'\\x01'.repeat(story.length)}"\n`)]);
assert(novel.files[0].bytes.length<16*1024*1024);
const imported=await client.importData([novel]);
assert.equal(imported.status.state,'completed');
const records=(await client.json('/api/novels?limit=1000')).body.data;
const id=records.find(row=>row.toc_url===novel.toc_url)?.id;
assert(id);
const storySession=await client.beginExport([id]);await storySession.until(value=>value.state==='selecting');
const frozen=await observe(storySession);assert(frozen.frozen.fragments>90);assert(frozen.frozen.max_row_bytes<=32768);assert.equal(frozen.stages.length,0,'metadata projection must not snapshot payload files before selection');
await select(storySession);await storySession.until(value=>['ready','reading'].includes(value.state));const exported=await storySession.download();
assert.equal(exported.manifest.novels[0].subtitle,story);
assert.deepEqual(exported.files.get('novels/0/toc.yaml'),novel.files[0].bytes);

let index='version: 2\nsources:\n';
for(let item=0;item<50000;item++)index+=`  'https://example.test/index/${item}/${'x'.repeat(160)}': ${item===0?'&shared shared.png':'*shared'}\n`;
assert(Buffer.byteLength(index)<16*1024*1024);
const indexed=fixtureNovel(`${slug}-index`,[{path:'.illustration_cache.yaml',bytes:Buffer.from(index)},{path:'挿絵/shared.png',bytes:Buffer.from([7])}]);
assert.equal((await client.importData([indexed])).status.state,'completed');
const indexId=(await client.json('/api/novels?limit=1000')).body.data.find(row=>row.toc_url===indexed.toc_url)?.id;
assert(indexId);
const selected=await client.beginExport([indexId]);
let initialized=false,lostIndexStage;
for(let step=0;step<80;step++){
  const attempt=await faultStep(selected,lostIndexStage?null:'initializer_after');const observed=await observe(selected);
  if(attempt.fired){const stage=observed.stages.find(stage=>stage.stagepurpose==='illustration_index');assert(stage?.initialized);lostIndexStage=stage.stage_id;assert.equal(stage.sealed,0);continue;}
  if(observed.stages.some(stage=>stage.stagepurpose==='illustration_index'&&stage.sealed)){
    assert(lostIndexStage,'the real initializer EOF checkpoint acknowledgement must be interrupted');
    assert(observed.stages.every(stage=>stage.stagepurpose==='illustration_index'),'metadata initialization cannot fetch image/body payloads');
    assert.equal(observed.stages.length,1,'response loss must reuse, not reparse, the finished metadata stage');assert.equal(observed.stages[0].stage_id,lostIndexStage);
    initialized=true;break;
  }
}
assert(initialized,'the actual 50000-entry metadata initializer must finish');await selected.cancel();

// The old compressed representation really traverses the streaming decoder,
// then its retained raw stage. Importing a v2 reference would not test this.
const legacyNovel=fixtureNovel(`${slug}-legacy`,[{path:'legacy.bin',bytes:Buffer.from([1])}]);await client.importData([legacyNovel]);
const legacyId=(await client.json('/api/novels?limit=1000')).body.data.find(row=>row.toc_url===legacyNovel.toc_url).id;
const legacyBytes=Buffer.alloc(16*1024*1024,7);const compressed=brotliCompressSync(legacyBytes);assert(compressed.length<=64*1024);
const seeded=await client.json('/_fixture/transfer-legacy',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({id:legacyId,path:'legacy.bin',size:legacyBytes.length,crc32:crc32(legacyBytes),brotli:compressed.toString('base64')})});assert.equal(seeded.response.status,200,JSON.stringify(seeded.body));
const legacy=await client.beginExport([legacyId]);await legacy.until(value=>value.state==='selecting');assert.equal((await observe(legacy)).stages.length,0);await select(legacy);
let lostLegacyStage;
for(let step=0;step<1000;step++){
  await legacy.get();if(['ready','reading'].includes(legacy.status.state))break;
  const attempt=await faultStep(legacy,lostLegacyStage?null:'initializer_after');
  if(attempt.fired){const stage=(await observe(legacy)).stages.find(stage=>stage.stagepurpose==='legacy_source');assert(stage?.initialized);assert.equal(stage.expected_size,16*1024*1024);lostLegacyStage=stage.stage_id;}
}
assert(lostLegacyStage,'the actual 16MiB decoded initializer must persist its EOF checkpoint');assert(['ready','reading'].includes(legacy.status.state));
const legacyStages=(await observe(legacy)).stages.filter(stage=>stage.stagepurpose==='legacy_source');assert.equal(legacyStages.length,1);assert.equal(legacyStages[0].stage_id,lostLegacyStage);assert(legacyStages[0].sealed);
assert.deepEqual((await legacy.download()).files.get('novels/0/legacy.bin'),legacyBytes);
console.log(JSON.stringify({event:'transfer_case_group_completed',case_group:'metadata',requests:client.requests.length,wasm_highwater_bytes:client.memoryHighWater}));
}

if(groups.includes('publication')){
const large=Buffer.alloc(17*1024*1024);for(let index=0;index<large.length;index++)large[index]=(index*31+(index>>>7))&255;
const original=fixtureNovel(`${slug}-bounded`,[{path:'notes.bin',bytes:large},{path:'replace.txt',bytes:Buffer.from('user settings')},{path:'本文/cache/old.yaml',bytes:Buffer.from('body: historic\n')}]);
assert.equal((await client.importData([original],{profile:'complete'})).status.state,'completed');
const originalId=(await client.json('/api/novels?limit=1000')).body.data.find(row=>row.toc_url===original.toc_url).id;
const runtime=await client.exportData([originalId]);assert.deepEqual(runtime.files.get('novels/0/notes.bin'),large);assert(!runtime.files.has('novels/0/本文/cache/old.yaml'));
const complete=await client.exportData([originalId],{profile:'complete'});assert(complete.files.has('novels/0/本文/cache/old.yaml'));
const replacement=fixtureNovel(`${slug}-bounded`,[{path:'notes.bin',bytes:Buffer.from('replacement')}]);await client.importData([replacement],{policy:'overwrite'});
const preserved=await client.exportData([originalId],{profile:'complete'});assert.equal(preserved.files.get('novels/0/notes.bin').toString(),'replacement');assert.equal(preserved.files.get('novels/0/replace.txt').toString(),'user settings');assert(preserved.files.has('novels/0/本文/cache/old.yaml'));
assert.equal((await client.importData([replacement])).plan[0].action,'skip');

const replay=await client.createImport([fixtureNovel(`${slug}-replay`)]);assert.equal((await client.json('/api/transfer/v2/imports',replay.create)).body.session_id,replay.status.session_id);await replay.manifest();await replay.until(value=>value.state==='transferring');await replay.upload();const ack=await replay.op('ready');const ackAgain=await client.json(`${replay.path}/ready`,ack.init);assert.equal(ackAgain.response.status,200);assert.equal(ackAgain.body.revision,ack.body.revision);await replay.op('publish',{plan_revision:replay.status.plan_revision});await replay.until(value=>value.state==='completed');await replay.drain();
const partial=await client.createImport([fixtureNovel(`${slug}-partial`,[{path:'a.bin',bytes:Buffer.from('first')},{path:'b.bin',bytes:Buffer.from('second')}])]);await partial.manifest();await partial.until(value=>value.state==='transferring');
const needed=(await partial.pages('files')).filter(file=>file.upload_required);const recordsToSend=needed.slice(0,2).map(file=>({file_id:file.file_id,chunk_index:0,bytes:partial.wire.files[file.file_id].bytes,final:true}));recordsToSend[1].sha256='0'.repeat(64);
const partialResult=await partial.batch(recordsToSend);assert.equal(partialResult.receipts.length,1);assert(partialResult.failure);const confirmed=(await partial.get()).confirmed_bytes;assert.equal((await partial.batch([recordsToSend[0]])).receipts[0].outcome,'duplicate');assert.equal((await partial.get()).confirmed_bytes,confirmed);await partial.upload();await partial.op('ready');await partial.op('publish',{plan_revision:partial.status.plan_revision});await partial.until(value=>value.state==='completed');await partial.drain();
const raw=await client.createImport([fixtureNovel(`${slug}-raw`,[{path:'raw/page.html',bytes:Buffer.from('<html/>')}])],{profile:'complete'});await raw.manifest();await raw.until(value=>value.state==='failed');assert.match(raw.status.error,/file_policy/);await raw.drain();

const uncertain=transferClient(base.origin,token,{verifyPrimary:true,fetcher:tracedFetch});uncertain.extraHeaders={'x-narou-test-transfer-fault':'commit_after'};const uncertainNovel=fixtureNovel(`${slug}-commit-ack`);assert.equal((await uncertain.importData([uncertainNovel])).status.state,'completed');assert(uncertain.faults>0);assert.equal((await client.json('/api/novels?limit=1000')).body.data.filter(row=>row.toc_url===uncertainNovel.toc_url).length,1);
const blocked=await client.createImport([fixtureNovel(`${slug}-bounded`,[{path:'notes.bin',bytes:Buffer.from('must not publish')}])],{policy:'overwrite'});await blocked.manifest();await blocked.until(value=>value.state==='transferring');await blocked.upload();await blocked.op('ready');await blocked.op('publish',{plan_revision:blocked.status.plan_revision});
for(let step=0;step<100;step++){if((await observe(blocked)).publications[0]?.state==='committing')break;await blocked.step();}
await blocked.get();assert((await faultStep(blocked,'commit_before')).fired);assert.equal((await observe(blocked)).targets[0].committed,0);await blocked.cancel();assert.equal((await client.exportData([originalId])).files.get('novels/0/notes.bin').toString(),'replacement');

const memberOne=fixtureNovel(`${slug}-member-one`,[],{attribute_tag:'漫画'}),memberTwo=fixtureNovel(`${slug}-member-two`);
const parent={toc_url:`https://narou.rs/virtual/${randomUUID().replaceAll('-','')}`,title:`Group ${slug}`,author:'Fixture',sitename:'Transfer',novel_type:1,tags:['group'],attribute_tag:'小説',virtual_members:[memberTwo.toc_url,memberOne.toc_url],files:[{path:'setting.ini',bytes:Buffer.from('enable_yokogaki=true\n')}]};
assert.equal((await client.importData([memberOne,memberTwo,parent],{profile:'complete'})).status.state,'completed');
const groupRecords=(await client.json('/api/novels?limit=1000')).body.data;const parentId=groupRecords.find(row=>row.toc_url===parent.toc_url).id;const memberId=groupRecords.find(row=>row.toc_url===memberOne.toc_url).id;
const group=await client.exportData([parentId,memberId],{profile:'complete'});assert.equal(group.manifest.novels.length,3);assert.deepEqual(group.manifest.novels[2].virtual_members,parent.virtual_members);assert(!group.manifest.novels[2].tags.some(tag=>tag.startsWith('vs_')));
assert((await client.json('/api/virtual_novels')).body.novels.some(group=>group.title===parent.title&&group.members.length===2));
client.memoryHighWater=Math.max(client.memoryHighWater??0,uncertain.memoryHighWater??0);
console.log(JSON.stringify({event:'transfer_case_group_completed',case_group:'publication',requests:client.requests.length,wasm_highwater_bytes:client.memoryHighWater}));
}

if(groups.includes('gates')){
const gateNovels=Array.from({length:33},(_,index)=>fixtureNovel(`${slug}-gate-${index}`));await client.importData(gateNovels);const listed=(await client.json('/api/novels?limit=1000')).body.data;const gateIds=gateNovels.map(novel=>listed.find(row=>row.toc_url===novel.toc_url).id);const gates=await client.beginExport(gateIds);
let selectionFault;for(let step=0;step<1000;step++){const attempt=await faultStep(gates,'backend_after');if(attempt.fired){selectionFault=attempt;break;}}assert(selectionFault,'actual selected-source cursor commit must be interrupted');const beforeSelectionReplay=(await observe(gates)).selection;await replayStep(gates,selectionFault);assert.deepEqual((await observe(gates)).selection,beforeSelectionReplay,'an old operation must not freeze the next source page');
await gates.until(value=>value.state==='selecting');await select(gates);
let gateFault;for(let step=0;step<1000;step++){const attempt=await faultStep(gates,'backend_after');if(attempt.fired){gateFault=attempt;break;}}assert(gateFault);
const beforeGateReplay=(await observe(gates)).snapshot;assert.equal(beforeGateReplay.gates,16);await replayStep(gates,gateFault);assert.equal((await observe(gates)).snapshot.gates,16,'an old operation must not close the next gate page');
let partialSnapshot=false;for(let step=0;step<10000;step++){if(['ready','reading'].includes(gates.status.state))break;const snapshot=(await observe(gates)).snapshot;if(snapshot.pinned>0&&snapshot.pinned<33){assert.equal(snapshot.gates,33,'every source gate remains closed until every selected file is pinned');partialSnapshot=true;}await gates.step();}
assert(partialSnapshot);assert.equal((await observe(gates)).snapshot.gates,0);assert.equal((await gates.download()).manifest.novels.length,33);
await sourceNamespaceContracts(client,`${slug}-source`);
console.log(JSON.stringify({event:'transfer_case_group_completed',case_group:'gates',requests:client.requests.length,wasm_highwater_bytes:client.memoryHighWater}));
}
// Each v2 response reports the current invocation's WASM allocation; this is
// not a later request to a possibly different isolate's diagnostic endpoint.
assert(client.memoryHighWater>0,'the deployed production WASM must be instrumented');
assert(client.memoryHighWater<112*1024*1024,`WASM highwater ${client.memoryHighWater} leaves insufficient room in the enforced 128 MiB isolate`);
assert(client.requests.filter(request=>request.type==='application/octet-stream').every(request=>request.bytes<=8*1024*1024));
assert(client.requests.every(request=>request.type!=='application/zip'));
// The parent reads this file only after the driver exits successfully.
writeFileSync(receiptsPath,JSON.stringify(ledger),{flag:'wx',mode:0o600});
console.log(JSON.stringify({passed:true,case_group:caseGroup,platform:'Cloudflare Workers',enforced_isolate_bytes:128*1024*1024,wasm_highwater_bytes:client.memoryHighWater,trace_attempts:ledger.length,scenarios:groups.flatMap(group=>caseGroups[group])}));
