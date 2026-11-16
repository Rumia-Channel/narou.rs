//! Indexed, primary-bound export selection and frozen metadata. No D1 row
//! contains a whole selection, YAML tree, or whole escaped novel metadata.
use std::collections::HashSet;
use narou_rs::{application::transfer::*,application::novel_access::NovelReadScope,platform::{NovelId,NovelRepository,ObjectPrefix},transfer::ArchiveNovel,virtual_novel};
use serde::{Deserialize,Serialize};
use worker::D1PreparedStatement;
use super::backend::{WorkerTransferBackend,error,invalid,statement,run,keys};

const SOURCE_PAGE:usize=16;
const TEXT_BYTES:usize=32*1024;
const BLOCK_BYTES:usize=256*1024;
// Reserve 2 MiB for bounded D1 rows, parser records and spool buffers.
const RETAINED_BYTES:usize=MAX_METADATA_BYTES-2*1024*1024;
#[derive(Serialize,Deserialize)]
struct SelectionState{request_cursor:usize,member_cursor:usize,normal_count:u32,parent_after:i64,phase:u8}
#[derive(Deserialize)]
pub(super) struct FrozenSource{pub novel_id:i64,pub kind:u8,pub record_revision:String,pub content_revision:String,pub membership_digest:Option<String>,pub member_count:usize}
#[derive(Deserialize)]
struct ParentRow{novel_id:i64,source_order:i64}
#[derive(Deserialize)]
struct MemberRow{position:usize,toc_url:String,record_revision:String,live_revision:String}
#[derive(Default,Serialize,Deserialize)]
struct MetadataCursor{phase:u8,field:u32,byte:usize,part:u32,member:usize,retained:usize,serialized:u64}
#[derive(Serialize,Deserialize)]
struct Layout{novel_type:u8,optional:u16,tags:usize,members:Option<usize>}
#[derive(Deserialize)]
struct MetadataRow{entry_json:String,layout_json:String,cursor_json:String,metadata_ready:i64}
#[derive(Serialize)]
struct Text<'a>{field:u32,part:u32,last:bool,bytes:u64,offset:usize,data:&'a str}
#[derive(Deserialize)]
struct FragmentRow{field:u32,part:u32,byte_offset:usize,data:String,is_final:i64,total_bytes:u64}

fn stale(message:impl Into<String>)->TransferError{TransferError::PlanStale{message:message.into()}}
fn field(n:&ArchiveNovel,index:u32)->Option<&str>{match index{0=>Some(&n.toc_url),1=>Some(&n.title),2=>Some(&n.author),3=>Some(&n.sitename),4=>n.domain.as_deref(),5=>n.attribute_tag.as_deref(),6=>n.ncode.as_deref(),7=>n.subtitle.as_deref(),8=>n.general_lastup.as_deref(),9=>n.last_update.as_deref(),_=>n.tags.get((index-10)as usize).map(String::as_str)}}
fn fragment(field:u32,part:u32,text:&str,byte:usize)->Result<(Text<'_>,usize),TransferError>{
    if byte>text.len()||!text.is_char_boundary(byte){return Err(invalid("metadata_corrupt","frozen text byte cursor is invalid"));}
    let mut end=(byte+TEXT_BYTES).min(text.len());while !text.is_char_boundary(end){end-=1;}
    Ok((Text{field,part,last:end==text.len(),bytes:text.len()as u64,offset:byte,data:&text[byte..end]},end))
}

