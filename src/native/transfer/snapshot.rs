#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SnapshotAdmission { phase:u8, cursor:Option<String>, admitted:bool, released:bool }
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotGate { source:SnapshotSource, owner:String, epoch:u64, released:bool }
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IllustrationSpool { path:String, complete:bool, size:u64, count:u32, operation:OperationId }
#[derive(Debug,Clone,Serialize,Deserialize)]
struct LegacySourceReference {handle:String,operation:OperationId,sha256:Option<String>}

impl NativeTransferBackend {
    fn snapshot_sources_owner(session:&SessionId)->String {format!("snapshot_sources:{}",session.0)}
    fn snapshot_gates_owner(session:&SessionId)->String {format!("snapshot_gates:{}",session.0)}
    fn snapshot_source(&self,session:&SessionId,ordinal:u32)->TR<SnapshotSource> {
        self.journal.backend_get(&format!("snapshot_by_ordinal:{}",session.0),&number(ordinal))?.ok_or_else(||TransferError::invalid("snapshot_missing","frozen source metadata missing"))
    }
    fn save_snapshot_source(&self,session:&SessionId,source:&SnapshotSource)->TR<()> {
        self.journal.run_write(|rows|{
            rows.put("backend",&Self::snapshot_sources_owner(session),&format!("{}\0{}",source.prefix,number(source.entry.novel_ordinal)),source)?;
            rows.put("backend",&format!("snapshot_by_ordinal:{}",session.0),&number(source.entry.novel_ordinal),source)
        })
    }
    fn check_snapshot_source(&self,source:&SnapshotSource)->TR<()> {
        let record=self.source_record(&source.entry)?;
        super::assert_source_namespace(&record).map_err(|error|TransferError::invalid("ambiguous_source_namespace",format!("{error}; resolve overlapping saved folders before export")))?;
        if Self::record_revision(&record)?!=source.entry.record_revision || self.content_revision(&record)?!=source.entry.content_revision || self.record_prefix(&record)?.as_ref()!=source.prefix {
            return Err(TransferError::PlanStale{message:"export source changed after cataloguing".into()});
        }
        if let Some(expected)=&source.entry.membership_digest {
            let members=crate::native::virtual_novel::load().map_err(TransferError::platform)?;
            let ids=members.get(record.id).ok_or_else(||TransferError::PlanStale{message:"source parent membership disappeared".into()})?;
            let actual=crate::platform::access_fence::membership_digest(ids.iter().map(i64::to_string));
            if &actual!=expected{return Err(TransferError::PlanStale{message:"source parent membership changed".into()});}
        }
        Ok(())
    }
    async fn admit_snapshot(&self,session:&SessionId)->TR<bool> {
        self.journal.require_authority()?;let current:SnapshotAdmission=self.journal.backend_get(&session.0,"snapshot_admission")?.unwrap_or_default();
        let mut state=self.work_input(session,"snapshot_admission",current)?;
        if state.admitted{return Ok(true);}
        let owner=format!("{}:snapshot:group",session.0);
        if state.phase==0 {
            let page=self.journal.backend_page::<SnapshotSource>(&Self::snapshot_sources_owner(session),state.cursor.as_deref(),MAX_PAGE_ITEMS)?;
            for source in &page.items {
                let key=format!("{}\0{}",source.prefix,number(source.entry.novel_ordinal));
                let mut gate=self.journal.backend_get::<SnapshotGate>(&Self::snapshot_gates_owner(session),&key)?.unwrap_or(SnapshotGate{source:source.clone(),owner:owner.clone(),epoch:0,released:false});
                // The durable owner is reserved before closing admission.
                self.journal.backend_put(&Self::snapshot_gates_owner(session),&key,&gate)?;
                let prefix=ObjectPrefix::new(&source.prefix).map_err(TransferError::platform)?;
                gate.epoch=self.primary.close_gate(&prefix,&owner).await.map_err(TransferError::platform)?;
                self.journal.backend_put(&Self::snapshot_gates_owner(session),&key,&gate)?;
            }
            state.cursor=page.next_cursor;if state.cursor.is_none(){state.phase=1;}
            self.journal.backend_put(&session.0,"snapshot_admission",&state)?;
            return Ok(false);
        }
        let page=self.journal.backend_page::<SnapshotGate>(&Self::snapshot_gates_owner(session),state.cursor.as_deref(),MAX_PAGE_ITEMS)?;
        for gate in &page.items {
            let prefix=ObjectPrefix::new(&gate.source.prefix).map_err(TransferError::platform)?;
            self.primary.renew_gate(&prefix,&gate.owner,gate.epoch).await.map_err(TransferError::platform)?;
            let info=self.primary.gate_info(&prefix).await.map_err(TransferError::platform)?;
            if info.pending_readers!=0{return Ok(false);}
            self.check_snapshot_source(&gate.source)?;
        }
        state.cursor=page.next_cursor;if state.cursor.is_none(){state.admitted=true;}
        self.journal.backend_put(&session.0,"snapshot_admission",&state)?;
        // A gate-control invocation never also copies a payload window.
        Ok(false)
    }
    async fn check_owned_source(&self,entry:&ExportNovelEntry)->TR<SnapshotSource> {
        self.journal.require_authority()?;let session=&entry.session_id;let source=self.snapshot_source(session,entry.novel_ordinal)?;
        if source.entry.id!=entry.id||source.entry.record_revision!=entry.record_revision||source.entry.content_revision!=entry.content_revision||source.entry.membership_digest!=entry.membership_digest{return Err(TransferError::invalid("snapshot_identity_mismatch","source summary differs from frozen catalog"));}
        let key=format!("{}\0{}",source.prefix,number(entry.novel_ordinal));
        let gate:SnapshotGate=self.journal.backend_get(&Self::snapshot_gates_owner(session),&key)?.ok_or_else(||TransferError::invalid("snapshot_gate_missing","source gate is missing"))?;
        if gate.released{return Err(TransferError::Busy{code:"snapshot_gate_lost",message:"snapshot source gate was already released".into()});}
        let prefix=ObjectPrefix::new(&source.prefix).map_err(TransferError::platform)?;
        self.primary.renew_gate(&prefix,&gate.owner,gate.epoch).await.map_err(TransferError::platform)?;
        let info=self.primary.gate_info(&prefix).await.map_err(TransferError::platform)?;
        if info.pending_readers!=0||info.content_revision.to_string()!=source.entry.content_revision{return Err(TransferError::PlanStale{message:"source generation changed or has active readers".into()});}
        // All selected gates were closed and drained before any file copy.
        // Recheck their immutable revision/owner here; the complete metadata
        // projection is checked once at admission and again before release.
        Ok(source)
    }
    async fn finish_snapshot(&self,session:&SessionId)->TR<PublicationStep> {
        if !self.admit_snapshot(session).await?{return Ok(PublicationStep::default());}
        let current:SnapshotAdmission=self.journal.backend_get(&session.0,"snapshot_admission")?.ok_or_else(||TransferError::invalid("snapshot_missing","snapshot admission missing"))?;
        let mut state=self.work_input(session,"snapshot_finish",current)?;
        if state.released{return Ok(PublicationStep{done:true,..Default::default()});}
        let page=self.journal.backend_page::<SnapshotGate>(&Self::snapshot_gates_owner(session),state.cursor.as_deref(),MAX_PAGE_ITEMS)?;
        for mut gate in page.items {
            if gate.released{continue;}
            self.check_snapshot_source(&gate.source)?;
            let prefix=ObjectPrefix::new(&gate.source.prefix).map_err(TransferError::platform)?;let current=self.primary.gate_info(&prefix).await.map_err(TransferError::platform)?;
            if current.owner.as_deref()==Some(&gate.owner)&&current.epoch==gate.epoch {self.primary.open_gate(&prefix,&gate.owner,gate.epoch).await.map_err(TransferError::platform)?;}
            else if current.owner.is_some()||current.epoch!=gate.epoch{return Err(TransferError::RecoveryRequired{message:"snapshot gate owner changed before release".into()});}
            gate.released=true;self.journal.backend_put(&Self::snapshot_gates_owner(session),&format!("{}\0{}",gate.source.prefix,number(gate.source.entry.novel_ordinal)),&gate)?;
        }
        state.cursor=page.next_cursor;state.released=state.cursor.is_none();self.journal.backend_put(&session.0,"snapshot_admission",&state)?;
        Ok(PublicationStep{done:state.released,..Default::default()})
    }
    async fn release_snapshot_page(&self,session:&SessionId,cursor:Option<&str>)->TR<PublicationStep> {
        self.journal.require_authority()?;let page=self.journal.backend_page::<SnapshotGate>(&Self::snapshot_gates_owner(session),cursor,MAX_PAGE_ITEMS)?;
        for mut gate in page.items {
            if gate.released{continue;}
            let prefix=ObjectPrefix::new(&gate.source.prefix).map_err(TransferError::platform)?;let current=self.primary.gate_info(&prefix).await.map_err(TransferError::platform)?;
            if current.owner.as_deref()==Some(&gate.owner) {self.primary.open_gate(&prefix,&gate.owner,current.epoch).await.map_err(TransferError::platform)?;}
            else if current.owner.is_some(){return Err(TransferError::RecoveryRequired{message:"snapshot gate now belongs to another operation".into()});}
            gate.released=true;self.journal.backend_put(&Self::snapshot_gates_owner(session),&format!("{}\0{}",gate.source.prefix,number(gate.source.entry.novel_ordinal)),&gate)?;
        }
        Ok(PublicationStep{done:page.next_cursor.is_none(),cursor:page.next_cursor,..Default::default()})
    }
    fn checked_source_path(&self,prefix:&str,relative:&str)->TR<PathBuf> {
        crate::transfer::validate_relative_path(relative).map_err(TransferError::platform)?;
        let key=ObjectKey::try_new(format!("{}/{relative}",prefix.trim_end_matches('/'))).map_err(TransferError::platform)?;
        let mapped=match &self.filesystem{NativeStore::Fs(store)=>store.resolve_path(&key).map_err(TransferError::platform)?,NativeStore::Sqlite(_)=>return Err(TransferError::invalid("source_store","filesystem source resolver is unavailable"))};let root=self.archive_root();let relative=mapped.strip_prefix(&root).map_err(|_|TransferError::invalid("unsafe_source","resolved source escapes archive root"))?;let mut path=root.clone();
        for part in relative.components(){if !matches!(part,std::path::Component::Normal(_)){return Err(TransferError::invalid("unsafe_source","resolved source contains a non-normal component"));}path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(metadata)=>{
                    #[cfg(windows)] {use std::os::windows::fs::MetadataExt;if metadata.file_attributes()&0x400!=0{return Err(TransferError::invalid("unsafe_source","source contains a reparse point"));}}
                    if metadata.file_type().is_symlink(){return Err(TransferError::invalid("unsafe_source","source contains a symbolic link"));}
                }
                Err(error)if error.kind()==std::io::ErrorKind::NotFound=>{},
                Err(error)=>return Err(TransferError::platform(error)),
            }
        }
        Ok(path)
    }
    async fn legacy_source(&self,session:&SessionId,binding_key:&str,logical:&ObjectKey,size:u64,code:&'static str)->TR<Option<String>> {
        self.journal.require_authority()?;use futures::StreamExt;
        let encoding=self.primary.stored_encoding(logical).await.map_err(TransferError::platform)?;
        if encoding.is_none_or(|encoding|encoding==crate::platform::ObjectEncoding::None){return Ok(None);}
        if size>16*1024*1024{return Err(TransferError::invalid("legacy_metadata_limit","legacy compressed source exceeds decoded16MiB storage contract"));}
        let operation=self.journal.executing_operation(session)?;
        if let Some(reference)=self.journal.backend_get::<LegacySourceReference>(&session.0,binding_key)? {
            if let Some(cursor)=self.primary.stage_cursor(&reference.handle).await.map_err(TransferError::platform)?&&cursor.sealed{if cursor.confirmed_bytes!=size||reference.sha256.is_none()||cursor.prefix_sha256()!=reference.sha256{return Err(TransferError::RecoveryRequired{message:"sealed initializer lost its immutable SHA proof".into()});}
                if reference.operation==operation{return Err(TransferError::Busy{code,message:"initializer unit already completed; adopt without copying payload".into()});}return Ok(Some(reference.handle));}
            self.primary.stage_abort(&reference.handle,&session.0,1).await.map_err(TransferError::platform)?;
        }
        let handle=self.opaque_id("source");let binding=StageBinding{owner:session.0.clone(),file_id:MANIFEST_FILE_ID,size,epoch:1};
        let mut reference=LegacySourceReference{handle:handle.clone(),operation,sha256:None};self.journal.backend_put("stages",&handle,&binding)?;self.journal.backend_put(&session.0,binding_key,&reference)?;
        self.primary.stage_open(&handle,&session.0,1,SESSION_IDLE_TTL_SECS).await.map_err(TransferError::platform)?;
        let mut input=self.assets.read_stream(logical).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("source_missing","legacy source is missing"))?;
        let mut offset=0u64;
        while let Some(bytes)=input.next().await {
            let bytes=bytes.map_err(TransferError::platform)?;
            if bytes.len()>crate::transfer::protocol::IO_CHUNK_BYTES{return Err(TransferError::invalid("codec_io_limit","legacy decoder exceeded64KiB output"));}
            let end=offset.checked_add(bytes.len() as u64).filter(|end|*end<=size).ok_or_else(||TransferError::invalid("source_size_mismatch","legacy source exceeds catalog size"))?;
            self.primary.stage_append(&handle,&session.0,1,offset,&bytes,end==size).await.map_err(TransferError::platform)?;offset=end;
        }
        if offset!=size{return Err(TransferError::invalid("source_short_read","legacy source is truncated"));}
        if size==0{self.primary.stage_append(&handle,&session.0,1,0,&[],true).await.map_err(TransferError::platform)?;}
        let cursor=self.primary.stage_cursor(&handle).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("stage_missing","initializer cursor missing"))?;
        let sha=cursor.prefix_sha256().ok_or_else(||TransferError::invalid("hash_checkpoint_corrupt","initializer hash checkpoint invalid"))?;
        reference.sha256=Some(sha.clone());self.journal.backend_put(&session.0,binding_key,&reference)?;
        self.primary.stage_seal(&handle,&session.0,1,size,&sha).await.map_err(TransferError::platform)?;
        Err(TransferError::Busy{code,message:"legacy source initialized once; resume the next bounded unit".into()})
    }
    async fn illustration_spool(&self,session:&SessionId,ordinal:u32,prefix:&ObjectKey)->TR<IllustrationSpool> {
        use std::io::{BufWriter,Write};
        let key=format!("illustration_spool:{ordinal}");
        if let Some(spool)=self.journal.backend_get::<IllustrationSpool>(&session.0,&key)? {
            if spool.complete{if spool.operation==self.journal.executing_operation(session)?{return Err(TransferError::Busy{code:"catalog_preparing",message:"metadata initializer unit already completed".into()});}return Ok(spool);}
            match fs::remove_file(self.root.join(&spool.path)){Ok(())=>{},Err(error)if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(TransferError::platform(error))}
        }
        let relative=format!(".narou/transfer/catalog-indexes/{}/{}.spool",session.0,self.opaque_id("index"));
        let mut spool=IllustrationSpool{path:relative,complete:false,size:0,count:0,operation:self.journal.executing_operation(session)?};self.journal.backend_put(&session.0,&key,&spool)?;
        self.journal.backend_put(&format!("catalog_spools:{}",session.0),&number(ordinal),&spool)?;
        let path=self.root.join(&spool.path);fs::create_dir_all(path.parent().expect("spool parent")).map_err(TransferError::platform)?;
        let file=fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(TransferError::platform)?;let mut output=BufWriter::with_capacity(64*1024,file);
        if let Some(bytes)=self.objects.read_small(&prefix.join(".illustration_cache.yaml").map_err(TransferError::platform)?).await.map_err(TransferError::platform)? {
            if bytes.len()>16*1024*1024{return Err(TransferError::invalid("metadata_budget","illustration index exceeds16MiB initializer contract"));}
            let yaml=std::str::from_utf8(&bytes).map_err(TransferError::platform)?;
            for filename in crate::illustration_store::IllustrationFilenameReader::new(yaml) {
                let filename=filename.map_err(TransferError::platform)?;
                if filename.contains('/')||filename.contains('\\'){return Err(TransferError::invalid("invalid_illustration_index","indexed illustration must be one filename"));}
                crate::transfer::validate_relative_path(&filename).map_err(TransferError::platform)?;
                spool.count=spool.count.checked_add(1).filter(|count|*count<=50_000).ok_or_else(||TransferError::invalid("metadata_budget","expanded illustration alias count exceeds50000"))?;
                spool.size=spool.size.checked_add(filename.len() as u64+1).filter(|size|*size<=MAX_METADATA_BYTES as u64).ok_or_else(||TransferError::invalid("metadata_budget","expanded illustration filename metadata exceeds24MiB"))?;
                output.write_all(filename.as_bytes()).and_then(|_|output.write_all(b"\n")).map_err(TransferError::platform)?;
            }
        }
        output.flush().map_err(TransferError::platform)?;output.get_ref().sync_all().map_err(TransferError::platform)?;
        spool.complete=true;self.journal.backend_put(&session.0,&key,&spool)?;
        self.journal.backend_put(&format!("catalog_spools:{}",session.0),&number(ordinal),&spool)?;
        Err(TransferError::Busy{code:"catalog_preparing",message:"illustration metadata initialized; resume the same advance identity".into()})
    }
}
