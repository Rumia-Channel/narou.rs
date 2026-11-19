// Test-only fault injection around the production D1 transfer commit batch.
const OriginalInstance=WebAssembly.Instance;
let memory,highWater=0,instances=0;
const sample=()=>{highWater=Math.max(highWater,memory?.buffer.byteLength??0);return{wasmBytes:memory?.buffer.byteLength??0,highWater,instances};};
WebAssembly.Instance=class extends OriginalInstance{constructor(...args){super(...args);sample();memory=this.exports.memory;instances++;}};
const Production=await import('../build/worker/shim.mjs');
const Worker=Production.default;
export const PushHub=Production.PushHub;
export const SiteRateLimiter=Production.SiteRateLimiter;
function instrument(database,mode,state){
  const originals=new WeakMap(),queries=new WeakMap();
  const statement=(target,sql,bound=[])=>{const proxy=new Proxy(target,{get(target,name){
    if(name==='constructor')return target.constructor;
    if(name==='bind')return(...values)=>statement(target.bind(...values),sql,values);
    if(name==='first')return async(...args)=>{
      const row=await target.first(...args);
      if(row&&mode==='snapshot_epoch_after_read'&&!state.fired&&/^SELECT prefix,owner,epoch FROM transfer_snapshot_gates WHERE /.test(sql)){
        const result=await database.withSession('first-primary').prepare('UPDATE novel_access_gates SET epoch=epoch+1 WHERE prefix=? AND owner=? AND epoch=? AND closed=1 AND epoch<9007199254740991').bind(row.prefix,row.owner,row.epoch).run();
        if(result.meta.changes!==1)throw new Error('snapshot epoch fixture lost its exact original gate');
        state.fired=true;
      }
      return row;
    };
    if(name==='all')return async(...args)=>{
      if(mode==='member_before_read'&&!state.fired&&/FROM json_each\(\?\) j LEFT JOIN novels n ON n.id=j.value/.test(sql)){
        const id=JSON.parse(bound[0]).at(-1);if(!Number.isSafeInteger(id))throw new Error('member fixture did not receive an exact novel id');
        const primary=database.withSession('first-primary');
        if(!await primary.prepare('SELECT id FROM novels WHERE id=?').bind(id).first())throw new Error('member fixture was absent before its injected deletion');
        await primary.prepare('DELETE FROM novels WHERE id=?').bind(id).run();
        if(await primary.prepare('SELECT id FROM novels WHERE id=?').bind(id).first())throw new Error('member fixture did not remove its exact novel');
        state.fired=true;
      }
      return target.all(...args);
    };
    const value=Reflect.get(target,name,target);return typeof value==='function'?value.bind(target):value;
  }});originals.set(proxy,target);queries.set(proxy,sql);return proxy;};
  const wrap=target=>new Proxy(target,{get(target,name){
    if(name==='constructor')return target.constructor;
    if(name==='prepare')return sql=>statement(target.prepare(sql),sql);
    if(name==='withSession')return(...args)=>{state.sessions.push(args[0]);if(state.primaryOnly&&args[0]==='first-unconstrained')throw new Error('replica session forbidden for transfer v2');return wrap(target.withSession(...args));};
    if(name==='batch')return async items=>{const texts=items.map(item=>queries.get(item)??'');const commit=texts.some(sql=>/UPDATE transfer_targets SET committed=1/.test(sql));const privateStep=texts.some(sql=>/UPDATE transfer_(export_selection_state|snapshot_state) SET/.test(sql));const initialized=texts.some(sql=>/UPDATE transfer_stage_owners SET [^;]*initialized=1/.test(sql));
      if(commit&&mode==='commit_before'){state.fired=true;throw new Error('fixture unavailable before transfer commit');}
      const result=await target.batch(items.map(item=>originals.get(item)??item));
      if(commit&&mode==='commit_after'&&!state.fired){state.fired=true;throw new Error('fixture lost D1 commit acknowledgement');}
      if(privateStep&&mode==='backend_after'&&!state.fired){state.fired=true;throw new Error('fixture lost private cursor commit acknowledgement');}
      if(initialized&&mode==='initializer_after'&&!state.fired){state.fired=true;throw new Error('fixture lost initializer EOF checkpoint acknowledgement');}
      return result;
    };
    const value=Reflect.get(target,name,target);return typeof value==='function'?value.bind(target):value;
  }});return wrap(database);
}
export default class TransferFailureWorker extends Worker{
  async fetch(request){
    const response=await this.dispatch(request);
    // Mirrors ci/transfer_trace.mjs DISPATCH_HEADER ('x-test-transfer-dispatched').
    // The client uses it to prove a response came from this Worker, not an
    // edge-generated error page (e.g. a route not propagated at this colo).
    try{response.headers.set('x-test-transfer-dispatched','1');return response;}catch{try{const result=new Response(response.body,response);result.headers.set('x-test-transfer-dispatched','1');return result;}catch{return response;}}
  }
  async dispatch(request){
    const url=new URL(request.url);
    if(url.pathname==='/_fixture/transfer-memory'){if(request.headers.get('authorization')!==`Bearer ${this.env.NAROU_ADMIN_TOKEN}`)return new Response('unauthorized',{status:401});return Response.json(sample());}
    if(url.pathname==='/_fixture/transfer-namespace'){
      if(request.headers.get('authorization')!==`Bearer ${this.env.NAROU_ADMIN_TOKEN}`)return new Response('unauthorized',{status:401});
      if(request.method!=='POST')return new Response('method not allowed',{status:405});
      const text=await request.text();if(text.length>32768)return new Response('fixture too large',{status:413});const body=JSON.parse(text);
      if(!Number.isSafeInteger(body.id)||!['sibling','child'].includes(body.mode)||!/^https:\/\/example\.test\/[a-zA-Z0-9_/-]+\/$/.test(body.toc_url)||!Array.isArray(body.files)||body.files.length!==2)return new Response('invalid namespace fixture',{status:400});
      const files=body.files.map(file=>({...file,bytes:Uint8Array.from(atob(file.data),point=>point.charCodeAt(0))}));
      if(!files.some(file=>file.path==='toc.yaml')||files.some(file=>!['toc.yaml','secret.bin'].includes(file.path)||file.bytes.length>4096||!Number.isSafeInteger(file.crc32)||file.crc32<0||file.crc32>0xffffffff))return new Response('invalid namespace fixture files',{status:400});
      const db=this.env.DB.withSession('first-primary');const parent=await db.prepare('SELECT n.sitename,n.sitename_fold,n.file_title,n.use_subdirectory,b.prefix,g.epoch FROM novels n JOIN novel_gate_bindings b ON b.novel_id=n.id JOIN novel_access_gates g ON g.prefix=b.prefix WHERE n.id=? AND g.closed=0').bind(body.id).first();
      if(!parent||parent.file_title!=='12'||parent.use_subdirectory!==0)return new Response('flat grouping parent fixture missing',{status:409});
      const next=await db.prepare('SELECT MAX(COALESCE((SELECT MAX(id) FROM novels),-1),COALESCE((SELECT MAX(id) FROM novel_id_reservations),-1),COALESCE((SELECT next_id FROM novel_id_sequence WHERE id=1),0))+1 id').first();const id=next.id;if(!Number.isSafeInteger(id))throw new Error('fixture id overflow');
      const child=body.mode==='child',fileTitle=child?'n1234':'12x',prefix=child?`${parent.prefix}/n1234`:`${parent.prefix}x`,title=`Legacy namespace ${body.mode}`,folded=body.toc_url.toLowerCase();
      // Real legacy canonical rows deliberately bypass NEW-creation admission;
      // export must detect this pre-existing ambiguous hierarchy itself.
      const statements=[db.prepare('INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novels n JOIN novel_gate_bindings b ON b.novel_id=n.id JOIN novel_access_gates g ON g.prefix=b.prefix WHERE n.id=? AND b.prefix=? AND g.epoch=? AND g.closed=0)').bind(body.id,parent.prefix,parent.epoch),
        db.prepare('INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update,novel_type,ncode,use_subdirectory,domain) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)').bind(id,'Fixture','fixture',title,title.toLowerCase(),fileTitle,body.toc_url,folded,parent.sitename,parent.sitename_fold,new Date().toISOString(),1,fileTitle,Number(child),'example.test'),
        db.prepare('UPDATE novel_id_sequence SET next_id=MAX(next_id,?) WHERE id=1').bind(id+1),
        db.prepare('INSERT INTO novel_gate_bindings(novel_id,prefix) VALUES(?,?)').bind(id,prefix),
        db.prepare('INSERT INTO novel_access_gates(prefix,updated_at,origin_url_fold) VALUES(?,unixepoch(),?)').bind(prefix,folded),
        ...files.map(file=>db.prepare("INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES(?,?,?,'none',?,?)").bind(`${prefix}/${file.path}`,file.bytes.length,new Date().toISOString(),file.crc32,file.bytes.buffer))];
      await db.batch(statements);return Response.json({id,prefix});
    }
    if(url.pathname==='/_fixture/transfer-legacy'){
      if(request.headers.get('authorization')!==`Bearer ${this.env.NAROU_ADMIN_TOKEN}`)return new Response('unauthorized',{status:401});
      if(request.method!=='POST')return new Response('method not allowed',{status:405});
      const text=await request.text();if(text.length>256*1024)return new Response('fixture payload too large',{status:413});
      const body=JSON.parse(text);if(!Number.isSafeInteger(body.id)||!Number.isSafeInteger(body.size)||body.size<0||body.size>16*1024*1024||!Number.isSafeInteger(body.crc32)||body.crc32<0||body.crc32>0xffffffff||! /^[a-zA-Z0-9_.-]{1,64}$/.test(body.path))return new Response('invalid legacy fixture',{status:400});
      const compressed=Uint8Array.from(atob(body.brotli),point=>point.charCodeAt(0));if(compressed.length>64*1024)return new Response('compressed fixture too large',{status:413});
      const db=this.env.DB.withSession('first-primary');
      const row=await db.prepare("SELECT r.object_key,b.prefix FROM novel_gate_bindings b JOIN novel_access_gates g ON g.prefix=b.prefix AND g.closed=0 JOIN transfer_object_refs r ON r.object_key=b.prefix||'/'||? WHERE b.novel_id=?").bind(body.path,body.id).first();
      if(!row)return new Response('canonical fixture file missing or recovering',{status:409});
      // This deliberately seeds the real pre-v2 compressed representation, not
      // a fake backend. The fixture is isolated and owns this logical key.
      await db.batch([
        db.prepare('INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novel_access_gates g ON g.prefix=b.prefix WHERE b.novel_id=? AND b.prefix=? AND g.closed=0)').bind(body.id,row.prefix),
        db.prepare('DELETE FROM transfer_object_refs WHERE object_key=?').bind(row.object_key),
        db.prepare('DELETE FROM object_chunks WHERE object_key=?').bind(row.object_key),
        db.prepare("INSERT INTO objects(object_key,size,updated_at,content_type,encoding,crc32,data) VALUES(?,?,?,'application/octet-stream','brotli',?,?) ON CONFLICT(object_key) DO UPDATE SET size=excluded.size,updated_at=excluded.updated_at,encoding=excluded.encoding,crc32=excluded.crc32,data=excluded.data").bind(row.object_key,body.size,new Date().toISOString(),body.crc32,compressed.buffer),
      ]);
      return Response.json({seeded:true});
    }
    if(url.pathname==='/_fixture/transfer-observe'){
      if(request.headers.get('authorization')!==`Bearer ${this.env.NAROU_ADMIN_TOKEN}`)return new Response('unauthorized',{status:401});
      const id=url.searchParams.get('session');const db=this.env.DB.withSession('first-primary');
      // A read-only batch keeps the observations in one primary snapshot
      // and avoids seven sequential D1 round trips for every gate check.
      const [publications,targets,stages,refs,snapshot,frozen,selection]=await db.batch([
        db.prepare('SELECT publication_id,state,assigned_id,receipt,error FROM transfer_publications WHERE session_id=? ORDER BY publication_id').bind(id),
        db.prepare('SELECT publication_id,assigned_id,prefix,epoch,committed,rolled_back FROM transfer_targets WHERE session_id=? ORDER BY publication_id').bind(id),
        db.prepare('SELECT stage_id,file_id,expected_size,backend,initialized,purpose AS stagepurpose,sealed_key IS NOT NULL AS sealed FROM transfer_stage_owners WHERE session_id=? ORDER BY file_id').bind(id),
        db.prepare('SELECT object_key,blob_key,size,sha256,backend FROM transfer_object_refs ORDER BY object_key'),
        db.prepare("SELECT (SELECT COUNT(*) FROM transfer_snapshot_gates s JOIN novel_access_gates g ON g.prefix=s.prefix AND g.owner=s.owner AND g.closed=1 WHERE s.session_id=?) gates,(SELECT COUNT(*) FROM transfer_files WHERE session_id=? AND state IN ('staged','pinned')) pinned,(SELECT COUNT(*) FROM transfer_catalog_entries WHERE session_id=? AND selected=1) selected").bind(id,id,id),
        db.prepare('SELECT COALESCE(MAX(length(CAST(data AS BLOB))),0) max_row_bytes,COUNT(*) fragments FROM transfer_inventory_fragments WHERE session_id=?').bind(id),
        db.prepare('SELECT * FROM transfer_export_selection_state WHERE session_id=?').bind(id),
      ]);
      return Response.json({publications:publications.results,targets:targets.results,stages:stages.results,refs:refs.results,snapshot:snapshot.results[0]??null,frozen:frozen.results[0]??null,selection:selection.results[0]??null});
    }
    const mode=request.headers.get('x-narou-test-transfer-fault');
    if(!url.pathname.startsWith('/api/transfer/v2'))return super.fetch(request);
    const state={fired:false,sessions:[],primaryOnly:true};
    const response=await new Worker(this.ctx,{...this.env,DB:instrument(this.env.DB,mode,state)}).fetch(request);
    // Direct D1 binding calls already run on primary; Sessions API is only
    // needed to opt into replicas. Keep rejecting every unconstrained session.
    const result=new Response(response.body,response);result.headers.set('x-test-transfer-fault-fired',String(state.fired));result.headers.set('x-test-transfer-primary-only',String(state.sessions.every(value=>value==='first-primary')));result.headers.set('x-test-transfer-wasm-bytes',String(sample().wasmBytes));return result;
  }
}
