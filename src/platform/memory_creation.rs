//! The Memory adapter uses the same actual gate, byte map and scalar allocator.
use super::*;
use crate::platform::{NovelIdReservation, WriteFence};
use crate::platform::access_fence::{namespace_prefixes_overlap, new_removal_owner};

#[derive(Debug,Clone)]struct Claim{id:i64,generation:i64,prefix:String,url:String,epoch:u64,empty_owned:bool}
#[derive(Debug,Clone)]struct Receipt{prefix:String,url:String,epoch:u64}
#[derive(Debug,Default)]pub(super) struct CreationState{claims:BTreeMap<String,Claim>,receipts:BTreeMap<(i64,i64),Receipt>}
fn belongs(prefix:&str,key:&str)->bool{key==prefix||key.strip_prefix(prefix).is_some_and(|tail|tail.starts_with('/'))}
fn identity(record:&NovelRecord)->Result<(String,String)>{let keys=super::super::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;Ok((keys.prefix().as_ref().to_owned(),crate::db::novel_codec::fold(&record.toc_url)))}
impl CreationState{
    pub(super) fn has_receipt(&self,id:NovelId,generation:i64)->bool{self.receipts.contains_key(&(id.0,generation))}
    pub(super) fn validate_mutations(&self,records:&BTreeMap<i64,NovelRecord>,mutations:&[NovelMutation])->Result<()>{
        for mutation in mutations{let NovelMutation::Upsert(record)=mutation else{continue;};let(prefix,url)=identity(record)?;
            if records.get(&record.id).is_some_and(|old|identity(old).is_ok_and(|old|old==(prefix.clone(),url.clone()))){continue;}
            if self.claims.values().any(|claim|claim.id==record.id||claim.url==url||namespace_prefixes_overlap(&claim.prefix,&prefix)){return Err(NarouError::Platform("record identity is reserved by another creation".into()));}
        }Ok(())
    }
}
impl MemoryNovelRepository{
    fn creation_objects(&self,fence:&WriteFence)->Result<&std::sync::Arc<MemoryObjectStore>>{
        let objects=self.objects.as_ref().ok_or_else(||NarouError::Platform("Memory creation has no bound object-store authority".into()))?;
        if fence.proof_source_id()!=Some(std::sync::Arc::as_ptr(&objects.gates) as usize){return Err(NarouError::Platform("mixed Memory creation writer authority".into()));}Ok(objects)
    }
    pub(super) fn reserve_memory_creation(&self,fence:&WriteFence,reservation:&NovelIdReservation,url:&str)->Result<()>{
        let objects=self.creation_objects(fence)?;let prefix=gate_key_of(&fence.prefix);let url=crate::db::novel_codec::fold(url);let mut gates=objects.gate_map();
        let gate=gates.get(&prefix).ok_or_else(||super::super::access_fence::gate_closed_error(&fence.prefix))?;
        if gate.state!=super::super::GateState::Open||gate.epoch!=fence.epoch{return Err(super::super::access_fence::gate_closed_error(&fence.prefix));}
        let records=self.records.lock();let allocator=self.id_allocator.lock();let mut creation=self.creation.lock();
        if let Some(receipt)=creation.receipts.get(&(reservation.id().0,reservation.generation())){
            let current=records.get(&reservation.id().0).ok_or_else(||NarouError::Platform("published creation was removed".into()))?;
            if receipt.prefix!=prefix||receipt.url!=url||receipt.epoch!=fence.epoch||identity(current)?!=(prefix,url){return Err(NarouError::Platform("published creation identity changed".into()));}return Ok(());
        }
        if !allocator.owns(reservation){return Err(NarouError::Platform("numeric creation reservation lost".into()));}
        if let Some(claim)=creation.claims.get(&prefix){
            if claim.id==reservation.id().0&&claim.generation==reservation.generation()&&claim.epoch==fence.epoch&&claim.url==url{return Ok(());}return Err(NarouError::Platform("creation namespace is already reserved".into()));
        }
        for record in records.values(){let(other,other_url)=identity(record)?;if record.id==reservation.id().0||other_url==url||namespace_prefixes_overlap(&other,&prefix){return Err(NarouError::Platform("registered creation namespace is occupied".into()));}}
        if creation.claims.values().any(|claim|claim.id==reservation.id().0||claim.url==url||namespace_prefixes_overlap(&claim.prefix,&prefix)){return Err(NarouError::Platform("creation identity overlaps another reservation".into()));}
        if gates.iter().any(|(other,gate)|other!=&prefix&&(gate.state==super::super::GateState::Closed||!gate.origin_url_fold.is_empty())&&namespace_prefixes_overlap(other,&prefix)){return Err(NarouError::Platform("creation namespace overlaps retained data".into()));}
        let gate=gates.get_mut(&prefix).expect("validated gate");let bytes=objects.objects.lock();let occupied=bytes.keys().any(|key|key==&prefix||key.strip_prefix(&prefix).is_some_and(|tail|tail.starts_with('/')));
        if (!gate.origin_url_fold.is_empty()&&gate.origin_url_fold!=url)||(gate.origin_url_fold.is_empty()&&occupied){return Err(NarouError::Platform("unowned or foreign retained creation data".into()));}
        let empty_owned=gate.origin_url_fold.is_empty();creation.claims.insert(prefix.clone(),Claim{id:reservation.id().0,generation:reservation.generation(),prefix,url:url.clone(),epoch:fence.epoch,empty_owned});gate.origin_url_fold=url;Ok(())
    }
    pub(super) fn commit_memory_creation(&self,fence:&WriteFence,reservation:&NovelIdReservation,record:NovelRecord)->Result<()>{
        let objects=self.creation_objects(fence)?;let(prefix,url)=identity(&record)?;if prefix!=gate_key_of(&fence.prefix)||record.id!=reservation.id().0{return Err(NarouError::Platform("creation record differs from original reservation".into()));}
        let mut gates=objects.gate_map();let gate=gates.get_mut(&prefix).ok_or_else(||super::super::access_fence::gate_closed_error(&fence.prefix))?;if gate.state!=super::super::GateState::Open||gate.epoch!=fence.epoch{return Err(super::super::access_fence::gate_closed_error(&fence.prefix));}
        let mut records=self.records.lock();let mut allocator=self.id_allocator.lock();let mut creation=self.creation.lock();
        if let Some(receipt)=creation.receipts.get(&(reservation.id().0,reservation.generation())){
            if receipt.prefix!=prefix||receipt.url!=url||receipt.epoch!=fence.epoch||!records.get(&record.id).is_some_and(|current|identity(current).is_ok_and(|current|current==(prefix.clone(),url.clone()))){return Err(NarouError::Platform("published creation replay conflicts".into()));}return Ok(());
        }
        let claim=creation.claims.get(&prefix).ok_or_else(||NarouError::Platform("creation identity was not reserved".into()))?;
        if claim.id!=record.id||claim.generation!=reservation.generation()||claim.epoch!=fence.epoch||claim.url!=url||gate.origin_url_fold!=url||!allocator.owns(reservation)||records.contains_key(&record.id){return Err(NarouError::Platform("creation ownership changed before commit".into()));}
        creation.receipts.insert((record.id,reservation.generation()),Receipt{prefix:prefix.clone(),url,epoch:fence.epoch});creation.claims.remove(&prefix);allocator.commit(record.id);records.insert(record.id,record);Ok(())
    }
    pub(super) fn release_memory_creation(&self,reservation:NovelIdReservation)->Result<()>{
        let Some(objects)=self.objects.as_ref()else{let records=self.records.lock();let committed=records.contains_key(&reservation.id().0);self.id_allocator.lock().release(reservation,committed);return Ok(());};
        let mut gates=objects.gate_map();let records=self.records.lock();let mut allocator=self.id_allocator.lock();let mut creation=self.creation.lock();
        if creation.receipts.contains_key(&(reservation.id().0,reservation.generation())){return Ok(());}
        let claim=creation.claims.values().find(|claim|claim.id==reservation.id().0&&claim.generation==reservation.generation()).cloned();
        let Some(claim)=claim else{let committed=records.contains_key(&reservation.id().0);allocator.release(reservation,committed);return Ok(());};
        if records.contains_key(&claim.id)||!allocator.owns(&reservation){return Err(NarouError::Platform("pending creation scalar authority changed".into()));}
        let prefix=super::super::ObjectPrefix::new(claim.prefix.clone())?;let next=claim.epoch.checked_add(1).ok_or_else(||NarouError::Platform("creation epoch exhausted".into()))?;let gate=gates.get_mut(&claim.prefix).ok_or_else(||super::super::access_fence::gate_closed_error(&prefix))?;gate.prune(super::super::transfer_stage::now_secs());
        if claim.empty_owned{
            if gate.state==super::super::GateState::Open&&gate.epoch==claim.epoch{if gate.origin_url_fold!=claim.url{return Err(NarouError::Platform("creation origin changed".into()));}let owner=new_removal_owner(reservation.id(),true)?;gate.state=super::super::GateState::Closed;gate.epoch=next;gate.owner=Some(owner);}
            if gate.state==super::super::GateState::Closed&&gate.epoch==next{
                let owner=gate.owner.as_deref().ok_or_else(||NarouError::Platform("creation cleanup owner missing".into()))?;if super::super::access_fence::parse_removal_owner(owner)?!=(reservation.id(),true){return Err(NarouError::Platform("creation cleanup owner changed".into()));}
                if !gate.readers.is_empty(){return Err(NarouError::CreationCleanupPending);}
                let revision=gate.content_revision.checked_add(1).ok_or_else(||NarouError::Platform("content revision exhausted".into()))?;
                let mut bytes=objects.objects.lock();let before=bytes.len();bytes.retain(|key,_|!belongs(&claim.prefix,key));if bytes.len()!=before{gate.content_revision=revision;}drop(bytes);objects.verified.lock().retain(|key,_|!belongs(&claim.prefix,key));gate.origin_url_fold.clear();gate.state=super::super::GateState::Open;gate.owner=None;
            }
            if gate.state!=super::super::GateState::Open||gate.epoch!=next||!gate.origin_url_fold.is_empty(){return Err(NarouError::Platform("creation cleanup is not complete".into()));}
        }else{
            if gate.state==super::super::GateState::Open&&gate.epoch==claim.epoch&&gate.origin_url_fold==claim.url{gate.epoch=next;}
            if gate.state!=super::super::GateState::Open||gate.epoch!=next||gate.origin_url_fold!=claim.url{return Err(NarouError::Platform("retained creation epoch changed".into()));}
        }
        creation.claims.remove(&claim.prefix);allocator.release(reservation,false);Ok(())
    }
}
