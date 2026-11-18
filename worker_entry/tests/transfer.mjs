// Real workerd/D1 transfer v2 contracts. Build artifacts first; this file
// never invokes a compiler. Helpers are also used by storage/tag contracts.
import assert from 'node:assert/strict';
import {spawn,spawnSync} from 'node:child_process';
import {existsSync,mkdirSync,mkdtempSync,readFileSync,writeFileSync,rmSync} from 'node:fs';
import {createHash,randomBytes,randomUUID} from 'node:crypto';
import {crc32} from 'node:zlib';
import {once} from 'node:events';
import {dirname,join} from 'node:path';
import {fileURLToPath,pathToFileURL} from 'node:url';
const root=join(dirname(fileURLToPath(import.meta.url)),'..');
const CHUNK=8*1024*1024-64*1024;
const WIRE=8*1024*1024;
export const sha256=bytes=>createHash('sha256').update(bytes).digest('hex');
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
// A discarded body can cancel the invocation we are trying to observe.
async function drainResponse(response){await response.body?.pipeTo(new WritableStream()).catch(()=>{});}
function assertFileBytes(actual,expected,path){
  assert(actual!==undefined,`${path}: missing file`);
  assert.equal(actual.length,expected.length,`${path}: byte length`);
  assert.equal(Buffer.compare(actual,expected),0,`${path}: content differs`);
}
export function encodeBatch(records){
  const frames=records.map(({file_id,chunk_index,bytes,final,sha256:digest})=>{
    const header=Buffer.from(JSON.stringify({file_id,chunk_index,payload_size:String(bytes.length),final,sha256:digest??sha256(bytes)}));
    assert(header.length<=512);const length=Buffer.alloc(4);length.writeUInt32BE(header.length);return Buffer.concat([length,header,bytes]);
  });const bytes=Buffer.concat(frames);assert(records.length<=100&&bytes.length<=WIRE);return bytes;
}
function wireManifest(novels,policyVersion,profile,roles){
  let ordinal=0,texts=0;const records=[];const files=[];
  for(const [index,novel]of novels.entries()){
    records.push({type:'novel',index,kind:novel.virtual_members?'virtual_parent':'normal',novel_type:novel.novel_type??1,files:novel.files.length});
    const text=(field,value,item)=>{const input=String(value);let pending='',partBytes=0;let part=0;for(const point of input){const n=Buffer.byteLength(point);if(partBytes+n>32768){records.push({type:'text',novel:index,field,item,part:part++,last:false,value:pending});texts++;pending='';partBytes=0;}pending+=point;partBytes+=n;}records.push({type:'text',novel:index,field,item,part,last:true,value:pending});texts++;};
    for(const field of ['toc_url','title','author','sitename','domain','attribute_tag','ncode','subtitle','general_lastup','last_update']){if(novel[field]!==undefined&&novel[field]!==null)text(field,novel[field]);}
    for(const [item,tag]of(novel.tags??[]).entries())text('tag',tag,item);
    for(const [item,url]of(novel.virtual_members??[]).entries())text('member',url,item);
    for(const file of novel.files){const bytes=Buffer.from(file.bytes??file.data);const value={type:'file',file_id:ordinal++,novel:index,path:file.path,size:String(bytes.length),sha256:sha256(bytes)};records.push(value);files.push({...value,bytes});}
  }
  const fingerprint=sha256(Buffer.from(JSON.stringify(records)));
  const u32=value=>{const bytes=Buffer.alloc(4);bytes.writeUInt32BE(value);return bytes;};
  const u64=value=>{const bytes=Buffer.alloc(8);bytes.writeBigUInt64BE(BigInt(value));return bytes;};
  const selected=createHash('sha256').update(Buffer.from('narou-transfer-projection-v2\0')).update(u32(policyVersion)).update(Buffer.from(fingerprint,'hex')).update(Buffer.from(profile+'\0'));
  const sorted=[...new Set(roles)].sort();selected.update(u32(sorted.length));for(const role of sorted)selected.update(Buffer.from(role+'\0'));
  for(const record of records){if(record.type==='novel')selected.update(Buffer.from('N')).update(u32(record.index));if(record.type==='file'){const path=Buffer.from(record.path);selected.update(Buffer.from('F')).update(u64(path.length)).update(path).update(u64(record.size)).update(Buffer.from(record.sha256,'hex'));}}
  const header={type:'header',format_version:2,protocol_version:2,policy_version:policyVersion,profile,extra_roles:roles,fingerprint,selection_digest:selected.digest('hex'),novels:novels.length,files:ordinal,texts};
  const bytes=Buffer.from([header,...records,{type:'end',novels:novels.length,files:ordinal,texts}].map(r=>JSON.stringify(r)+'\n').join(''));
  return {bytes,files,fingerprint};
}
export function selectionDigest(revision,profile,roles,items){
  const rows=items.map(item=>`${item.novel_ordinal}\0${item.logical_path}\0${item.size}\0${item.role}`).sort((a,b)=>Buffer.compare(Buffer.from(a),Buffer.from(b)));
  return sha256(Buffer.from(`v2\0${revision}\0${profile}\0${[...roles].sort().join(',')}\0${rows.map(r=>r+'\x01').join('')}`));
}

function missingCloudflareRoute(response) {
  return response.status === 404 && response.headers.has('cf-ray')
    && !response.headers.has('x-test-transfer-primary-only');
}

const MAX_EDGE_REPLAYS = 3;
const sessionRoute = /^\/api\/transfer\/v2\/(?:imports|exports|sessions)\/[^/]+$/;
const controlRoute = /^\/api\/transfer\/v2\/(?:imports|exports)\/[^/]+\/(?:manifest\/seal|advance|select|publish|ready|pause|resume|complete)$/;
const uploadRoute = /^\/api\/transfer\/v2\/imports\/[^/]+\/(?:chunks|manifest\/chunks\/\d+)$/;
const hasIdentity = value => typeof value === 'string' && value.length > 0;