struct Spool<'a>{backend:&'a WorkerTransferBackend,session:&'a SessionId,ordinal:u32,cursor:MetadataCursor,story:bool,other_retained:u64,other_serialized:u64}
impl<'a> Spool<'a>{
    async fn new(backend:&'a WorkerTransferBackend,session:&'a SessionId,ordinal:u32,cursor:MetadataCursor)->Result<Self,TransferError>{
        #[derive(Deserialize)]struct Budget{retained:u64,serialized:u64}
        let used=statement(&backend.db,"SELECT COALESCE(SUM(metadata_bytes),0) retained,COALESCE(SUM(serialized_bytes),0) serialized FROM transfer_inventory_metadata WHERE session_id=? AND novel_ordinal<>?",&[session.0.as_str().into(),(ordinal as f64).into()])?.first::<Budget>(None).await.map_err(error)?.ok_or_else(||error("metadata admission row missing"))?;
        Ok(Self{backend,session,ordinal,cursor,story:false,other_retained:used.retained,other_serialized:used.serialized})
    }
    async fn push(&mut self,text:&Text<'_>)->Result<usize,TransferError>{
        struct Count(usize);impl std::io::Write for Count{fn write(&mut self,bytes:&[u8])->std::io::Result<usize>{self.0+=bytes.len();Ok(bytes.len())}fn flush(&mut self)->std::io::Result<()>{Ok(())}}
        let mut line=Count(0);serde_json::to_writer(&mut line,text).map_err(error)?;
        if line.0+1>BLOCK_BYTES{return Err(invalid("metadata_budget","frozen text record exceeds 256 KiB"));}
        let retained=if text.part==0{self.cursor.retained.checked_add(text.bytes.try_into().map_err(|_|invalid("metadata_budget","field size overflow"))?).filter(|n|*n<=RETAINED_BYTES&&self.other_retained.checked_add(*n as u64).is_some_and(|total|total<=MAX_METADATA_BYTES as u64)).ok_or_else(||invalid("metadata_budget","frozen metadata exceeds retained budget"))?}else{self.cursor.retained};
        let serialized=self.cursor.serialized.checked_add(line.0 as u64+1).filter(|n|self.other_serialized.checked_add(*n).is_some_and(|total|total<=MAX_WIRE_MANIFEST_BYTES)).ok_or_else(||invalid("metadata_budget","frozen metadata exceeds serialized budget"))?;
        self.backend.journal.ensure_execution(self.session).await?;
        let values=[self.session.0.as_str().into(),(self.ordinal as f64).into(),(text.field as f64).into(),(text.part as f64).into(),(text.offset as f64).into(),text.data.into(),(text.last as i32).into(),(text.bytes as f64).into()];
        let insert=statement(&self.backend.db,"INSERT OR IGNORE INTO transfer_inventory_fragments(session_id,novel_ordinal,field,part,byte_offset,data,is_final,total_bytes) VALUES(?,?,?,?,?,?,?,?)",&values)?;
        let check=statement(&self.backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM transfer_inventory_fragments WHERE session_id=? AND novel_ordinal=? AND field=? AND part=? AND byte_offset=? AND data=? AND is_final=? AND total_bytes=?)",&values)?;
        let result=self.backend.db.batch(vec![self.backend.journal.execution_guard(self.session)?,insert,check]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
        self.cursor.retained=retained;self.cursor.serialized=serialized;Ok(line.0+1)
    }
    async fn checkpoint(self,ready:bool)->Result<(),TransferError>{
        self.backend.journal.ensure_execution(self.session).await?;
        let cursor=serde_json::to_string(&self.cursor).map_err(error)?;
        let update=statement(&self.backend.db,"UPDATE transfer_inventory_metadata SET cursor_json=?,metadata_ready=?,metadata_bytes=?,serialized_bytes=?,layout_json=CASE WHEN ? THEN json_set(layout_json,'$.optional',CAST(json_extract(layout_json,'$.optional') AS INTEGER)|128) ELSE layout_json END WHERE session_id=? AND novel_ordinal=?",&[cursor.into(),(ready as i32).into(),(self.cursor.retained as f64).into(),(self.cursor.serialized as f64).into(),(self.story as i32).into(),self.session.0.as_str().into(),(self.ordinal as f64).into()])?;
        let admission=statement(&self.backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE (SELECT COALESCE(SUM(metadata_bytes),0) FROM transfer_inventory_metadata WHERE session_id=?)>25165824 OR (SELECT COALESCE(SUM(serialized_bytes),0) FROM transfer_inventory_metadata WHERE session_id=?)>67108864",&[self.session.0.as_str().into(),self.session.0.as_str().into()])?;
        let result=self.backend.db.batch(vec![self.backend.journal.execution_guard(self.session)?,update,admission,self.backend.finish_backend_step(self.session,&format!("metadata:{}",self.ordinal),"false")?]).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&result).map_err(error)
    }
}

impl WorkerTransferBackend{
    pub(super) async fn member_ids(&self,id:i64)->Result<Vec<i64>,TransferError>{
        let path=format!("$.\"{id}\"");
        let raw=statement(&self.db,"SELECT json_extract(value_json,?) members FROM app_state WHERE scope='inv' AND key='virtual_novel_members'",&[path.into()])?.first::<String>(Some("members")).await.map_err(error)?;
        let Some(raw)=raw else{return Ok(Vec::new())};
        if raw.len()>256*1024{return Err(invalid("novel_limit","parent membership exceeds bounded metadata storage"));}
        let ids:Vec<i64>=serde_json::from_str(&raw).map_err(error)?;
        if ids.len()>10_000{return Err(invalid("novel_limit","parent membership exceeds 10000 novels"));}
        virtual_novel::validate_members(&ids).map_err(|e|invalid("invalid_member",e))?;Ok(ids)
    }
    async fn frozen_source_id(&self,session:&SessionId,id:i64)->Result<Option<FrozenSource>,TransferError>{statement(&self.db,"SELECT novel_id,kind,record_revision,content_revision,membership_digest,member_count FROM transfer_export_sources WHERE session_id=? AND novel_id=?",&[session.0.as_str().into(),(id as f64).into()])?.first(None).await.map_err(error)}
    pub(super) async fn frozen_source(&self,session:&SessionId,ordinal:u32)->Result<Option<FrozenSource>,TransferError>{statement(&self.db,"SELECT novel_id,kind,record_revision,content_revision,membership_digest,member_count FROM transfer_export_sources WHERE session_id=? AND novel_ordinal=?",&[session.0.as_str().into(),(ordinal as f64).into()])?.first(None).await.map_err(error)}
    async fn verify_source(&self,source:&FrozenSource)->Result<(),TransferError>{
        if self.revision(source.novel_id).await?!=(source.record_revision.clone(),source.content_revision.clone()){return Err(stale("selected source changed while freezing metadata"));}
        if let Some(expected)=source.membership_digest.as_deref(){if self.membership(source.novel_id).await?!=expected{return Err(stale("selected parent membership changed while freezing metadata"));}}Ok(())
    }
    async fn add_normal(&self,session:&SessionId,id:i64,next:&mut u32,seen:&mut HashSet<i64>,statements:&mut Vec<D1PreparedStatement>)->Result<(String,String),TransferError>{
        let (revision,content)=self.revision(id).await?;
        let record=self.repository.get(NovelId(id)).await.map_err(error)?.ok_or_else(||invalid("missing_member","selected member is missing"))?;
        if virtual_novel::is_virtual_record(&record){return Err(invalid("invalid_member","selected parent member is itself virtual"));}
        if record.toc_url.len()>1800{return Err(invalid("backend_metadata_limit","source URL exceeds D1 indexed metadata limit"));}
        if let Some(source)=self.frozen_source_id(session,id).await?{if source.kind!=0||source.record_revision!=revision||source.content_revision!=content{return Err(stale("shared selected member changed during selection"));}}
        else if seen.insert(id){
            statements.push(statement(&self.db,"INSERT INTO transfer_export_sources(session_id,novel_id,kind,source_order,novel_ordinal,record_revision,content_revision) VALUES(?,?,0,?,?,?,?)",&[session.0.as_str().into(),(id as f64).into(),(*next as f64).into(),(*next as f64).into(),revision.as_str().into(),content.into()])?);
            *next=next.checked_add(1).ok_or_else(||invalid("novel_limit","selection count overflow"))?;
        }
        if self.revision(id).await?.0!=revision{return Err(stale("member URL changed while freezing selection"));}
        Ok((record.toc_url,revision))
    }
    pub(super) async fn freeze_selection(&self,session:&SessionId,ids:&[i64])->Result<bool,TransferError>{
        self.journal.ensure_execution(session).await?;
        run(&self.db,"INSERT OR IGNORE INTO transfer_export_selection_state(session_id) VALUES(?)",&[session.0.as_str().into()]).await?;
        let state=statement(&self.db,"SELECT * FROM transfer_export_selection_state WHERE session_id=?",&[session.0.as_str().into()])?.first::<SelectionState>(None).await.map_err(error)?.ok_or_else(||error("selection state missing"))?;
        let step=self.begin_backend_step(session,"freeze_selection",&serde_json::to_string(&state).map_err(error)?).await?;
        if step.result_json.is_some(){return Ok(false);}
        if step.input_json!=serde_json::to_string(&state).map_err(error)?{return Err(TransferError::RecoveryRequired{message:"selection cursor advanced without its operation receipt".into()});}
        if state.phase==2{return Ok(true);}
        let mut statements=vec![self.journal.execution_guard(session)?];let mut next=state.normal_count;
        let (request_cursor,member_cursor,parent_after,phase)=if state.phase==1{
            let parents=statement(&self.db,"SELECT novel_id,source_order FROM transfer_export_sources WHERE session_id=? AND kind=1 AND source_order>? ORDER BY source_order LIMIT 17",&[session.0.as_str().into(),(state.parent_after as f64).into()])?.all().await.map_err(error)?.results::<ParentRow>().map_err(error)?;
            let more=parents.len()>SOURCE_PAGE;let mut after=state.parent_after;
            for parent in parents.into_iter().take(SOURCE_PAGE){statements.push(statement(&self.db,"UPDATE transfer_export_sources SET novel_ordinal=? WHERE session_id=? AND novel_id=? AND novel_ordinal IS NULL",&[(next as f64).into(),session.0.as_str().into(),(parent.novel_id as f64).into()])?);next+=1;after=parent.source_order;}
            (state.request_cursor,0,after,if more{1}else{2})
        }else if let Some(id)=ids.get(state.request_cursor){
            let record=self.repository.get(NovelId(*id)).await.map_err(error)?.ok_or_else(||invalid("source_missing","selected novel is missing"))?;
            let mut seen=HashSet::new();
            if virtual_novel::is_virtual_record(&record){
                if !virtual_novel::is_portable_toc_url(&record.toc_url){self.portable_url(&record).await?;}
                let members=self.member_ids(*id).await?;if members.is_empty(){return Err(invalid("missing_member","selected parent has no members"));}
                let names=members.iter().map(i64::to_string).collect::<Vec<_>>();let digest=narou_rs::platform::membership_digest(names.iter().map(String::as_str));
                let (revision,content)=self.revision(*id).await?;
                if let Some(source)=self.frozen_source_id(session,*id).await?{if source.membership_digest.as_deref()!=Some(digest.as_str())||source.record_revision!=revision||source.content_revision!=content{return Err(stale("parent changed while freezing selection"));}}
                else{statements.push(statement(&self.db,"INSERT INTO transfer_export_sources(session_id,novel_id,kind,source_order,record_revision,content_revision,membership_digest,member_count) VALUES(?,?,1,?,?,?,?,?)",&[session.0.as_str().into(),(*id as f64).into(),(state.request_cursor as f64).into(),revision.into(),content.into(),digest.as_str().into(),(members.len()as f64).into()])?);}
                let end=(state.member_cursor+SOURCE_PAGE).min(members.len());
                for (position,member) in members.iter().enumerate().take(end).skip(state.member_cursor){let(url,revision)=self.add_normal(session,*member,&mut next,&mut seen,&mut statements).await?;
                    statements.push(statement(&self.db,"INSERT OR IGNORE INTO transfer_export_members(session_id,parent_id,position,member_id,toc_url,record_revision) VALUES(?,?,?,?,?,?)",&[session.0.as_str().into(),(*id as f64).into(),(position as f64).into(),(*member as f64).into(),url.into(),revision.into()])?);}
                if self.membership(*id).await?!=digest{return Err(stale("membership changed while freezing member URLs"));}
                if end==members.len(){(state.request_cursor+1,0,state.parent_after,0)}else{(state.request_cursor,end,state.parent_after,0)}
            }else{self.add_normal(session,*id,&mut next,&mut seen,&mut statements).await?;(state.request_cursor+1,0,state.parent_after,0)}
        }else{(state.request_cursor,0,state.parent_after,1)};
        statements.push(statement(&self.db,"UPDATE transfer_export_selection_state SET request_cursor=?,member_cursor=?,normal_count=?,parent_after=?,phase=? WHERE session_id=? AND request_cursor=? AND member_cursor=? AND phase=?",&[(request_cursor as f64).into(),(member_cursor as f64).into(),(next as f64).into(),(parent_after as f64).into(),(phase as f64).into(),session.0.as_str().into(),(state.request_cursor as f64).into(),(state.member_cursor as f64).into(),(state.phase as f64).into()])?);
        statements.push(self.db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE changes()=0"));
        statements.push(statement(&self.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE (SELECT COUNT(*) FROM transfer_export_sources WHERE session_id=?)>10000",&[session.0.as_str().into()])?);
        statements.push(self.finish_backend_step(session,"freeze_selection","false")?);
        let result=self.db.batch(statements).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
        Ok(false)
    }
    async fn metadata_row(&self,session:&SessionId,ordinal:u32)->Result<Option<MetadataRow>,TransferError>{statement(&self.db,"SELECT entry_json,layout_json,cursor_json,metadata_ready FROM transfer_inventory_metadata WHERE session_id=? AND novel_ordinal=?",&[session.0.as_str().into(),(ordinal as f64).into()])?.first(None).await.map_err(error)}
    async fn members_page(&self,session:&SessionId,parent:i64,after:usize,limit:usize)->Result<Vec<MemberRow>,TransferError>{
        statement(&self.db,"SELECT m.position,m.member_id,m.toc_url,m.record_revision,CAST(COALESCE(r.record_revision,0) AS TEXT) live_revision FROM transfer_export_members m LEFT JOIN novel_revisions r ON r.novel_id=m.member_id LEFT JOIN novels n ON n.id=m.member_id WHERE m.session_id=? AND m.parent_id=? AND m.position>=? AND n.toc_url=m.toc_url ORDER BY m.position LIMIT ?",&[session.0.as_str().into(),(parent as f64).into(),(after as f64).into(),(limit as f64).into()])?.all().await.map_err(error)?.results::<MemberRow>().map_err(error)
    }
    pub(super) async fn frozen_inventory(&self,session:&SessionId,source:&FrozenSource,ordinal:u32,want_metadata:bool)->Result<Option<ExportNovelEntry>,TransferError>{
        self.check_source_namespace(source.novel_id).await?;
        self.verify_source(source).await?;
        let mut row=self.metadata_row(session,ordinal).await?;
        if row.is_none(){
            let (entry,metadata)=self.inventory_metadata(session,source.novel_id,ordinal).await?;
            if entry.record_revision!=source.record_revision||entry.content_revision!=source.content_revision||entry.membership_digest!=source.membership_digest{return Err(stale("source changed before metadata admission"));}
            let optional=(0..10).fold(0u16,|bits,index|bits|if field(&metadata,index).is_some(){1<<index}else{0});
            let layout=Layout{novel_type:metadata.novel_type,optional,tags:metadata.tags.len(),members:(source.kind==1).then_some(source.member_count)};
            let retained=layout.tags.checked_add(layout.members.unwrap_or(0)).and_then(|n|n.checked_mul(std::mem::size_of::<String>())).and_then(|n|n.checked_add(std::mem::size_of::<ArchiveNovel>())).filter(|n|*n<=RETAINED_BYTES).ok_or_else(||invalid("metadata_budget","frozen metadata field count exceeds retained budget"))?;
            let cursor=MetadataCursor{retained,..Default::default()};
            let result=self.db.batch(vec![self.journal.execution_guard(session)?,statement(&self.db,"INSERT OR IGNORE INTO transfer_inventory_metadata(session_id,novel_ordinal,entry_json,layout_json,cursor_json,metadata_bytes) VALUES(?,?,?,?,?,?)",&[session.0.as_str().into(),(ordinal as f64).into(),serde_json::to_string(&entry).map_err(error)?.into(),serde_json::to_string(&layout).map_err(error)?.into(),serde_json::to_string(&cursor).map_err(error)?.into(),(retained as f64).into()])?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
            row=self.metadata_row(session,ordinal).await?;
        }
        let row=row.ok_or_else(||error("frozen metadata row missing"))?;
        let step=self.begin_backend_step(session,&format!("metadata:{ordinal}"),&row.cursor_json).await?;
        if step.result_json.is_some(){return Ok(None);}
        if step.input_json!=row.cursor_json{return Err(TransferError::RecoveryRequired{message:"metadata cursor advanced without its operation receipt".into()});}
        let mut entry:ExportNovelEntry=serde_json::from_str(&row.entry_json).map_err(error)?;
        if row.metadata_ready!=0{if want_metadata{let layout:Layout=serde_json::from_str(&row.layout_json).map_err(error)?;entry.metadata=Some(narou_rs::transfer::NovelJsonLayout{novel_type:layout.novel_type,optional:layout.optional,tags:layout.tags.try_into().map_err(|_|invalid("metadata_budget","tag count overflow"))?,members:layout.members.map(u32::try_from).transpose().map_err(|_|invalid("metadata_budget","member count overflow"))?});}return Ok(Some(entry));}
        let layout:Layout=serde_json::from_str(&row.layout_json).map_err(error)?;
        let mut spool=Spool::new(self,session,ordinal,serde_json::from_str(&row.cursor_json).map_err(error)?).await?;
        if spool.cursor.phase==0{
            if source.kind==0{
                let record=self.resolve_record(&entry).await?;let keys=keys(&record)?;
                let mut scope=NovelReadScope::new(&self.objects,"transfer-metadata-initializer");
                let result=async{let prefix=ObjectPrefix::from(keys.prefix().clone());let mut lease=scope.lease_for(&prefix).await.map_err(error)?;
                    let meta=self.objects.stat(&keys.toc()).await.map_err(error)?;
                    let bytes=if let Some(meta)=meta{super::backend::read_metadata(&mut lease,&keys.toc(),meta.size).await?}else{None};
                    let story=bytes.as_deref().map(narou_rs::downloader::persistence::deserialize_toc_story).transpose().map_err(|e|{let message=e.to_string();if message.contains("transfer_metadata_budget:"){TransferError::ExceedsLimit{code:"metadata_budget",message}}else{invalid("invalid_toc_metadata",message)}})?.flatten();
                    drop(bytes);
                    spool.story=story.is_some();
                    if let Some(story)=story{let mut at=0;let mut part=0;loop{let(text,end)=fragment(7,part,&story,at)?;scope.refresh_all().await.map_err(error)?;spool.push(&text).await?;if text.last{break;}at=end;part+=1;}}
                    self.verify_source(source).await
                }.await;
                let released=scope.release_all().await.map_err(error);result.and(released)?;
            }
            spool.cursor.phase=1;spool.checkpoint(false).await?;
            // One existing YAML metadata initializer only: no file snapshot or
            // second payload operation is added to this invocation.
            return Ok(None);
        }
        if spool.cursor.phase==1{
            let (live,metadata)=self.inventory_metadata(session,source.novel_id,ordinal).await?;
            if live.record_revision!=source.record_revision||live.content_revision!=source.content_revision||metadata.tags.len()!=layout.tags{return Err(stale("source metadata changed during bounded freeze"));}
            let fields=10u32.checked_add(layout.tags.try_into().map_err(|_|invalid("metadata_budget","tag count overflow"))?).ok_or_else(||invalid("metadata_budget","field count overflow"))?;
            let mut records=0;let mut bytes=0usize;
            while spool.cursor.field<fields&&records<100&&bytes<MAX_BINARY_BODY_BYTES-BLOCK_BYTES{
                if spool.cursor.field==7{spool.cursor.field+=1;continue;}
                let Some(value)=field(&metadata,spool.cursor.field)else{spool.cursor.field+=1;continue};
                let(text,end)=fragment(spool.cursor.field,spool.cursor.part,value,spool.cursor.byte)?;bytes+=spool.push(&text).await?;records+=1;
                if text.last{spool.cursor.field+=1;spool.cursor.byte=0;spool.cursor.part=0;}else{spool.cursor.byte=end;spool.cursor.part+=1;}
            }
            if spool.cursor.field==fields{spool.cursor.phase=2;}
            self.verify_source(source).await?;spool.checkpoint(false).await?;return Ok(None);
        }
        if spool.cursor.phase==2{
            if let Some(count)=layout.members{
                let rows=self.members_page(session,source.novel_id,spool.cursor.member,100).await?;
                if rows.is_empty()&&spool.cursor.member<count{return Err(stale("frozen member URL is missing or changed"));}
                for member in rows{if member.position!=spool.cursor.member||member.record_revision!=member.live_revision{return Err(stale("member URL revision changed while freezing metadata"));}
                    let index=10u32+layout.tags as u32+member.position as u32;
                    let(text,_)=fragment(index,0,&member.toc_url,0)?;if !text.last{return Err(invalid("backend_metadata_limit","member URL exceeds indexed metadata bound"));}spool.push(&text).await?;spool.cursor.member+=1;
                }
                if spool.cursor.member<count{self.verify_source(source).await?;spool.checkpoint(false).await?;return Ok(None);}
            }
            self.verify_source(source).await?;spool.cursor.phase=3;spool.checkpoint(true).await?;return Ok(None);
        }
        Err(invalid("metadata_corrupt","frozen metadata has an invalid phase"))
    }
    pub(super) async fn frozen_metadata_page(&self,session:&SessionId,ordinal:u32,cursor:&narou_rs::application::transfer::MetadataCursor,limit:usize)->Result<ExportMetadataPage,TransferError>{
        self.journal.ensure_execution(session).await?;
        let ready=statement(&self.db,"SELECT metadata_ready FROM transfer_inventory_metadata WHERE session_id=? AND novel_ordinal=?",&[session.0.as_str().into(),(ordinal as f64).into()])?.first::<i64>(Some("metadata_ready")).await.map_err(error)?;
        if ready!=Some(1){return Err(invalid("metadata_missing","source metadata is not frozen"));}
        let limit=limit.clamp(1,16);
        let mut rows=statement(&self.db,"SELECT field,part,byte_offset,data,is_final,total_bytes FROM transfer_inventory_fragments WHERE session_id=? AND novel_ordinal=? AND (field,part)>=(?,?) ORDER BY field,part LIMIT ?",&[session.0.as_str().into(),(ordinal as f64).into(),(cursor.field as f64).into(),(cursor.part as f64).into(),((limit+1)as f64).into()])?.all().await.map_err(error)?.results::<FragmentRow>().map_err(error)?;
        let more=rows.len()>limit;rows.truncate(limit);let mut fragments=Vec::with_capacity(rows.len());
        let mut next=narou_rs::application::transfer::MetadataCursor{field:cursor.field,byte:cursor.byte,part:cursor.part};
        for row in rows{
            if row.field<next.field||row.field==next.field&&(row.part!=next.part||row.byte_offset!=next.byte)||row.field>next.field&&(next.part!=0||next.byte!=0){return Err(invalid("metadata_corrupt","frozen metadata cursor is not contiguous"));}
            let last=row.is_final!=0;let end=(row.byte_offset as u64).checked_add(row.data.len()as u64).ok_or_else(||invalid("metadata_corrupt","metadata byte count overflow"))?;
            if row.data.len()>TEXT_BYTES||end>row.total_bytes||last!=(end==row.total_bytes){return Err(invalid("metadata_corrupt","frozen metadata fragment identity differs"));}
            next=if last{narou_rs::application::transfer::MetadataCursor{field:row.field.checked_add(1).ok_or_else(||invalid("metadata_corrupt","field ordinal overflow"))?,byte:0,part:0}}else{narou_rs::application::transfer::MetadataCursor{field:row.field,byte:end as usize,part:row.part.checked_add(1).ok_or_else(||invalid("metadata_corrupt","fragment ordinal overflow"))?}};
            fragments.push(MetadataFragment{field:row.field,part:row.part,byte_offset:row.byte_offset,bytes:row.data.into_bytes(),last});
        }
        if !more&&fragments.last().is_some_and(|fragment|!fragment.last){return Err(invalid("metadata_corrupt","frozen metadata ended inside a field"));}
        Ok(ExportMetadataPage{fragments,next_cursor:more.then_some(next)})
    }
}
