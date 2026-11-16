//! Bounded native-to-S3 preparation. Canonical publication only swaps a ref.
use super::*;

const REMOTE_PART_BYTES: u64 = ts::S3_STAGE_PART_MIN as u64;
const CONTROL_LIMIT: usize = 64 * 1024;

impl NativeStores {
    pub(super) async fn load_remote_reservation(&self,key:&ObjectKey)->Result<Option<NativeRemoteReservation>> {
        let Some(bytes)=self.primary.read_small(key).await? else{return Ok(None);};
        if bytes.len()>CONTROL_LIMIT{return Err(NarouError::Platform("remote preparation control exceeds admission".into()));}
        serde_json::from_slice(&bytes).map(Some).map_err(|error|NarouError::Platform(format!("invalid remote preparation: {error}")))
    }
    async fn save_remote_reservation(&self,key:&ObjectKey,expected:Option<u64>,mut row:NativeRemoteReservation)->Result<NativeRemoteReservation>{
        row.revision=expected.unwrap_or(0).checked_add(1).ok_or_else(||NarouError::Platform("remote preparation revision overflow".into()))?;
        let bytes=serde_json::to_vec(&row).map_err(|error|NarouError::Platform(error.to_string()))?;
        if bytes.len()>CONTROL_LIMIT{return Err(NarouError::Platform("remote preparation control exceeds admission".into()));}
        let mut applied=false;
        let mut mutate=|current:Option<&[u8]>|->Result<Option<std::borrow::Cow<'_,[u8]>>>{
            let matches=match(current,expected){(None,None)=>true,(Some(current),Some(expected))=>{
                let current:NativeRemoteReservation=serde_json::from_slice(current).map_err(|error|NarouError::Platform(error.to_string()))?;current.revision==expected
            },_=>false};
            if !matches{return Ok(None);}applied=true;Ok(Some(std::borrow::Cow::Borrowed(&bytes)))
        };
        match self.primary.update_small(key,&mut mutate).await?{
            ObjectUpdateResult::Committed if applied=>Ok(row),
            ObjectUpdateResult::ProjectionFailed(error)=>Err(error),
            _=>Err(NarouError::Platform("transfer_operation_busy: remote preparation changed".into())),
        }
    }
    async fn archive_remote_operation(&self,id:&str,operation:&NativeRemoteOperation)->Result<()> {
        let key=remote_operation_key(id,&operation.id)?;
        let bytes=serde_json::to_vec(operation).map_err(|error|NarouError::Platform(error.to_string()))?;
        let mut mutate=|current:Option<&[u8]>|->Result<Option<std::borrow::Cow<'_,[u8]>>>{
            if let Some(current)=current{
                let current:NativeRemoteOperation=serde_json::from_slice(current).map_err(|error|NarouError::Platform(error.to_string()))?;
                if current!=*operation{return Err(NarouError::Platform("immutable remote operation receipt conflict".into()));}return Ok(None);
            }Ok(Some(std::borrow::Cow::Borrowed(&bytes)))
        };
        match self.primary.update_small(&key,&mut mutate).await?{ObjectUpdateResult::Committed=>Ok(()),ObjectUpdateResult::ProjectionFailed(error)=>Err(error),ObjectUpdateResult::Conflict=>Err(NarouError::Platform("remote operation receipt conflict".into()))}
    }
    /// Exactly one fixed operation per invocation. Retries reconcile the same
    /// range or return its immutable receipt; they never start the next part.
    pub fn prepare_transfer_asset<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,asset:PreparedAsset<'a>,operation:&'a str)->PlatformFuture<'a,Result<(bool,u64)>>{Box::pin(async move{
        let(stage,key,sha)=(asset.stage,asset.key,asset.sha256);
        let Some(remote)=self.s3.as_ref().filter(|_|crate::platform::is_illustration_key(key)) else{return Ok((true,0));};
        if operation.is_empty()||!prefix.matches(key){return Err(NarouError::Platform("invalid remote preparation identity".into()));}
        ts::check_hash_named_key(key,sha)?;self.primary.renew_gate(prefix,owner,epoch).await?;
        let(id,reservation_key)=remote_preparation_identity(owner,epoch,stage,key,sha)?;
        if let Some(bytes)=self.primary.read_small(&remote_operation_key(&id,operation)?).await?{
            let receipt:NativeRemoteOperation=serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(error.to_string()))?;
            if receipt.id!=operation||!receipt.done{return Err(NarouError::Platform("invalid remote operation receipt".into()));}return Ok((receipt.ready,receipt.processed));
        }
        let mut row=match self.load_remote_reservation(&reservation_key).await?{
            Some(row)=>row,
            None=>{
                let(size,verified)=self.primary.transfer_fingerprint(stage).await?.ok_or_else(||NarouError::Platform("remote source sealed proof missing".into()))?;
                if !verified.eq_ignore_ascii_case(sha){return Err(NarouError::Platform("remote source SHA proof differs".into()));}
                let generation=self.primary.source_generation(stage).await?.ok_or_else(||NarouError::NotFound(stage.to_string()))?;
                let pool=remote.illustration_pool_enabled_for(key);let physical=if pool{ts::illustration_pool_key(key)?}else{ObjectKey::try_new(format!("content/{id}"))?};
                let row=NativeRemoteReservation{owner:owner.into(),epoch,reference:NativeRemoteReference{physical:physical.to_string(),size,sha256:sha.to_ascii_lowercase(),logical:key.to_string(),prefix:prefix.as_ref().trim_end_matches('/').into(),etag:String::new(),legacy_gc:None},pool,revision:0,
                    preparation:Some(NativeRemotePreparation{source:stage.to_string(),source_generation:generation,target_url:remote.location().object_url(&physical),remote_stage:id.clone(),phase:NativeRemotePhase::Probe,cursor:empty_remote_cursor(),etag:None,operation:None})};
                // The physical owner reference is durable BEFORE any S3 I/O.
                self.save_remote_reservation(&reservation_key,None,row).await?
            }
        };
        if row.owner!=owner||row.epoch!=epoch||row.reference.logical!=key.as_ref()||row.reference.prefix!=prefix.as_ref().trim_end_matches('/')||!row.reference.sha256.eq_ignore_ascii_case(sha){return Err(NarouError::Platform("remote preparation owner/identity conflict".into()));}
        let physical=ObjectKey::try_new(row.reference.physical.clone())?;
        let mut prep=row.preparation.take().ok_or_else(||NarouError::Platform("remote preparation state missing".into()))?;
        if prep.source!=stage.as_ref()||prep.remote_stage!=id||prep.target_url!=remote.location().object_url(&physical){return Err(NarouError::Platform("remote preparation source/location changed".into()));}
        if let Some(previous)=&prep.operation{
            if previous.id==operation&&previous.done{return Ok((previous.ready,previous.processed));}
            if previous.id!=operation{
                if !previous.done{return Err(NarouError::Platform("transfer_operation_busy: previous remote operation pending".into()));}
                self.archive_remote_operation(&id,previous).await?;
            }
        }
        if !prep.operation.as_ref().is_some_and(|previous|previous.id==operation){
            let offset=prep.cursor.confirmed_bytes;if offset>row.reference.size{return Err(NarouError::Platform("remote preparation cursor exceeds source".into()));}
            let action=match prep.phase{NativeRemotePhase::Probe=>NativeRemoteAction::Probe,NativeRemotePhase::Uploading if offset==row.reference.size&&row.reference.size!=0=>NativeRemoteAction::Seal,NativeRemotePhase::Uploading=>NativeRemoteAction::Upload,NativeRemotePhase::Verifying=>NativeRemoteAction::Verify,NativeRemotePhase::Ready=>NativeRemoteAction::Ready};
            let length=if matches!(action,NativeRemoteAction::Upload|NativeRemoteAction::Verify){(row.reference.size-offset).min(REMOTE_PART_BYTES)}else{0};
            prep.operation=Some(NativeRemoteOperation{id:operation.into(),action,offset,length,done:false,ready:false,processed:0});
            let revision=row.revision;row.preparation=Some(prep);row=self.save_remote_reservation(&reservation_key,Some(revision),row).await?;prep=row.preparation.take().expect("saved preparation");
        }
        let op=prep.operation.clone().expect("claimed operation");let end=op.offset.checked_add(op.length).ok_or_else(||NarouError::Platform("remote operation range overflow".into()))?;
        match op.action{
            NativeRemoteAction::Probe=>{
                prep.cursor=remote.stage_open_target(&id,owner,epoch,24*60*60,&physical).await?;
                if let Some(metadata)=AssetStore::stat(remote.as_ref(),&physical).await?{start_verification(&mut prep,metadata,row.reference.size)?;}else{prep.phase=NativeRemotePhase::Uploading;}
            }
            NativeRemoteAction::Upload=>{
                let cursor=remote.stage_cursor(&id).await?.ok_or_else(||NarouError::Platform("remote stage disappeared".into()))?;
                if op.length!=0&&cursor.confirmed_bytes==end{
                    prep.cursor=cursor;
                }else{
                    if cursor.confirmed_bytes!=op.offset{return Err(NarouError::Platform("remote append offset conflict".into()));}
                    if self.primary.source_generation(stage).await?.as_deref()!=Some(prep.source_generation.as_str()){return Err(NarouError::Platform("remote source generation changed".into()));}
                    let mut input=self.primary.read_stream_range(stage,op.offset..end).await?.ok_or_else(||NarouError::NotFound(stage.to_string()))?;let mut bytes=Vec::with_capacity(op.length as usize);
                    while let Some(chunk)=input.next().await{let chunk=chunk?;if bytes.len() as u64+chunk.len() as u64>op.length{return Err(NarouError::Platform("remote source range overflow".into()));}bytes.extend_from_slice(&chunk);}
                    if bytes.len() as u64!=op.length||self.primary.source_generation(stage).await?.as_deref()!=Some(prep.source_generation.as_str()){return Err(NarouError::Platform("remote source range/generation changed".into()));}
                    match remote.stage_append(&id,owner,epoch,op.offset,&bytes,end==row.reference.size).await{
                        Ok(cursor)=>{if cursor.confirmed_bytes!=end{return Err(NarouError::Platform("remote append checkpoint conflict".into()));}prep.cursor=cursor;
                            if row.reference.size==0{let sealed=remote.stage_seal(&id,owner,epoch,0,sha).await?;if sealed.size!=0{return Err(NarouError::Platform("empty remote seal conflict".into()));}prep.etag=Some(remote.sealed_stage_etag(&id,owner,epoch,0,sha).await?);prep.cursor.sealed=true;prep.phase=NativeRemotePhase::Ready;}
                        }
                        Err(error)=>{
                            let recovered=remote.stage_cursor(&id).await?;
                            if op.length!=0&&recovered.as_ref().is_some_and(|cursor|cursor.confirmed_bytes==end){prep.cursor=recovered.expect("checked cursor");}
                            else if let Some(metadata)=AssetStore::stat(remote.as_ref(),&physical).await?{start_verification(&mut prep,metadata,row.reference.size)?;}else{return Err(error);}
                        }
                    }
                }
            }
            NativeRemoteAction::Seal=>{
                match remote.stage_seal(&id,owner,epoch,row.reference.size,sha).await{
                    Ok(_)=>{prep.cursor=remote.stage_cursor(&id).await?.ok_or_else(||NarouError::Platform("sealed remote stage disappeared".into()))?;prep.etag=Some(remote.sealed_stage_etag(&id,owner,epoch,row.reference.size,sha).await?);prep.phase=NativeRemotePhase::Ready;}
                    Err(error)=>{if let Some(metadata)=AssetStore::stat(remote.as_ref(),&physical).await?{start_verification(&mut prep,metadata,row.reference.size)?;}else{return Err(error);}}
                }
            }
            NativeRemoteAction::Verify=>{
                let etag=prep.etag.as_deref().ok_or_else(||NarouError::Platform("remote verification generation missing".into()))?;
                let mut input=remote.read_stream_range_if_match(&physical,op.offset..end,etag).await?.ok_or_else(||NarouError::NotFound(physical.to_string()))?;
                let mut hash=prep.cursor.resume_hasher()?;let mut crc=crc32fast::Hasher::new_with_initial(prep.cursor.crc32);let mut seen=0u64;
                while let Some(chunk)=input.next().await{let chunk=chunk?;seen=seen.checked_add(chunk.len() as u64).filter(|value|*value<=op.length).ok_or_else(||NarouError::Platform("remote verification range overflow".into()))?;hash.update(&chunk);crc.update(&chunk);}
                if seen!=op.length{return Err(NarouError::Platform("remote verification range truncated".into()));}
                prep.cursor=StageCursor{confirmed_bytes:end,hash_state:Some(ts::encode_sha_state(&hash)),sealed:false,crc32:crc.finalize(),storage_position:1};
                if end==row.reference.size{
                    if !hex::encode(hash.finalize()).eq_ignore_ascii_case(sha){return Err(NarouError::Platform("existing immutable target SHA differs".into()));}
                    remote.seal_verified_target(&id,owner,epoch,&prep.cursor,sha,etag).await?;prep.cursor.sealed=true;prep.phase=NativeRemotePhase::Ready;
                }
            }
            NativeRemoteAction::Ready=>{}
        }
        self.primary.renew_gate(prefix,owner,epoch).await?;
        if prep.phase==NativeRemotePhase::Ready{row.reference.etag=prep.etag.clone().filter(|etag|!etag.is_empty()).ok_or_else(||NarouError::Platform("prepared remote generation proof missing".into()))?;}
        let ready=prep.phase==NativeRemotePhase::Ready;let current=prep.operation.as_mut().expect("claimed operation");current.done=true;current.ready=ready;current.processed=op.length;
        let revision=row.revision;row.preparation=Some(prep);self.save_remote_reservation(&reservation_key,Some(revision),row).await?;Ok((ready,op.length))
    })}
}

fn start_verification(prep:&mut NativeRemotePreparation,metadata:ObjectMetadata,size:u64)->Result<()>{
    if metadata.size!=size{return Err(NarouError::Platform("existing immutable target length differs".into()));}
    prep.etag=Some(metadata.etag.filter(|etag|!etag.is_empty()).ok_or_else(||NarouError::Platform("existing immutable target omitted ETag".into()))?);prep.cursor=empty_remote_cursor();prep.phase=NativeRemotePhase::Verifying;Ok(())
}
