#[derive(Debug,Clone,Serialize,Deserialize)]
struct TocStoryStage {handle:String,operation:OperationId,sha256:Option<String>,complete:bool,present:bool,size:u64,generation:String,record_revision:String,content_revision:String}

impl NativeTransferBackend {
    async fn readable_generation(&self,key:&ObjectKey)->TR<Option<String>>{
        if let Some(generation)=self.filesystem.source_generation(key).await.map_err(TransferError::platform)?{return Ok(Some(format!("fs:{generation}")));}
        self.primary.source_generation(key).await.map(|value|value.map(|generation|format!("primary:{generation}"))).map_err(TransferError::platform)
    }
    async fn source_generation(&self,key:&ObjectKey)->TR<Option<String>>{
        if let Some(generation)=self.readable_generation(key).await?{return Ok(Some(generation));}let Some(metadata)=self.assets.stat(key).await.map_err(TransferError::platform)?else{return Ok(None)};let etag=metadata.etag.ok_or_else(||TransferError::RecoveryRequired{message:"source has no stable generation or ETag".into()})?;Ok(Some(format!("remote:{}:{etag}",metadata.size)))
    }
    async fn frozen_source_generation(&self,entry:&ExportNovelEntry,file:&SourceFileEntry,key:&ObjectKey)->TR<String>{
        let generation=self.source_generation(key).await?.ok_or_else(||TransferError::PlanStale{message:"source disappeared before snapshot".into()})?;let binding=format!("source_generation:{}:{}",entry.novel_ordinal,sha256_hex(file.logical_path.as_bytes()));
        let expected=self.journal.run_write(|rows|{if let Some(expected)=rows.get::<String>("backend",&entry.session_id.0,&binding)?{return Ok(expected);}rows.put("backend",&entry.session_id.0,&binding,&generation)?;Ok(generation.clone())})?;
        if expected!=generation{return Err(TransferError::PlanStale{message:"source changed between snapshot ranges".into()});}Ok(expected)
    }
    async fn restart_unverified_stage(&self,session:&SessionRow,mut file:FileRow,handle:&str)->TR<()>{
        if !matches!(file.state,FileState::Pending|FileState::Receiving|FileState::Failed){return Err(TransferError::RecoveryRequired{message:"verified or finalizing files cannot be reset".into()});}
        file.stage=None;file.pin=None;file.state=FileState::Pending;file.confirmed_bytes=0;file.next_chunk_index=0;file.confirmed_prefix_sha256=Some(sha256_hex(&[]));file.hash_checkpoint=None;file.finalize_operation=None;
        if session.direction==TransferDirection::Import{self.journal.reset_file(&file).await?;}else{self.journal.update_file(&file).await?;}self.stage_abort(&session.session_id,handle).await
    }
    async fn toc_story_stage(&self,entry:&ExportNovelEntry,prefix:&ObjectKey)->TR<TocStoryStage>{
        use futures::StreamExt;
        let session=&entry.session_id;let key=format!("toc_story:{}",entry.novel_ordinal);let toc=prefix.join("toc.yaml").map_err(TransferError::platform)?;
        let generation=self.readable_generation(&toc).await?.ok_or_else(||TransferError::invalid("required_file_missing","source toc.yaml is missing"))?;
        let operation=self.journal.executing_operation(session)?;
        if let Some(mut previous)=self.journal.backend_get::<TocStoryStage>(&session.0,&key)?{
            if previous.generation!=generation||previous.record_revision!=entry.record_revision||previous.content_revision!=entry.content_revision{return Err(TransferError::PlanStale{message:"TOC metadata changed during capture".into()});}
            if let Some(cursor)=self.primary.stage_cursor(&previous.handle).await.map_err(TransferError::platform)?&&cursor.sealed{if cursor.confirmed_bytes!=previous.size||previous.sha256.is_none()||cursor.prefix_sha256()!=previous.sha256{return Err(TransferError::RecoveryRequired{message:"frozen TOC story lost its immutable seal".into()});}
                if !previous.complete{previous.complete=true;self.journal.backend_put(&session.0,&key,&previous)?;}
                if previous.operation==operation{return Err(TransferError::Busy{code:"catalog_preparing",message:"TOC metadata initializer already completed; adopt only".into()});}return Ok(previous);}
            if previous.complete{return Err(TransferError::RecoveryRequired{message:"completed TOC story stage is missing".into()});}self.stage_abort(session,&previous.handle).await?;
        }
        let handle=self.opaque_id("story");let mut story_stage=TocStoryStage{handle:handle.clone(),operation,sha256:None,complete:false,present:false,size:0,generation,record_revision:entry.record_revision.clone(),content_revision:entry.content_revision.clone()};
        self.journal.backend_put(&session.0,&key,&story_stage)?;self.journal.backend_put("stages",&handle,&StageBinding{owner:session.0.clone(),file_id:MANIFEST_FILE_ID,size:0,epoch:1})?;
        self.primary.stage_open(&handle,&session.0,1,SESSION_IDLE_TTL_SECS).await.map_err(TransferError::platform)?;
        let mut lease=crate::application::novel_access::NovelReadLease::admit(&self.objects,&ObjectPrefix::from(prefix.clone()),&format!("{}:metadata:{}",session.0,entry.novel_ordinal)).await.map_err(TransferError::platform)?;
        let result:TR<()> = async {
            let admitted=self.primary.gate_info(lease.prefix()).await.map_err(TransferError::platform)?;let expected_revision=entry.content_revision.parse::<u64>().map_err(TransferError::platform)?;if admitted.epoch!=lease.epoch()||admitted.content_revision!=expected_revision{return Err(TransferError::PlanStale{message:"source changed before metadata admission".into()});}
            let metadata=self.assets.stat(&toc).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("required_file_missing","source TOC disappeared"))?;
            if metadata.size>16*1024*1024{return Err(TransferError::invalid("metadata_budget","TOC metadata initializer exceeds16MiB"));}
            let local=self.checked_source_path(prefix.as_ref(),"toc.yaml")?.is_file();
            let initialized=if local{None}else{self.legacy_source(session,&format!("initialized_source:{}:{}",entry.novel_ordinal,sha256_hex(b"toc.yaml")),&toc,metadata.size,"catalog_preparing").await?};
            let mut input=if let Some(stage)=initialized{self.primary.stage_read(&stage,0..metadata.size).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("metadata_missing","initialized TOC stage missing"))?}else{lease.open_all(&toc).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("required_file_missing","source TOC disappeared"))?};
            let mut bytes=Vec::with_capacity(metadata.size as usize);while let Some(chunk)=input.next().await{let chunk=chunk.map_err(TransferError::platform)?;if chunk.len()>crate::transfer::protocol::IO_CHUNK_BYTES||bytes.len()+chunk.len()>16*1024*1024{return Err(TransferError::invalid("metadata_budget","TOC decoder exceeded initializer bounds"));}bytes.extend_from_slice(&chunk);lease.refresh().await.map_err(TransferError::platform)?;}
            if bytes.len() as u64!=metadata.size{return Err(TransferError::PlanStale{message:"TOC source size changed during projection".into()});}
            let story=crate::downloader::persistence::deserialize_toc_story(&bytes).map_err(TransferError::platform)?;
            if self.readable_generation(&toc).await?.as_deref()!=Some(story_stage.generation.as_str()){return Err(TransferError::PlanStale{message:"TOC generation changed during projection".into()});}
            let observed=self.primary.gate_info(lease.prefix()).await.map_err(TransferError::platform)?;if observed.epoch!=admitted.epoch||observed.content_revision!=admitted.content_revision{return Err(TransferError::PlanStale{message:"source content changed during metadata projection".into()});}
            story_stage.present=story.is_some();let story=story.as_deref().unwrap_or("");story_stage.size=story.len() as u64;story_stage.sha256=Some(sha256_hex(story.as_bytes()));
            self.journal.backend_put(&session.0,&key,&story_stage)?;self.journal.backend_put("stages",&handle,&StageBinding{owner:session.0.clone(),file_id:MANIFEST_FILE_ID,size:story_stage.size,epoch:1})?;
            for(offset,chunk)in story.as_bytes().chunks(crate::transfer::protocol::IO_CHUNK_BYTES).enumerate(){let offset=offset as u64*crate::transfer::protocol::IO_CHUNK_BYTES as u64;self.primary.stage_append(&handle,&session.0,1,offset,chunk,offset+chunk.len() as u64==story_stage.size).await.map_err(TransferError::platform)?;}
            if story.is_empty(){self.primary.stage_append(&handle,&session.0,1,0,&[],true).await.map_err(TransferError::platform)?;}
            self.primary.stage_seal(&handle,&session.0,1,story_stage.size,story_stage.sha256.as_deref().ok_or_else(||TransferError::invalid("metadata_corrupt","story seal SHA missing"))?).await.map_err(TransferError::platform)?;
            lease.validate().await.map_err(TransferError::platform)?;story_stage.complete=true;self.journal.backend_put(&session.0,&key,&story_stage)?;Ok(())
        }.await;
        let validation=lease.validate().await.map_err(TransferError::platform);let release=lease.release().await.map_err(TransferError::platform);validation?;release?;result?;
        Err(TransferError::Busy{code:"catalog_preparing",message:"TOC metadata projected once; resume raw fragment capture".into()})
    }
    async fn source_metadata_layout(&self,entry:&ExportNovelEntry,record:&crate::db::NovelRecord,prefix:&ObjectKey,members:Option<usize>)->TR<crate::transfer::NovelJsonLayout>{
        let parent=entry.kind==NovelKind::VirtualParent;let mut optional=0x0fu16;if record.domain.is_some(){optional|=1<<4;}
        if record.attribute_tag().is_some(){optional|=1<<5;}
        if !parent&&record.ncode.is_some(){optional|=1<<6;}
        if record.general_lastup.is_some(){optional|=1<<8;}optional|=1<<9;
        if !parent&&self.toc_story_stage(entry,prefix).await?.present{optional|=1<<7;}
        let tags=record.tags.iter().filter(|tag|!parent||!crate::virtual_novel::is_internal_tag(tag)).count();let tags=u32::try_from(tags).map_err(|_|TransferError::invalid("metadata_budget","tag count overflow"))?;let members=members.map(u32::try_from).transpose().map_err(|_|TransferError::invalid("metadata_budget","member count overflow"))?;
        let layout=crate::transfer::NovelJsonLayout{novel_type:record.novel_type,optional,tags,members};layout.fields().map_err(TransferError::platform)?;Ok(layout)
    }
    async fn metadata_page(&self,session:&SessionId,ordinal:u32,input:&MetadataCursor,limit:usize)->TR<ExportMetadataPage>{
        use std::borrow::Cow;
        self.journal.executing_operation(session)?;let source=self.snapshot_source(session,ordinal)?;let fields=source.layout.fields().map_err(TransferError::platform)?;let mut cursor=input.clone();
        if cursor.field>fields{return Err(TransferError::invalid("invalid_cursor","metadata cursor is outside layout"));}
        let frozen:FrozenSelection=self.journal.backend_get(&session.0,"export_selection")?.ok_or_else(||TransferError::invalid("snapshot_missing","export selection missing"))?;let members=frozen.memberships.get(&source.entry.id.0);let limit=limit.clamp(1,MAX_PAGE_ITEMS);
        let mut fragments=crate::db::with_database_mut(|db|{
            db.refresh()?;let record=db.get(source.entry.id.0).ok_or_else(||crate::error::NarouError::NotFound("export source disappeared".into()))?;
            if Self::record_revision(record).map_err(|error|crate::error::NarouError::Platform(error.to_string()))?!=source.entry.record_revision{return Err(crate::error::NarouError::Platform("plan_stale: source metadata changed".into()));}
            let parent=source.entry.kind==NovelKind::VirtualParent;let general_lastup=record.general_lastup.map(super::rfc3339);let last_update=super::rfc3339(record.last_update);let mut tags=record.tags.iter().filter(|tag|!parent||!crate::virtual_novel::is_internal_tag(tag));let mut tag_position=0u32;let mut current_tag=None;let mut fragments=Vec::new();
            while cursor.field<fields&&fragments.len()<limit{
                if !source.layout.present(cursor.field){cursor.field+=1;continue;}
                if cursor.field==7{break;}
                let text:Cow<'_,str>=match cursor.field{0=>Cow::Borrowed(&record.toc_url),1=>Cow::Borrowed(&record.title),2=>Cow::Borrowed(&record.author),3=>Cow::Borrowed(&record.sitename),4=>Cow::Borrowed(record.domain.as_deref().unwrap_or("")),5=>Cow::Borrowed(record.attribute_tag().unwrap_or("")),6=>Cow::Borrowed(record.ncode.as_deref().unwrap_or("")),8=>Cow::Borrowed(general_lastup.as_deref().unwrap_or("")),9=>Cow::Borrowed(&last_update),field if field<10+source.layout.tags=>{let index=field-10;if current_tag.as_ref().is_none_or(|(at,_)|*at!=index){let tag=tags.nth((index-tag_position) as usize).ok_or_else(||crate::error::NarouError::Platform("metadata tag cursor changed".into()))?;tag_position=index+1;current_tag=Some((index,tag.as_str()));}Cow::Borrowed(current_tag.as_ref().expect("assigned tag").1)},field=>{let id=members.and_then(|members|members.get((field-10-source.layout.tags) as usize)).ok_or_else(||crate::error::NarouError::Platform("metadata member cursor changed".into()))?;Cow::Borrowed(&db.get(*id).ok_or_else(||crate::error::NarouError::NotFound("member disappeared".into()))?.toc_url)}};
                if cursor.byte>text.len()||!text.is_char_boundary(cursor.byte){return Err(crate::error::NarouError::Platform("invalid metadata byte cursor".into()));}let mut end=cursor.byte.saturating_add(crate::transfer::protocol::TEXT_FRAGMENT_MAX_BYTES).min(text.len());while !text.is_char_boundary(end){end-=1;}let last=end==text.len();fragments.push(MetadataFragment{field:cursor.field,part:cursor.part,byte_offset:cursor.byte,bytes:text.as_bytes()[cursor.byte..end].to_vec(),last});cursor.byte=end;cursor.part+=1;if last{cursor.field+=1;cursor.byte=0;cursor.part=0;}
            }Ok(fragments)
        }).map_err(TransferError::platform)?;
        if cursor.field==7&&fragments.len()<limit{
            let story:TocStoryStage=self.journal.backend_get(&session.0,&format!("toc_story:{ordinal}"))?.ok_or_else(||TransferError::invalid("metadata_missing","frozen story descriptor missing"))?;
            while cursor.field==7&&fragments.len()<limit{let end=(cursor.byte as u64).saturating_add(crate::transfer::protocol::TEXT_FRAGMENT_MAX_BYTES as u64).min(story.size);if cursor.byte as u64>story.size{return Err(TransferError::invalid("invalid_cursor","story byte cursor is outside field"));}let mut bytes=self.stage_read_range(&story.handle,cursor.byte as u64..end).await?;if let Err(error)=std::str::from_utf8(&bytes){if error.error_len().is_some(){return Err(TransferError::invalid("metadata_corrupt","story stage is not UTF-8"));}bytes.truncate(error.valid_up_to());}let next=cursor.byte+bytes.len();let last=next as u64==story.size;if !last&&bytes.is_empty(){return Err(TransferError::invalid("metadata_corrupt","story fragment makes no progress"));}fragments.push(MetadataFragment{field:7,part:cursor.part,byte_offset:cursor.byte,bytes,last});cursor.byte=next;cursor.part+=1;if last{cursor.field+=1;cursor.byte=0;cursor.part=0;}}
        }
        while cursor.field<fields&&!source.layout.present(cursor.field){cursor.field+=1;}Ok(ExportMetadataPage{fragments,next_cursor:(cursor.field<fields).then_some(cursor)})
    }
}
