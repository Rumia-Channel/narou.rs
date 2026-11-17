#[derive(Debug,Clone,Serialize,Deserialize)]
struct SavedPublicationStep { done:bool,committed:bool,cursor:Option<String>,processed_bytes:u64,assigned_id:Option<i64>,receipt:Option<Vec<u8>> }
impl From<&PublicationStep> for SavedPublicationStep {fn from(step:&PublicationStep)->Self{Self{done:step.done,committed:step.committed,cursor:step.cursor.clone(),processed_bytes:step.processed_bytes,assigned_id:step.assigned_id,receipt:step.receipt.clone()}}}
impl From<SavedPublicationStep> for PublicationStep {fn from(step:SavedPublicationStep)->Self{Self{done:step.done,committed:step.committed,cursor:step.cursor,processed_bytes:step.processed_bytes,assigned_id:step.assigned_id,receipt:step.receipt}}}
#[derive(Debug,Clone,Default,Serialize,Deserialize)]
struct NativeGcCursor { phase:u8,cursor:Option<String>,inner:Option<String> }

impl NativeTransferBackend {
    fn step_result_key(&self,spec:&PublicationSpec,phase:&str)->TR<String>{
        let operation=self.journal.executing_operation(&spec.session_id)?;
        Ok(format!("step:{}:{}:{phase}",operation.0,spec.publication_id))
    }
    fn saved_step(&self,spec:&PublicationSpec,phase:&str)->TR<Option<PublicationStep>>{
        Ok(self.journal.backend_get::<SavedPublicationStep>(&spec.session_id.0,&self.step_result_key(spec,phase)?)?.map(Into::into))
    }
    fn save_step(&self,spec:&PublicationSpec,phase:&str,publication:&NativePublication,undo:Option<(&str,&FileUndo)>,step:PublicationStep)->TR<PublicationStep>{
        let result_key=self.step_result_key(spec,phase)?;
        self.journal.run_write(|rows|{
            if let Some((key,undo))=undo{rows.put("backend",&spec.session_id.0,key,undo)?;}
            rows.put("backend",&spec.session_id.0,&format!("publication:{}",spec.publication_id),publication)?;
            rows.put("backend",&format!("publication_owners:{}",spec.session_id.0),&publication.owner,publication)?;
            rows.put("backend",&spec.session_id.0,&result_key,&SavedPublicationStep::from(&step))
        })?;Ok(step)
    }
    async fn canonical_identity(&self,key:&ObjectKey)->TR<Option<String>>{
        if let Some(identity)=self.primary.source_generation(key).await.map_err(TransferError::platform)?{return Ok(Some(identity));}
        let Some(metadata)=self.assets.stat(key).await.map_err(TransferError::platform)?else{return Ok(None)};
        let etag=metadata.etag.ok_or_else(||TransferError::RecoveryRequired{message:"canonical object has no stable generation or ETag".into()})?;
        Ok(Some(format!("remote:{}:{etag}",metadata.size)))
    }
    async fn copy_cursor(&self,stage:&str,session:&SessionId)->TR<crate::platform::StageCursor>{
        match self.primary.stage_open(stage,&session.0,1,SESSION_IDLE_TTL_SECS).await{Ok(cursor)=>Ok(cursor),Err(crate::error::NarouError::Platform(message))if message.starts_with("stage_checkpoint_invalid:")=>self.primary.stage_cursor(stage).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("stage_missing","copy stage ledger missing")),Err(error)=>Err(TransferError::platform(error))}
    }
    async fn prepare_publication(&self,spec:&PublicationSpec)->TR<PublicationStep>{
        self.journal.require_authority()?;if let Some(step)=self.saved_step(spec,"prepare")?{return Ok(step);}
        let current=self.publication(spec)?;let mut publication=self.work_input(&spec.session_id,&format!("prepare:{}",spec.publication_id),current)?;
        self.check_plan(spec,&publication).await?;let index=publication.prepared;
        if index>=spec.file_count{return self.save_step(spec,"prepare",&publication,None,PublicationStep{done:true,assigned_id:Some(publication.record_id),..Default::default()});}
        let file=self.publication_file(spec,index).await?;let key=ObjectKey::try_new(format!("{}/{}",publication.prefix.trim_end_matches('/'),file.logical_path)).map_err(TransferError::platform)?;
        let undo_key=format!("undo:{}:{}:{}",spec.publication_id,publication.attempt,number(index));
        let mut undo=if let Some(undo)=self.journal.backend_get::<FileUndo>(&spec.session_id.0,&undo_key)?{undo}else{
            let old=self.assets.stat(&key).await.map_err(TransferError::platform)?;
            let undo=FileUndo{path:file.logical_path.clone(),old_stage:old.as_ref().map(|_|self.opaque_id("undo")),old_size:old.as_ref().map_or(0,|metadata|metadata.size),old_sha256:None,old_generation:self.canonical_identity(&key).await?,copied:0,ready:old.is_none(),installed:false,install_stage:None,install_offset:0,restore_stage:None,restore_offset:0};
            self.journal.backend_put(&spec.session_id.0,&undo_key,&undo)?;undo
        };
        let mut processed=0u64;
        if !undo.ready {
            if self.canonical_identity(&key).await?!=undo.old_generation{return Err(TransferError::PlanStale{message:"canonical preimage changed while preparing undo".into()});}
            let mut handle=undo.old_stage.clone().ok_or_else(||TransferError::invalid("preimage_missing","undo stage owner missing"))?;
            let mut cursor=self.copy_cursor(&handle,&spec.session_id).await?;
            if cursor.sealed&&(cursor.confirmed_bytes!=undo.old_size||cursor.prefix_sha256().is_none()){return Err(TransferError::RecoveryRequired{message:"sealed preimage checkpoint is corrupt".into()});}
            if !cursor.sealed&&cursor.prefix_sha256().is_none(){self.primary.stage_abort(&handle,&spec.session_id.0,1).await.map_err(TransferError::platform)?;undo.old_stage=Some(self.opaque_id("undo"));undo.copied=0;return self.save_step(spec,"prepare",&publication,Some((&undo_key,&undo)),PublicationStep{assigned_id:Some(publication.record_id),..Default::default()});}
            let compressed=self.primary.stored_encoding(&key).await.map_err(TransferError::platform)?.is_some_and(|encoding|encoding!=crate::platform::ObjectEncoding::None);
            if !cursor.sealed&&compressed {
                if undo.old_size>16*1024*1024{return Err(TransferError::invalid("legacy_metadata_limit","compressed preimage exceeds existing16MiB decoded contract"));}
                if cursor.confirmed_bytes!=0 {
                    self.primary.stage_abort(&handle,&spec.session_id.0,1).await.map_err(TransferError::platform)?;
                    handle=self.opaque_id("undo");undo.old_stage=Some(handle.clone());undo.copied=0;self.journal.backend_put(&spec.session_id.0,&undo_key,&undo)?;
                    self.primary.stage_open(&handle,&spec.session_id.0,1,SESSION_IDLE_TTL_SECS).await.map_err(TransferError::platform)?;
                }
                use futures::StreamExt;
                let mut input=self.assets.read_stream(&key).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("preimage_missing","canonical preimage disappeared"))?;let mut offset=0u64;
                while let Some(bytes)=input.next().await{let bytes=bytes.map_err(TransferError::platform)?;if bytes.len()>64*1024{return Err(TransferError::invalid("codec_io_limit","preimage decoder exceeded64KiB"));}let end=offset.checked_add(bytes.len() as u64).filter(|end|*end<=undo.old_size).ok_or_else(||TransferError::invalid("source_size_mismatch","preimage exceeds declared size"))?;self.primary.stage_append(&handle,&spec.session_id.0,1,offset,&bytes,end==undo.old_size).await.map_err(TransferError::platform)?;offset=end;}
                if offset!=undo.old_size{return Err(TransferError::invalid("source_short_read","preimage is truncated"));}
                if offset==0{self.primary.stage_append(&handle,&spec.session_id.0,1,0,&[],true).await.map_err(TransferError::platform)?;}
                processed=offset;cursor=self.primary.stage_cursor(&handle).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("stage_missing","preimage cursor missing"))?;
            }else if !cursor.sealed {
                let offset=self.work_input(&spec.session_id,&format!("prepare_offset:{}:{index}",spec.publication_id),undo.copied)?;
                let end=offset.saturating_add(CHUNK_BYTES).min(undo.old_size);
                if cursor.confirmed_bytes<end {if cursor.confirmed_bytes!=offset{return Err(TransferError::RecoveryRequired{message:"preimage checkpoint is not at planned range".into()});}let bytes=self.read_bytes(&key,offset..end).await?;cursor=self.primary.stage_append(&handle,&spec.session_id.0,1,offset,&bytes,end==undo.old_size).await.map_err(TransferError::platform)?;}
                else if undo.old_size==0&&cursor.confirmed_bytes==0{cursor=self.primary.stage_append(&handle,&spec.session_id.0,1,0,&[],true).await.map_err(TransferError::platform)?;}
                undo.copied=end;processed=end-offset;
            }
            if self.canonical_identity(&key).await?!=undo.old_generation{return Err(TransferError::PlanStale{message:"canonical preimage changed during undo copy".into()});}
            if cursor.confirmed_bytes==undo.old_size {
                let sha=cursor.prefix_sha256().ok_or_else(||TransferError::RecoveryRequired{message:"preimage checkpoint is corrupt".into()})?;
                self.primary.stage_seal(&handle,&spec.session_id.0,1,undo.old_size,&sha).await.map_err(TransferError::platform)?;
                undo.old_sha256=Some(sha);undo.copied=undo.old_size;undo.ready=true;
            }
        }
        if undo.ready{publication.prepared=index+1;}
        self.save_step(spec,"prepare",&publication,Some((&undo_key,&undo)),PublicationStep{done:publication.prepared==spec.file_count,processed_bytes:processed,assigned_id:Some(publication.record_id),..Default::default()})
    }
    async fn install_publication(&self,spec:&PublicationSpec)->TR<PublicationStep>{
        self.journal.require_authority()?;if let Some(step)=self.saved_step(spec,"install")?{return Ok(step);}let current=self.publication(spec)?;let mut publication=self.work_input(&spec.session_id,&format!("install:{}",spec.publication_id),current)?;let index=publication.installed;
        if index>=spec.file_count{return self.save_step(spec,"install",&publication,None,PublicationStep{done:true,..Default::default()});}let file=self.publication_file(spec,index).await?;let undo_key=format!("undo:{}:{}:{}",spec.publication_id,publication.attempt,number(index));let mut undo:FileUndo=self.journal.backend_get(&spec.session_id.0,&undo_key)?.ok_or_else(||TransferError::invalid("preimage_missing","publication preimage missing"))?;
        if !undo.ready{return Err(TransferError::invalid("preimage_incomplete","cannot install before durable undo"));}
        if let Some(old)=&undo.old_stage{let cursor=self.primary.stage_cursor(old).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::RecoveryRequired{message:"immutable preimage missing".into()})?;if !cursor.sealed||cursor.confirmed_bytes!=undo.old_size||cursor.prefix_sha256()!=undo.old_sha256{return Err(TransferError::RecoveryRequired{message:"immutable preimage seal changed".into()});}}
        let target=ObjectKey::try_new(format!("{}/{}",publication.prefix.trim_end_matches('/'),file.logical_path)).map_err(TransferError::platform)?;let installed=self.stores.transfer_asset_fingerprint(&target).await.map_err(TransferError::platform)?.is_some_and(|(size,sha)|size==file.size&&sha==file.sha256);let mut processed=0;
        if !installed{
            self.check_plan(spec,&publication).await?;if self.canonical_identity(&target).await?!=undo.old_generation{return Err(TransferError::RecoveryRequired{message:"destination no longer matches prepared preimage".into()});}
            let source=file.stage.as_deref().or(file.pin.as_deref()).ok_or_else(||TransferError::invalid("stage_missing","verified source missing"))?;let stage=undo.install_stage.clone().unwrap_or_else(||self.opaque_id("install"));undo.install_stage=Some(stage.clone());self.journal.backend_put(&spec.session_id.0,&undo_key,&undo)?;let cursor=self.copy_cursor(&stage,&spec.session_id).await?;
            if !cursor.sealed&&cursor.prefix_sha256().is_none(){self.primary.stage_abort(&stage,&spec.session_id.0,1).await.map_err(TransferError::platform)?;undo.install_stage=None;undo.install_offset=0;return self.save_step(spec,"install",&publication,Some((&undo_key,&undo)),PublicationStep::default());}
            let offset=self.work_input(&spec.session_id,&format!("install_offset:{}:{index}",spec.publication_id),undo.install_offset)?;let end=offset.saturating_add(CHUNK_BYTES).min(file.size);
            if !cursor.sealed&&cursor.confirmed_bytes<end{if cursor.confirmed_bytes!=offset{return Err(TransferError::RecoveryRequired{message:"install cursor differs from planned range".into()});}let bytes=self.stage_read_range(source,offset..end).await?;self.primary.stage_append(&stage,&spec.session_id.0,1,offset,&bytes,end==file.size).await.map_err(TransferError::platform)?;}else if !cursor.sealed&&file.size==0{self.primary.stage_append(&stage,&spec.session_id.0,1,0,&[],true).await.map_err(TransferError::platform)?;}
            undo.install_offset=end;processed=end-offset;if end<file.size{return self.save_step(spec,"install",&publication,Some((&undo_key,&undo)),PublicationStep{processed_bytes:processed,..Default::default()});}let sealed=self.primary.stage_seal(&stage,&spec.session_id.0,1,file.size,&file.sha256).await.map_err(TransferError::platform)?;
            if processed>0{return self.save_step(spec,"install",&publication,Some((&undo_key,&undo)),PublicationStep{processed_bytes:processed,..Default::default()});}
            let prefix=ObjectPrefix::new(&publication.prefix).map_err(TransferError::platform)?;let operation=self.journal.executing_operation(&spec.session_id)?;let(done,bytes)=self.stores.prepare_transfer_asset(&prefix,&publication.owner,publication.epoch,crate::native::object_store::PreparedAsset{stage:&sealed.key,key:&target,sha256:&file.sha256},&operation.0).await.map_err(TransferError::platform)?;processed=bytes;if !done{return self.save_step(spec,"install",&publication,Some((&undo_key,&undo)),PublicationStep{processed_bytes:processed,..Default::default()});}self.stores.commit_transfer_asset(&prefix,&publication.owner,publication.epoch,&sealed.key,&target,&file.sha256).await.map_err(TransferError::platform)?;
        }
        undo.installed=true;publication.installed=index+1;publication.content_revision=self.primary.gate_info(&ObjectPrefix::new(&publication.prefix).map_err(TransferError::platform)?).await.map_err(TransferError::platform)?.content_revision;self.save_step(spec,"install",&publication,Some((&undo_key,&undo)),PublicationStep{done:publication.installed==spec.file_count,processed_bytes:processed,..Default::default()})
    }
    async fn rollback_publication(&self,spec:&PublicationSpec,cursor:Option<&str>)->TR<PublicationStep>{
        self.journal.require_authority()?;if self.committed(spec)?{return self.publication_finish(spec,cursor).await;}
        if let Some(step)=self.saved_step(spec,"rollback")?{return Ok(step);}
        if self.journal.backend_get::<NativePublication>(&spec.session_id.0,&format!("publication:{}",spec.publication_id))?.is_none(){return Ok(PublicationStep{done:true,..Default::default()});}
        let publication=self.publication(spec)?;super::recover_metadata(&self.publication_directory(spec)).map_err(TransferError::platform)?;
        let index=cursor.map(str::parse::<u32>).transpose().map_err(TransferError::platform)?.unwrap_or(0);
        if index>=spec.file_count{return self.save_step(spec,"rollback",&publication,None,PublicationStep{done:true,..Default::default()});}
        let file=self.publication_file(spec,index).await?;let undo_key=format!("undo:{}:{}:{}",spec.publication_id,publication.attempt,number(index));
        let Some(mut undo)=self.journal.backend_get::<FileUndo>(&spec.session_id.0,&undo_key)?else{return self.save_step(spec,"rollback",&publication,None,PublicationStep{done:index+1==spec.file_count,cursor:Some((index+1).to_string()),..Default::default()});};
        let finished=PublicationStep{done:index+1==spec.file_count,cursor:Some((index+1).to_string()),..Default::default()};
        if !undo.installed&&undo.install_stage.is_none(){return self.save_step(spec,"rollback",&publication,None,finished);}
        let target=ObjectKey::try_new(format!("{}/{}",publication.prefix.trim_end_matches('/'),file.logical_path)).map_err(TransferError::platform)?;
        if !undo.installed&&self.canonical_identity(&target).await?==undo.old_generation{return self.save_step(spec,"rollback",&publication,None,finished);}
        let current=self.current_hash(&spec.session_id,&target).await?;
        if current.as_ref().map(|(_,sha)|sha)==undo.old_sha256.as_ref()&&(current.is_some()||undo.old_stage.is_none()){undo.installed=false;return self.save_step(spec,"rollback",&publication,Some((&undo_key,&undo)),finished);}
        if current.as_ref().map(|(_,sha)|sha)!=Some(&file.sha256){return Err(TransferError::RecoveryRequired{message:"canonical bytes match neither installed version nor preimage".into()});}
        let prefix=ObjectPrefix::new(&publication.prefix).map_err(TransferError::platform)?;self.primary.renew_gate(&prefix,&publication.owner,publication.epoch).await.map_err(TransferError::platform)?;
        let mut processed=0;
        if let Some(old)=undo.old_stage.clone(){
            let stage=undo.restore_stage.clone().unwrap_or_else(||self.opaque_id("restore"));undo.restore_stage=Some(stage.clone());self.journal.backend_put(&spec.session_id.0,&undo_key,&undo)?;let current=self.copy_cursor(&stage,&spec.session_id).await?;
            if !current.sealed&&current.prefix_sha256().is_none(){self.primary.stage_abort(&stage,&spec.session_id.0,1).await.map_err(TransferError::platform)?;undo.restore_stage=None;undo.restore_offset=0;return self.save_step(spec,"rollback",&publication,Some((&undo_key,&undo)),PublicationStep{cursor:Some(index.to_string()),..Default::default()});}
            let offset=self.work_input(&spec.session_id,&format!("restore_offset:{}:{index}",spec.publication_id),undo.restore_offset)?;let end=offset.saturating_add(CHUNK_BYTES).min(undo.old_size);
            if !current.sealed&&current.confirmed_bytes<end{if current.confirmed_bytes!=offset{return Err(TransferError::RecoveryRequired{message:"restore cursor differs from planned range".into()});}let bytes=self.stage_read_range(&old,offset..end).await?;self.primary.stage_append(&stage,&spec.session_id.0,1,offset,&bytes,end==undo.old_size).await.map_err(TransferError::platform)?;}else if !current.sealed&&undo.old_size==0{self.primary.stage_append(&stage,&spec.session_id.0,1,0,&[],true).await.map_err(TransferError::platform)?;}
            processed=end-offset;undo.restore_offset=end;if end<undo.old_size{return self.save_step(spec,"rollback",&publication,Some((&undo_key,&undo)),PublicationStep{cursor:Some(index.to_string()),processed_bytes:processed,..Default::default()});}let sha=undo.old_sha256.as_deref().ok_or_else(||TransferError::RecoveryRequired{message:"preimage SHA missing".into()})?;let sealed=self.primary.stage_seal(&stage,&spec.session_id.0,1,undo.old_size,sha).await.map_err(TransferError::platform)?;
            if processed>0{return self.save_step(spec,"rollback",&publication,Some((&undo_key,&undo)),PublicationStep{cursor:Some(index.to_string()),processed_bytes:processed,..Default::default()});}let operation=self.journal.executing_operation(&spec.session_id)?;let(done,bytes)=self.stores.prepare_transfer_asset(&prefix,&publication.owner,publication.epoch,crate::native::object_store::PreparedAsset{stage:&sealed.key,key:&target,sha256:sha},&operation.0).await.map_err(TransferError::platform)?;processed=bytes;if !done{return self.save_step(spec,"rollback",&publication,Some((&undo_key,&undo)),PublicationStep{cursor:Some(index.to_string()),processed_bytes:processed,..Default::default()});}self.stores.commit_transfer_asset(&prefix,&publication.owner,publication.epoch,&sealed.key,&target,sha).await.map_err(TransferError::platform)?;
        }else{self.stores.remove_transfer_asset(&prefix,&publication.owner,publication.epoch,&target,&file.sha256).await.map_err(TransferError::platform)?;}
        undo.installed=false;self.save_step(spec,"rollback",&publication,Some((&undo_key,&undo)),PublicationStep{processed_bytes:processed,..finished})
    }
    async fn collect_session_resources(&self,session:&SessionRow,cursor:Option<&str>)->TR<PublicationStep>{
        self.journal.require_authority()?;if !session.state.is_terminal(){return Err(TransferError::invalid("session_active","live resources cannot be reclaimed"));}
        if !self.journal.active_publications(&session.session_id,1).await?.is_empty(){return Err(TransferError::RecoveryRequired{message:"active publication protects its resources".into()});}
        let mut state:NativeGcCursor=cursor.map(serde_json::from_str).transpose().map_err(TransferError::platform)?.unwrap_or_default();
        match state.phase {
            0=>{let step=self.release_snapshot_page(&session.session_id,state.cursor.as_deref()).await?;state.cursor=step.cursor;if step.done{state.phase=1;state.cursor=None;}},
            1=>{
                let(leases,next)=self.primary.stage_lease_page(state.cursor.as_deref(),MAX_PAGE_ITEMS).await.map_err(TransferError::platform)?;
                for lease in leases{if lease.owner==session.session_id.0{
                    // A terminal owner can never renew its lease, so the 24h stage TTL must not delay collection: abort first under the same (owner, epoch) CAS to mark `Aborting` and drain bounded checkpoint pages, then reclaim.
                    self.primary.stage_abort(&lease.stage_id,&lease.owner,lease.epoch).await.map_err(TransferError::platform)?;
                    if !self.primary.stage_reclaim(&lease.stage_id,&lease.owner,lease.epoch).await.map_err(TransferError::platform)?{return Ok(PublicationStep{cursor:Some(serde_json::to_string(&state).map_err(TransferError::platform)?),..Default::default()});}
                }}
                state.cursor=next;if state.cursor.is_none(){state.phase=2;}
            },
            2=>{
                let page=self.journal.backend_page::<NativePublication>(&format!("publication_owners:{}",session.session_id.0),state.cursor.as_deref(),1)?;
                if let Some(publication)=page.items.first(){state.inner=self.stores.reclaim_remote_owner(&publication.owner,publication.epoch,state.inner.as_deref(),MAX_PAGE_ITEMS).await.map_err(TransferError::platform)?;if state.inner.is_none(){state.cursor=page.next_cursor;if state.cursor.is_none(){state.phase=3;}}}else{state.phase=3;state.cursor=None;}
            },
            3=>{
                let page=self.journal.backend_page::<IllustrationSpool>(&format!("catalog_spools:{}",session.session_id.0),state.cursor.as_deref(),MAX_PAGE_ITEMS)?;
                for spool in page.items{if !spool.path.starts_with(&format!(".narou/transfer/catalog-indexes/{}/",session.session_id.0))||spool.path.contains("..") {return Err(TransferError::invalid("unsafe_spool","private spool owner path is invalid"));}match fs::remove_file(self.root.join(spool.path)){Ok(())=>{},Err(error)if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(TransferError::platform(error))}}
                state.cursor=page.next_cursor;if state.cursor.is_none(){state.phase=4;}
            },
            4=>{
                let connection=self.journal.control_connection();let owners={let connection=connection.lock();let exists:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='native_transfer_id_owners')",[],|row|row.get(0)).map_err(TransferError::platform)?;if exists{let mut statement=connection.prepare("SELECT owner FROM native_transfer_id_owners WHERE allocator=?1 AND owner>=?2 AND owner<?3 AND (?4 IS NULL OR owner>?4) ORDER BY owner LIMIT 100").map_err(TransferError::platform)?;statement.query_map(rusqlite::params![if self.journal.state.is_some(){"sqlite"}else{"yaml"},format!("{}:",session.session_id.0),format!("{};",session.session_id.0),state.cursor],|row|row.get::<_,String>(0)).map_err(TransferError::platform)?.collect::<std::result::Result<Vec<_>,_>>().map_err(TransferError::platform)?}else{Vec::new()}};
                for owner in &owners{crate::db::with_database_mut(|db|db.release_transfer_id(owner)).map_err(TransferError::platform)?;}
                if owners.len()<MAX_PAGE_ITEMS{connection.lock().execute("DELETE FROM native_transfer_target_claims WHERE session_id=?1",[&session.session_id.0]).map_err(TransferError::platform)?;state.phase=5;state.cursor=None;}else{state.cursor=owners.last().cloned();}
            },
            5=>{
                let page=self.journal.backend_page::<NativePublication>(&format!("publication_owners:{}",session.session_id.0),state.cursor.as_deref(),1)?;if let Some(publication)=page.items.first(){if super::reclaim_metadata_images(&self.root,&session.session_id.0,publication.publication_id,MAX_PAGE_ITEMS).map_err(TransferError::platform)?{state.cursor=page.next_cursor;if state.cursor.is_none(){return Ok(PublicationStep{done:true,..Default::default()});}}}else{return Ok(PublicationStep{done:true,..Default::default()});}
            },
            _=>return Err(TransferError::invalid("invalid_gc_cursor","unknown private GC phase")),
        }
        Ok(PublicationStep{cursor:Some(serde_json::to_string(&state).map_err(TransferError::platform)?),..Default::default()})
    }
}

#[cfg(test)]
#[path="publication_tests.rs"]
mod tests;