function canReplayTransferRequest(path, { method = 'GET', body, headers }) {
  const [route, query] = path.split('?', 2);
  if (method === 'GET') return body === undefined;
  if (method === 'PUT') {
    // The server checks the digest and finality of each already-received chunk.
    return uploadRoute.test(route) && Buffer.isBuffer(body);
  }
  if (method === 'DELETE') {
    return sessionRoute.test(route) && headers.has('if-match')
      && hasIdentity(new URLSearchParams(query).get('operation_id') ?? headers.get('x-transfer-operation-id'));
  }
  if (method !== 'POST' || typeof body !== 'string') return false;
  const isCreate = route === '/api/transfer/v2/imports' || route === '/api/transfer/v2/exports';
  if (!isCreate && !controlRoute.test(route)) return false;
  try {
    const data = JSON.parse(body);
    // The creation journal deduplicates by request_id. Keep the original
    // ticket and secret rather than creating a new request after a lost ACK.
    return isCreate
      ? hasIdentity(data?.request_id) && hasIdentity(data?.creation_ticket)
      : hasIdentity(data?.operation_id) && (route.endsWith('/advance') || headers.has('if-match'));
  } catch {
    return false;
  }
}

export async function waitForDeployedTransfer(client){
  const path='/api/transfer/v2/capabilities';
  for(let attempt=0;attempt<15;attempt++){
    const response=await fetch(client.base+path,{redirect:'manual',headers:{Authorization:`Bearer ${client.token}`,...client.extraHeaders},
      signal:AbortSignal.timeout(5000)});
    const marker=response.headers.get('x-test-transfer-primary-only');
    if(missingCloudflareRoute(response)){
      // A newly published Workers route can be absent at this edge even
      // after provisioning observed another successful readiness request.
      await response.body?.cancel();
      if(attempt<14){await sleep(1000);continue;}
    }
    assert.equal(response.status,200,'deployed transfer readiness HTTP status');
    assert.equal(marker,'true','deployed transfer readiness must check primary access');
    assert(response.headers.has('cf-ray'),'the memory test must execute on Cloudflare');
    const caps=await response.json();
    assert.equal(caps.protocol_version,2);
    assert.equal(caps.storage.import.allowed,true);
    return;
  }
}
export function transferClient(base,token,{verifyPrimary=false,fetcher=fetch}={}){
  const client={base,token,verifyPrimary,requests:[],
    async request(path,init={}){const bytes=init.body===undefined?0:Buffer.byteLength(init.body);if(init.headers?.['content-type']==='application/octet-stream')assert(bytes<=WIRE);
      const v2=client.verifyPrimary&&path.startsWith('/api/transfer/v2/');
      const request = {...init, redirect:'manual', headers:new Headers({Authorization:`Bearer ${token}`,...client.extraHeaders,...init.headers})};
      const canReplay = v2 && canReplayTransferRequest(path, request);
      const observe = client.verifyPrimary && (request.method??'GET')==='GET' &&
        path.split('?',1)[0]==='/_fixture/transfer-observe';
      let response;
      for (let attempt = 0; ; attempt++) {
        client.requests.push({path,method:init.method??'GET',bytes,type:init.headers?.['content-type']??''});
        const retryAvailable = attempt < MAX_EDGE_REPLAYS && !request.signal?.aborted;
        let delay = 100 * (attempt + 1);
        let reason;
        try {
          response = await fetcher(client.base + path, request);
        } catch (error) {
          const code = error?.cause?.code;
          if (!retryAvailable || !(canReplay || observe) || request.signal?.aborted || !['ECONNRESET','UND_ERR_SOCKET'].includes(code)) throw error;
          reason = {code};
        }
        if (!reason) {
          const edge404 = missingCloudflareRoute(response);
          const edge5xx = response.status >= 500 && !response.headers.has('x-test-transfer-primary-only');
          if (!retryAvailable || !canReplay || (!edge404 && !edge5xx)) break;
          delay = edge404 ? 1000 : delay;
          reason = {status:response.status};
          // Do not turn a completed response into a canceled invocation.
          await drainResponse(response);
        }
        // Preserve operation identity on replay. The fetcher assigns each
        // wire attempt a different trace sequence in the deployed fixture.
        console.log(JSON.stringify({event:'transfer_edge_retry',...reason,attempt:attempt+1,path}));
        await sleep(delay);
      }
      if(v2)assert.equal(response.headers.get('x-test-transfer-primary-only'),'true',`all v2 store access must use primary (HTTP ${response.status}, marker ${response.headers.get('x-test-transfer-primary-only')===null?'absent':'present'})`);
      const wasmBytes=Number(response.headers.get('x-test-transfer-wasm-bytes'));if(Number.isSafeInteger(wasmBytes)&&wasmBytes>0)client.memoryHighWater=Math.max(client.memoryHighWater??0,wasmBytes);if(response.headers.get('x-test-transfer-fault-fired')==='true')client.faults=(client.faults??0)+1;return response;},
    async json(path,init={}){for(let attempt=0;;attempt++){const response=await client.request(path,init);const text=await response.text();let body;try{body=text?JSON.parse(text):null;}catch{throw new Error(`HTTP ${response.status}: ${text.slice(0,1000)}`);}
      if(((response.status===503||response.status===423)&&body?.error?.retryable||response.status===409&&body?.error?.code==='too_many_sessions')&&attempt<1200){await sleep(50);continue;}return{response,body};}},
    async caps(){const{response,body}=await client.json('/api/transfer/v2/capabilities');assert.equal(response.status,200,JSON.stringify(body));assert.equal(body.protocol_version,2);assert.equal(body.storage.import.allowed,true,JSON.stringify(body));return body;},
    async createImport(novels,{policy='skip',profile='runtime',roles=[],requestId=randomUUID(),secret=randomBytes(32).toString('base64url')}={}){
      const caps=await client.caps();const wire=wireManifest(novels,caps.storage.policy_version,profile,roles);
      const body={request_id:requestId,creation_ticket:caps.creation_ticket,source_fingerprint:wire.fingerprint,profile,extra_roles:roles,policy_version:caps.storage.policy_version,conflict_policy:policy,manifest_size:String(wire.bytes.length),manifest_sha256:sha256(wire.bytes)};
      const init={method:'POST',headers:{'content-type':'application/json','x-narou-transfer-secret':secret},body:JSON.stringify(body)};
      const created=await client.json('/api/transfer/v2/imports',init);assert.equal(created.response.status,200,JSON.stringify(created.body));
      return session(client,'imports',created.body,secret,{wire,create:init});
    },
    async importData(novels,options={}){const s=await client.createImport(novels,options);await s.manifest();await s.until(v=>v.state==='transferring');await s.upload();await s.op('ready');const plan=await s.pages('plan');await s.op('publish',{plan_revision:s.status.plan_revision});await s.until(v=>['completed','completed_with_errors'].includes(v.state));const final=s.status;await s.drain();return{status:final,plan,session:s};},
    async beginExport(ids,{profile='runtime',roles=[]}={}){const caps=await client.caps();const secret=randomBytes(32).toString('base64url');const body={request_id:randomUUID(),creation_ticket:caps.creation_ticket,ids:Array.isArray(ids)?ids:[Number(ids)],profile,extra_roles:roles};
      const created=await client.json('/api/transfer/v2/exports',{method:'POST',headers:{'content-type':'application/json','x-narou-transfer-secret':secret},body:JSON.stringify(body)});assert.equal(created.response.status,200,JSON.stringify(created.body));return session(client,'exports',created.body,secret);},
    async createExport(ids,{profile='runtime',roles=[]}={}){const s=await client.beginExport(ids,{profile,roles});await s.until(v=>v.state==='selecting');const catalog=await s.pages('catalog');await s.op('select',{profile,extra_roles:roles,selection_digest:selectionDigest(s.status.catalog_revision,profile,roles,catalog)});await s.until(v=>['ready','reading'].includes(v.state));return s;},
    async exportData(ids,options={}){const s=await client.createExport(ids,options);return s.download();},
  };return client;
}
function session(client,direction,status,secret,extra={}){
 const s={client,direction,status,secret,...extra,get path(){return`/api/transfer/v2/${direction}/${s.status.session_id}`;},
  headers(extra={}){return{'x-narou-transfer-secret':secret,...extra};},
  async get(){const r=await client.json(s.path,{headers:s.headers()});assert.equal(r.response.status,200,JSON.stringify(r.body));s.status=r.body;return r.body;},
  async pages(name){let items=[],cursor=null;do{const r=await client.json(`${s.path}/${name}${cursor===null?'':`?cursor=${encodeURIComponent(cursor)}`}`,{headers:s.headers()});assert.equal(r.response.status,200,JSON.stringify(r.body));assert(r.body.items.length<=100);assert(Buffer.byteLength(JSON.stringify(r.body))<=256*1024);items.push(...r.body.items);cursor=r.body.next_cursor??null;}while(cursor!==null);return items;},
  async op(action,body={},identity=randomUUID(),revision=s.status.revision){const init={method:'POST',headers:s.headers({'content-type':'application/json','if-match':`"${revision}"`}),body:JSON.stringify({...body,operation_id:identity})};const r=await client.json(`${s.path}/${action}`,init);assert.equal(r.response.status,200,JSON.stringify(r.body));if(BigInt(r.body.revision)>=BigInt(s.status.revision))s.status=r.body;return{...r,identity,init};},
  async step(){const r=await client.json(`${s.path}/advance`,{method:'POST',headers:s.headers({'content-type':'application/json'}),body:JSON.stringify({operation_id:s.status.operation_id})});assert.equal(r.response.status,200,JSON.stringify(r.body));s.status=r.body;return r;},
  async until(predicate){await s.get();for(let i=0;i<100000;i++){if(predicate(s.status))return s.status;if(['failed','expired','cancelled'].includes(s.status.state))throw new Error(JSON.stringify(s.status));assert(['advance','finalize_file'].includes(s.status.next_action),JSON.stringify(s.status));await s.step();}throw new Error('advance did not converge');},
  async drain(){await s.get();for(let i=0;i<100000;i++){if(s.status.next_action==='none')return;if(!s.status.operation_id)return;await s.step();}throw new Error('GC did not converge');},
  async batch(records,manifest=false){const bytes=encodeBatch(records);const path=manifest?`${s.path}/manifest/chunks/${records[0].chunk_index}`:`${s.path}/chunks`;const r=await client.json(path,{method:'PUT',headers:s.headers({'content-type':'application/octet-stream','x-transfer-record-count':String(records.length),'x-transfer-body-bytes':String(bytes.length)}),body:bytes});assert.equal(r.response.status,200,JSON.stringify(r.body));s.status=r.body.status??r.body;return r.body;},
  async manifest(){for(let start=0,index=0;start<s.wire.bytes.length;index++){const end=Math.min(start+CHUNK,s.wire.bytes.length);const r=await s.batch([{file_id:0xffffffff,chunk_index:index,bytes:s.wire.bytes.subarray(start,end),final:end===s.wire.bytes.length}],true);assert.equal(r.failure,null);start=end;}if(s.status.next_action==='finalize_file')await s.until(v=>v.next_action!=='finalize_file');await s.op('manifest/seal',{size:String(s.wire.bytes.length),sha256:sha256(s.wire.bytes)});},
  async upload(){const files=await s.pages('files');let batch=[],size=0;const flush=async()=>{if(batch.length){const result=await s.batch(batch);assert.equal(result.failure,null,JSON.stringify(result));batch=[];size=0;}};
    for(const file of files){if(!file.upload_required)continue;const source=s.wire.files[file.manifest_file_index];assert(source);let offset=Number(file.confirmed_bytes),index=file.next_chunk_index;
      do{const end=Math.min(offset+CHUNK,source.bytes.length);const record={file_id:file.file_id,chunk_index:index++,bytes:source.bytes.subarray(offset,end),final:end===source.bytes.length};const n=encodeBatch([record]).length;if(batch.length&&(batch.length===100||size+n>WIRE))await flush();batch.push(record);size+=n;offset=end;}while(offset<source.bytes.length);
    }await flush();if(s.status.next_action==='finalize_file')await s.until(v=>v.next_action!=='finalize_file');},
  async cancel(){await s.get();const identity=randomUUID();const r=await client.json(`${s.path}?operation_id=${identity}`,{method:'DELETE',headers:s.headers({'if-match':`"${s.status.revision}"`})});assert.equal(r.response.status,200,JSON.stringify(r.body));s.status=r.body;await s.until(v=>v.state==='cancelled');await s.drain();},
  async download(){const chunks=[];for(let index=0;;index++){const r=await client.request(`${s.path}/manifest/chunks/${index}`,{headers:s.headers()});assert.equal(r.status,200);assert.match(r.headers.get('content-type'),/octet-stream/);const bytes=Buffer.from(await r.arrayBuffer());assert(bytes.length<=WIRE);assert.equal(Number(r.headers.get('x-transfer-body-bytes')),bytes.length);chunks.push(bytes);if(r.headers.get('x-transfer-last')==='true'){assert.equal(Number(r.headers.get('x-transfer-manifest-bytes')),chunks.reduce((n,v)=>n+v.length,0));break;}}
    const manifest=JSON.parse(Buffer.concat(chunks));const pieces=new Map();const expected=[];for(const[index,novel]of manifest.novels.entries())for(const file of novel.files)expected.push({...file,path:`novels/${index}/${file.path}`});
    for(let batch=0;;batch++){const r=await client.request(`${s.path}/batches/${batch}`,{headers:s.headers()});assert.equal(r.status,200);const bytes=Buffer.from(await r.arrayBuffer());assert(bytes.length<=WIRE);assert.equal(Number(r.headers.get('x-transfer-body-bytes')),bytes.length);let offset=0,count=0;
      while(offset<bytes.length){const n=bytes.readUInt32BE(offset);offset+=4;assert(n>0&&n<=512);const header=JSON.parse(bytes.subarray(offset,offset+n));offset+=n;const length=Number(header.payload_size);const data=bytes.subarray(offset,offset+length);assert.equal(data.length,length);offset+=length;const value=pieces.get(header.file_id)??{next:0,chunks:[],final:false};assert.equal(header.chunk_index,value.next++);assert.equal(value.final,false);value.final=header.final;value.chunks.push(data);pieces.set(header.file_id,value);count++;}
      assert.equal(count,Number(r.headers.get('x-transfer-record-count')));assert(count<=100);if(r.headers.get('x-transfer-last')==='true')break;
    }
    const files=new Map();for(const[index,entry]of expected.entries()){const value=pieces.get(index);assert(value?.final,entry.path);const bytes=Buffer.concat(value.chunks);assert.equal(BigInt(bytes.length),BigInt(entry.size));assert.equal(sha256(bytes),entry.sha256);files.set(entry.path,bytes);}assert.equal(pieces.size,expected.length);
    await s.op('complete',{archive_size:'0',archive_sha256:null});await s.drain();return{manifest,files,session:s};
  },
 };return s;
}
export function fixtureNovel(slug,files=[],extra={}){const title=`Transfer ${slug}`,toc_url=`https://example.test/${slug}/`;return{toc_url,title,author:'Fixture',sitename:'Transfer',domain:'example.test',ncode:slug,novel_type:1,tags:['transfer'],attribute_tag:'小説',files:[{path:'toc.yaml',bytes:Buffer.from(`title: ${title}\nauthor: Fixture\ntoc_url: ${toc_url}\nsubtitles: []\nnovel_type: 1\n`)},...files],...extra};}
export async function sourceNamespaceContracts(client,slug){
  const parent=fixtureNovel(`${slug}-parent`,[{path:'own.bin',bytes:Buffer.from('parent-only')}],{ncode:'12'});assert.equal((await client.importData([parent])).status.state,'completed');
  const parentId=(await client.json('/api/novels?limit=1000')).body.data.find(record=>record.toc_url===parent.toc_url).id;
  const seed=async mode=>{const novel=fixtureNovel(`${slug}-${mode}`,[{path:'secret.bin',bytes:Buffer.from(`private-${mode}`)}]);const result=await client.json('/_fixture/transfer-namespace',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({id:parentId,mode,toc_url:novel.toc_url,files:novel.files.map(file=>({path:file.path,data:file.bytes.toString('base64'),crc32:crc32(file.bytes)}))})});assert.equal(result.response.status,200,JSON.stringify(result.body));return result.body.id;};
  const siblingId=await seed('sibling');const clean=await client.exportData([parentId]);assert.equal(clean.manifest.novels.length,1);assert.equal(clean.files.get('novels/0/own.bin').toString(),'parent-only');assert([...clean.files.keys()].every(path=>!path.includes('secret.bin')),'a lexical sibling must not be enumerated as source files');
  const childId=await seed('child');const listed=await client.json('/api/novels?limit=1000');assert.equal(listed.response.status,200);assert([parentId,siblingId,childId].every(id=>listed.body.data.some(row=>row.id===id)),'metadata listing remains available for legacy collisions');
  for(const ids of[[parentId],[parentId,childId]]){
    const source=await client.beginExport(ids);await source.until(value=>value.state==='failed');assert.match(String(source.status.error),/source_namespace_overlap/);
    const observed=(await client.json(`/_fixture/transfer-observe?session=${source.status.session_id}`)).body;assert.equal(observed.stages.length,0,'namespace collisions must fail before any source snapshots or metadata staging');assert.equal(observed.frozen.fragments,0);assert.equal(observed.snapshot.gates,0);assert.equal(observed.snapshot.pinned,0);assert(!client.requests.some(request=>request.path.startsWith(`${source.path}/batches/`)),'no child body batch may be requested');await source.drain();
  }
  return{parentId,siblingId,childId};
}

async function contracts(){
  assert(existsSync(join(root,'build/worker/shim.mjs')),'build Worker artifacts before running this contract');mkdirSync(join(root,'.wrangler'),{recursive:true});const work=mkdtempSync(join(root,'.wrangler/transfer-v2-'));const token='isolated-transfer-v2-token';const windows=process.platform==='win32';const npx=windows?'npx.cmd':'npx';const pin=process.env.NAROU_WRANGLER_VERSION??'4.151.3';assert.match(pin,/^\d+\.\d+\.\d+$/);const children=[];
  const run=(args,phase)=>{const r=spawnSync(npx,[`wrangler@${pin}`,...args,'--config',phase.config,'--persist-to',phase.persist],{cwd:root,shell:windows,encoding:'utf8',env:{...process.env,CI:'true',WRANGLER_SEND_METRICS:'false'}});assert.equal(r.status,0,`${r.stdout}\n${r.stderr}`);return r.stdout;};
  const sql=(phase,query)=>{const file=join(phase.dir,'fixture.sql');writeFileSync(file,query);return run(['d1','execute','DB','--local','--file',file],phase);};
  const setup=name=>{const dir=join(work,name);mkdirSync(dir);const config=join(dir,'wrangler.toml');let toml=readFileSync(join(root,'wrangler.toml'),'utf8').replace(/\[build\][^\[]*/,'').replace(/^main = .*$/m,`main = ${JSON.stringify(join(root,'tests/transfer_failure_worker.mjs').replaceAll('\\','/'))}`).replace(/^directory = .*$/m,`directory = ${JSON.stringify(join(root,'public').replaceAll('\\','/'))}`).replace(/^migrations_dir = .*$/m,`migrations_dir = ${JSON.stringify(join(root,'migrations').replaceAll('\\','/'))}`);writeFileSync(config,toml);writeFileSync(join(dir,'.dev.vars'),`NAROU_ADMIN_TOKEN=${token}\nNAROU_RS_LOGIN_KEY=${Buffer.alloc(32,7).toString('base64')}\n`);const phase={dir,config,persist:join(dir,'state')};run(['d1','migrations','apply','DB','--local'],phase);sql(phase,`INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES('inv','content_backend','"d1"','d1'),('inv','asset_backend','"d1"','d1');`);return phase;};
  const stop=async child=>{if(child.exitCode!==null||child.signalCode!==null)return;const ended=once(child,'exit');if(windows)spawnSync('taskkill',['/pid',String(child.pid),'/T','/F']);else{try{process.kill(-child.pid,'SIGTERM');}catch(error){if(error.code!=='ESRCH')throw error;}}await ended;};
  const start=async phase=>{const net=await import('node:net');const probe=net.createServer().listen(0,'127.0.0.1');await once(probe,'listening');const port=probe.address().port;await new Promise(r=>probe.close(r));let logs='';const child=spawn(npx,[`wrangler@${pin}`,'dev','--config',phase.config,'--persist-to',phase.persist,'--port',String(port),'--test-scheduled'],{cwd:root,shell:windows,detached:!windows,stdio:['ignore','pipe','pipe'],env:{...process.env,CI:'true',WRANGLER_SEND_METRICS:'false'}});children.push(child);for(const stream of[child.stdout,child.stderr])stream.on('data',bytes=>{logs=(logs+bytes).slice(-50000);});const base=`http://127.0.0.1:${port}`;for(let i=0;i<480;i++){if(child.exitCode!==null)throw new Error(logs);try{if((await fetch(base+'/health/live')).ok)return{child,client:transferClient(base,token,{verifyPrimary:true})};}catch{}await sleep(250);}throw new Error(logs);};
  try{
    const a=setup('a'),b=setup('b');let wa=await start(a),wb=await start(b);let ca=wa.client,cb=wb.client;
    assert.equal((await fetch(ca.base+'/api/transfer/v2/capabilities')).status,401);
    assert.equal((await ca.request('/api/transfer/export?ids=1')).status,404);assert.equal((await ca.request('/api/transfer/import',{method:'POST',body:'retired'})).status,404);
    const large=Buffer.alloc(17*1024*1024);for(let i=0;i<large.length;i++)large[i]=(i*31+(i>>>7))&255;
    const first=fixtureNovel('first',[{path:'notes.bin',bytes:large},{path:'replace.txt',bytes:Buffer.from('user settings')},{path:'本文/cache/old.yaml',bytes:Buffer.from('body: historic\n')}]);
    const imported=await ca.importData([first],{profile:'complete'});assert.equal(imported.status.state,'completed');assert.equal(imported.plan[0].action,'create');
    let listed=(await ca.json('/api/novels')).body.data;const id=listed.find(v=>v.toc_url===first.toc_url).id;
    const runtime=await ca.exportData([id]);assert(!runtime.files.has('novels/0/本文/cache/old.yaml'));assertFileBytes(runtime.files.get('novels/0/notes.bin'),large,'runtime notes.bin');
    const complete=await ca.exportData([id],{profile:'complete'});assert(complete.files.has('novels/0/本文/cache/old.yaml'));
    const transferred=complete.manifest.novels.map((novel,index)=>({...novel,files:novel.files.map(file=>({path:file.path,bytes:complete.files.get(`novels/${index}/${file.path}`)}))}));
    const second=await cb.importData(transferred,{profile:'complete'});assert.equal(second.status.state,'completed');const idb=(await cb.json('/api/novels')).body.data.find(v=>v.toc_url===first.toc_url).id;
    const round=await cb.exportData([idb],{profile:'complete'});for(const[path,bytes]of complete.files)assertFileBytes(round.files.get(path),bytes,path);
    const replacement=fixtureNovel('first',[{path:'notes.bin',bytes:Buffer.from('replacement')}]);const overwrite=await cb.importData([replacement],{policy:'overwrite'});assert.equal(overwrite.status.state,'completed');const after=await cb.exportData([idb],{profile:'complete'});assert.equal(after.files.get('novels/0/notes.bin').toString(),'replacement');assert.equal(after.files.get('novels/0/replace.txt').toString(),'user settings');assert(after.files.has('novels/0/本文/cache/old.yaml'));
    const skipped=await cb.importData([replacement]);assert.equal(skipped.plan[0].action,'skip');assert(!skipped.session.client.requests.slice(-10).some(r=>r.path.endsWith('/chunks')&&r.method==='PUT'));
    console.log('PASS bounded 17MiB file, runtime/complete, two libraries, overwrite preservation, Skip');

    const replay=await ca.createImport([fixtureNovel('replay')]);const again=await ca.json('/api/transfer/v2/imports',replay.create);assert.equal(again.body.session_id,replay.status.session_id);const conflict=await ca.json('/api/transfer/v2/imports',{...replay.create,headers:{...replay.create.headers,'x-narou-transfer-secret':randomBytes(32).toString('base64url')}});assert.equal(conflict.response.status,409);await replay.manifest();await replay.until(v=>v.state==='transferring');
    await stop(wa.child);wa=await start(a);ca=wa.client;replay.client.base=ca.base;await replay.upload();const ack=await replay.op('ready');const ackAgain=await replay.client.json(`${replay.path}/ready`,ack.init);assert.equal(ackAgain.response.status,200);assert.equal(ackAgain.body.revision,ack.body.revision);await replay.op('publish',{plan_revision:replay.status.plan_revision});await replay.until(v=>v.state==='completed');await replay.drain();
    console.log('PASS create/control ACK replay and workerd restart resume');

    const partial=await ca.createImport([fixtureNovel('partial',[{path:'a.bin',bytes:Buffer.from('first')},{path:'b.bin',bytes:Buffer.from('second')}])]);await partial.manifest();await partial.until(v=>v.state==='transferring');const files=await partial.pages('files');const needed=files.filter(v=>v.upload_required);const good=needed[0],bad=needed[1];const records=[good,bad].map(v=>({file_id:v.file_id,chunk_index:0,bytes:partial.wire.files[v.file_id].bytes,final:true}));records[1].sha256='0'.repeat(64);const result=await partial.batch(records);assert.equal(result.receipts.length,1);assert(result.failure);const positions=await partial.pages('files');assert.equal(positions.find(v=>v.file_id===good.file_id).state,'verified');assert.equal(positions.find(v=>v.file_id===bad.file_id).confirmed_bytes,'0');const confirmed=(await partial.get()).confirmed_bytes;const duplicate=await partial.batch([records[0]]);assert.equal(duplicate.receipts[0].outcome,'duplicate');assert.equal((await partial.get()).confirmed_bytes,confirmed);await partial.upload();await partial.op('ready');await partial.op('publish',{plan_revision:partial.status.plan_revision});await partial.until(v=>v.state==='completed');await partial.drain();
    console.log('PASS partial-batch durable receipt, mismatched SHA, identical retry without double count');

    const raw=await ca.createImport([fixtureNovel('raw-forbidden',[{path:'raw/page.html',bytes:Buffer.from('<html/>')}])],{profile:'complete'});await raw.manifest();await raw.until(v=>v.state==='failed');assert.match(raw.status.error,/file_policy/);await raw.drain();assert(!(await ca.json('/api/novels')).body.data.some(v=>v.toc_url.includes('raw-forbidden')));
    const long=fixtureNovel('unicode',[{path:'memo.txt',bytes:Buffer.from('unknown file retained')}],{subtitle:'あ\n"'.repeat(30000)});const longResult=await ca.importData([long]);assert.equal(longResult.status.state,'completed');
    console.log('PASS Worker raw policy is enforced before file payload; fragmented metadata and unknown files survive');

    const one=fixtureNovel('member-one',[],{attribute_tag:'漫画'}),two=fixtureNovel('member-two');const parent={toc_url:'https://narou.rs/virtual/0123456789abcdef0123456789abcdef',title:'Group',author:'Fixture',sitename:'Transfer',novel_type:1,tags:['group'],attribute_tag:'小説',virtual_members:[two.toc_url,one.toc_url],files:[{path:'setting.ini',bytes:Buffer.from('enable_yokogaki=true\n')}]};
    const groupResult=await ca.importData([one,two,parent]);assert.equal(groupResult.status.state,'completed');listed=(await ca.json('/api/novels?limit=1000')).body.data;const parentId=listed.find(v=>v.toc_url===parent.toc_url).id;const memberId=listed.find(v=>v.toc_url===one.toc_url).id;const group=await ca.exportData([parentId,memberId],{profile:'complete'});assert.equal(group.manifest.novels.length,3);assert.deepEqual(group.manifest.novels[2].virtual_members,parent.virtual_members);assert(!group.manifest.novels[2].tags.some(t=>t.startsWith('vs_')));
    const groupInput=group.manifest.novels.map((novel,index)=>({...novel,files:novel.files.map(file=>({path:file.path,bytes:group.files.get(`novels/${index}/${file.path}`)}))}));await cb.importData([one]);const groupImport=await cb.importData(groupInput,{profile:'complete'});assert.equal(groupImport.status.state,'completed');assert(groupImport.plan.some(v=>v.action==='skip'));const groups=(await cb.json('/api/virtual_novels')).body.novels;assert(groups.some(v=>v.title==='Group'&&v.members.length===2));
    console.log('PASS virtual expansion, deduplication, mixed attributes, ordered destination membership and skipped member');
    const vanishingMember=fixtureNovel('vanishing-member');await ca.importData([vanishingMember]);
    const vanishingParent={...parent,toc_url:`https://narou.rs/virtual/${randomBytes(16).toString('hex')}`,title:'Vanishing member group',virtual_members:[vanishingMember.toc_url]};
    const vanishing=await ca.createImport([vanishingMember,vanishingParent]);await vanishing.manifest();await vanishing.until(v=>v.state==='transferring');await vanishing.upload();await vanishing.op('ready');await vanishing.op('publish',{plan_revision:vanishing.status.plan_revision});
    let memberFault=false,memberHttpError=false;for(let i=0;i<2000;i++){
      await vanishing.get();const response=await ca.request(`${vanishing.path}/advance`,{method:'POST',headers:vanishing.headers({'content-type':'application/json','x-narou-test-transfer-fault':'member_before_read'}),body:JSON.stringify({operation_id:vanishing.status.operation_id})});const body=JSON.parse(await response.text());
      if(response.headers.get('x-test-transfer-fault-fired')==='true'){memberFault=true;if(response.status!==200){assert.equal(body.error?.code,'plan_stale',JSON.stringify(body));memberHttpError=true;}else vanishing.status=body;break;}
      assert.equal(response.status,200,JSON.stringify(body));vanishing.status=body;
    }assert(memberFault,'the real virtual-member join must observe a concurrent deletion');
    if(memberHttpError){await vanishing.get();if(!['failed','completed','completed_with_errors','cancelled'].includes(vanishing.status.state))await vanishing.cancel();else await vanishing.drain();}
    else{await vanishing.until(v=>['failed','completed','completed_with_errors'].includes(v.state));const failedPlan=await vanishing.pages('plan');assert.match(JSON.stringify({status:vanishing.status,plan:failedPlan}),/plan_stale/);await vanishing.drain();}
    const afterVanishing=(await ca.json('/api/novels?limit=1000')).body.data;assert(!afterVanishing.some(row=>row.toc_url===vanishingParent.toc_url),'a parent with a deleted member must not be published');assert(afterVanishing.some(row=>row.toc_url===one.toc_url),'an unrelated member must remain managed');
    console.log('PASS concurrent virtual-member deletion is a plan conflict, not a deserialization/platform retry');

    const uncertain=transferClient(cb.base,token,{verifyPrimary:true});uncertain.extraHeaders={'x-narou-test-transfer-fault':'commit_after'};
    const uncertainResult=await uncertain.importData([fixtureNovel('commit-ack')]);assert.equal(uncertainResult.status.state,'completed');assert(uncertain.faults>0,'fixture must interrupt the real committed D1 batch acknowledgement');
    const committedRows=(await cb.json('/api/novels?limit=1000')).body.data.filter(row=>row.toc_url==='https://example.test/commit-ack/');assert.equal(committedRows.length,1);
    const blocked=await cb.createImport([fixtureNovel('first',[{path:'notes.bin',bytes:Buffer.from('must not become canonical')}])],{policy:'overwrite'});await blocked.manifest();await blocked.until(v=>v.state==='transferring');await blocked.upload();await blocked.op('ready');await blocked.op('publish',{plan_revision:blocked.status.plan_revision});
    for(let i=0;i<100;i++){const observed=(await cb.json(`/_fixture/transfer-observe?session=${blocked.status.session_id}`)).body;if(observed.publications[0]?.state==='committing')break;await blocked.step();}
    await blocked.get();const fail=await cb.request(`${blocked.path}/advance`,{method:'POST',headers:blocked.headers({'content-type':'application/json','x-narou-test-transfer-fault':'commit_before'}),body:JSON.stringify({operation_id:blocked.status.operation_id})});assert([423,503].includes(fail.status));assert.equal(fail.headers.get('x-test-transfer-fault-fired'),'true');await fail.arrayBuffer();
    const beforeCancel=(await cb.json(`/_fixture/transfer-observe?session=${blocked.status.session_id}`)).body;assert.equal(beforeCancel.targets[0].committed,0);await blocked.cancel();
    const [rolledBack]=await blocked.pages('plan');
    assert.deepEqual(rolledBack.receipt,{novel_ordinal:0,action:'overwrite',outcome:'rolled_back',assigned_id:idb,files_installed:0,error:null});
    const preserved=await cb.exportData([idb]);assert.equal(preserved.files.get('novels/0/notes.bin').toString(),'replacement');
    console.log('PASS actual D1 commit ACK loss is idempotent; pre-commit failure/cancel preserves old canonical bytes');

    let index='version: 2\nsources:\n';for(let i=0;i<50000;i++)index+=`  'https://example.test/index/${i}/${'x'.repeat(160)}': shared.png\n`;assert(Buffer.byteLength(index)<16*1024*1024);
    const indexNovel=fixtureNovel('large-index',[{path:'.illustration_cache.yaml',bytes:Buffer.from(index)},{path:'挿絵/shared.png',bytes:Buffer.from([7])}]);const indexImport=await ca.importData([indexNovel]);assert.equal(indexImport.status.state,'completed');
    const indexId=(await ca.json('/api/novels?limit=1000')).body.data.find(row=>row.toc_url===indexNovel.toc_url).id;const indexCaps=await ca.caps();const indexSecret=randomBytes(32).toString('base64url');const indexCreated=await ca.json('/api/transfer/v2/exports',{method:'POST',headers:{'content-type':'application/json','x-narou-transfer-secret':indexSecret},body:JSON.stringify({request_id:randomUUID(),creation_ticket:indexCaps.creation_ticket,ids:[indexId],profile:'runtime',extra_roles:[]})});assert.equal(indexCreated.response.status,200,JSON.stringify(indexCreated.body));const indexSession=session(ca,'exports',indexCreated.body,indexSecret);
    let initialized=false,lostIndexStage;for(let i=0;i<80;i++){
      const response=await ca.request(`${indexSession.path}/advance`,{method:'POST',headers:indexSession.headers({'content-type':'application/json',...(!lostIndexStage?{'x-narou-test-transfer-fault':'initializer_after'}:{})}),body:JSON.stringify({operation_id:indexSession.status.operation_id})});const text=await response.text();
      const observed=(await ca.json(`/_fixture/transfer-observe?session=${indexSession.status.session_id}`)).body;
      if(response.headers.get('x-test-transfer-fault-fired')==='true'){assert([500,503].includes(response.status),text);const stage=observed.stages.find(stage=>stage.stagepurpose==='illustration_index');assert(stage?.initialized);assert.equal(stage.sealed,0);lostIndexStage=stage.stage_id;continue;}
      assert.equal(response.status,200,text);indexSession.status=JSON.parse(text);
      if(observed.stages.some(stage=>stage.stagepurpose==='illustration_index'&&stage.sealed)){assert(lostIndexStage);assert.equal(observed.stages.length,1);assert.equal(observed.stages[0].stage_id,lostIndexStage);assert(observed.stages.every(stage=>stage.stagepurpose==='illustration_index'),'no selected image/body snapshots may exist before select');initialized=true;break;}
    }assert(initialized,'real illustration metadata initializer must run');
    assert.equal(indexSession.status.state,'cataloguing');const indexMemory=(await ca.json('/_fixture/transfer-memory')).body;assert.equal(indexMemory.instances,1);assert(indexMemory.highWater<112*1024*1024,JSON.stringify(indexMemory));await indexSession.cancel();
    console.log('PASS 50000-entry real YAML metadata initializer is bounded, reuses completed stage after ACK loss, and never snapshots images before selection');

    const story='\x01'.repeat(3*1024*1024);const storyNovel=fixtureNovel('escaped-story',[],{subtitle:story});storyNovel.files[0].bytes=Buffer.concat([storyNovel.files[0].bytes,Buffer.from(`story: "${'\\x01'.repeat(story.length)}"\n`)]);
    assert(storyNovel.files[0].bytes.length<16*1024*1024);await ca.importData([storyNovel]);const storyId=(await ca.json('/api/novels?limit=1000')).body.data.find(row=>row.toc_url===storyNovel.toc_url).id;
    const storySession=await ca.beginExport([storyId]);await storySession.until(value=>value.state==='selecting');const storyObserved=(await ca.json(`/_fixture/transfer-observe?session=${storySession.status.session_id}`)).body;
    assert(storyObserved.frozen.fragments>90);assert(storyObserved.frozen.max_row_bytes<=32768);assert(storyObserved.stages.every(stage=>stage.file_id===null),'metadata-only initializer cannot snapshot payload files');
    const storyCatalog=await storySession.pages('catalog');await storySession.op('select',{profile:'runtime',extra_roles:[],selection_digest:selectionDigest(storySession.status.catalog_revision,'runtime',[],storyCatalog)});await storySession.until(value=>['ready','reading'].includes(value.state));const storyExport=await storySession.download();assert.equal(storyExport.manifest.novels[0].subtitle,story);
    const storyMemory=(await ca.json('/_fixture/transfer-memory')).body;assert.equal(storyMemory.instances,1);assert(storyMemory.highWater<112*1024*1024,JSON.stringify(storyMemory));
    console.log('PASS measured local workerd memory: 12MiB YAML / 3MiB scalar / 18MiB escaped JSON uses <=32KiB D1 rows and exact pull cursors');

    const gateNovels=Array.from({length:33},(_,index)=>fixtureNovel(`gate-${index}`));await ca.importData(gateNovels);const gateListed=(await ca.json('/api/novels?limit=1000')).body.data;const gateIds=gateNovels.map(novel=>gateListed.find(row=>row.toc_url===novel.toc_url).id);const gates=await ca.beginExport(gateIds);
    let creationOp,selectionInterrupted=false;for(let index=0;index<1000;index++){
      creationOp={method:'POST',headers:gates.headers({'content-type':'application/json','x-narou-test-transfer-fault':'backend_after'}),body:JSON.stringify({operation_id:gates.status.operation_id})};const response=await ca.request(`${gates.path}/advance`,creationOp);const text=await response.text();if(response.headers.get('x-test-transfer-fault-fired')==='true'){assert([500,503].includes(response.status),text);selectionInterrupted=true;break;}assert.equal(response.status,200,text);gates.status=JSON.parse(text);
    }assert(selectionInterrupted,'the actual selected-source cursor commit must be interrupted');const selectionBefore=(await ca.json(`/_fixture/transfer-observe?session=${gates.status.session_id}`)).body.selection;
    const selectionReplay=await ca.json(`${gates.path}/advance`,{...creationOp,headers:gates.headers({'content-type':'application/json'})});assert.equal(selectionReplay.response.status,200,JSON.stringify(selectionReplay.body));gates.status=selectionReplay.body;assert.deepEqual((await ca.json(`/_fixture/transfer-observe?session=${gates.status.session_id}`)).body.selection,selectionBefore,'old opcode must not freeze the next selected source');
    await gates.until(value=>value.state==='selecting');const gateCatalog=await gates.pages('catalog');await gates.op('select',{profile:'runtime',extra_roles:[],selection_digest:selectionDigest(gates.status.catalog_revision,'runtime',[],gateCatalog)});
    let gateIntent;for(let index=0;index<1000;index++){await gates.get();gateIntent={method:'POST',headers:gates.headers({'content-type':'application/json','x-narou-test-transfer-fault':'backend_after'}),body:JSON.stringify({operation_id:gates.status.operation_id})};const response=await ca.request(`${gates.path}/advance`,gateIntent);const text=await response.text();if(response.headers.get('x-test-transfer-fault-fired')==='true'){assert([500,503].includes(response.status),text);break;}assert.equal(response.status,200,text);gates.status=JSON.parse(text);gateIntent=null;}assert(gateIntent,'real gate-admission commit must be interrupted');
    const beforeGateReplay=(await ca.json(`/_fixture/transfer-observe?session=${gates.status.session_id}`)).body.snapshot;assert.equal(beforeGateReplay.gates,16);
    const replayGate=await ca.json(`${gates.path}/advance`,{...gateIntent,headers:gates.headers({'content-type':'application/json'})});assert.equal(replayGate.response.status,200,JSON.stringify(replayGate.body));gates.status=replayGate.body;assert.equal((await ca.json(`/_fixture/transfer-observe?session=${gates.status.session_id}`)).body.snapshot.gates,beforeGateReplay.gates,'old opcode must not close the next gate page');
    let partialSnapshot=false;for(let index=0;index<10000;index++){await gates.get();if(['ready','reading'].includes(gates.status.state))break;const observed=(await ca.json(`/_fixture/transfer-observe?session=${gates.status.session_id}`)).body.snapshot;if(observed.pinned>0&&observed.pinned<33){assert.equal(observed.gates,33,'all source gates remain closed until every selected file is pinned');partialSnapshot=true;}await gates.step();}assert(partialSnapshot);assert.equal((await ca.json(`/_fixture/transfer-observe?session=${gates.status.session_id}`)).body.snapshot.gates,0);assert.equal((await gates.download()).manifest.novels.length,33);
    console.log('PASS private cursor ACK replay, decimal ordinal selection digest and full-selection gate retention until all pins exist');
    await sourceNamespaceContracts(ca,'local-source-overlap');console.log('PASS legacy parent/child sources fail before snapshot/body; lexical siblings and metadata lists remain valid');
    const staleSnapshot=await ca.beginExport([id]);await staleSnapshot.until(v=>v.state==='selecting');const staleCatalog=await staleSnapshot.pages('catalog');await staleSnapshot.op('select',{profile:'runtime',extra_roles:[],selection_digest:selectionDigest(staleSnapshot.status.catalog_revision,'runtime',[],staleCatalog)});
    let snapshotFault=false;for(let i=0;i<2000;i++){
      await staleSnapshot.get();const response=await ca.request(`${staleSnapshot.path}/advance`,{method:'POST',headers:staleSnapshot.headers({'content-type':'application/json','x-narou-test-transfer-fault':'snapshot_epoch_after_read'}),body:JSON.stringify({operation_id:staleSnapshot.status.operation_id})});const text=await response.text();
      if(response.headers.get('x-test-transfer-fault-fired')==='true'){snapshotFault=true;break;}
      assert.equal(response.status,200,text);staleSnapshot.status=JSON.parse(text);
    }assert(snapshotFault,'the real snapshot release must race a later epoch of the same owner');
    const stillOwned=(await ca.json(`/_fixture/transfer-observe?session=${staleSnapshot.status.session_id}`)).body;assert.equal(stillOwned.snapshot.gates,1,'an old snapshot must never re-stamp its token and open the later gate');
    console.log('PASS stale snapshot release preserves the later same-owner gate instead of recapturing its epoch');
    assert(ca.requests.every(r=>r.type!=='application/zip'));assert(ca.requests.filter(r=>r.type==='application/octet-stream').every(r=>r.bytes<=WIRE));
  }finally{for(const child of children)await stop(child);rmSync(work,{recursive:true,force:true,maxRetries:20,retryDelay:100});}
}
if(process.argv[1]&&import.meta.url===pathToFileURL(process.argv[1]).href){await contracts();}
