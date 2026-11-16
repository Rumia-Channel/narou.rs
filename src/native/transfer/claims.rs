//! Active transfer identity claims share the ordinary metadata authority.
use std::{collections::{BTreeMap,BTreeSet},path::Path};
use rusqlite::{Connection,OptionalExtension};
use crate::{db::NovelRecord,error::{NarouError,Result},platform::NovelObjectKeys};
use super::TransferCommitMarker;

use crate::native::object_store::{NativeNamespaceKey,NativeNamespacePaths};

type PhysicalTarget=(NativeNamespacePaths,NativeNamespaceKey);
fn physical_target(root:&Path,prefix:&str)->Result<Option<PhysicalTarget>>{let Some(paths)=NativeNamespacePaths::new(root)?else{return Ok(None);};let key=paths.key(prefix)?;Ok(Some((paths,key)))}
fn physical_overlap(target:&Option<PhysicalTarget>,prefix:&str)->Result<bool>{match target{Some((paths,key))=>Ok(key.overlaps(&paths.key(prefix)?)),None=>Ok(false)}}
fn physical_collisions<'a>(root:&Path,namespaces:&BTreeSet<&'a str>)->Result<BTreeSet<&'a str>>{
    let Some(paths)=NativeNamespacePaths::new(root)?else{return Ok(BTreeSet::new());};let mut index:BTreeMap<NativeNamespaceKey,(&'a str,Vec<&'a str>)>=BTreeMap::new();for prefix in namespaces{match index.entry(paths.key(prefix)?){std::collections::btree_map::Entry::Vacant(entry)=>{entry.insert((*prefix,Vec::new()));},std::collections::btree_map::Entry::Occupied(mut entry)=>entry.get_mut().1.push(*prefix)}}
    let mut collisions=BTreeSet::new();for(key,(prefix,aliases))in &index{if !aliases.is_empty(){collisions.insert(*prefix);collisions.extend(aliases.iter().copied());}for ancestor in key.ancestor_keys(){if &ancestor==key{continue;}
        if let Some((other,other_aliases))=index.get(&ancestor){collisions.insert(*prefix);collisions.extend(aliases.iter().copied());collisions.insert(*other);collisions.extend(other_aliases.iter().copied());}}}Ok(collisions)
}
fn assert_physical_current_conn(connection:&Connection,id:crate::platform::NovelId,physical:&Option<PhysicalTarget>)->Result<()>{
    if physical.is_none(){return Ok(());}let mut statement=connection.prepare("SELECT b.prefix FROM novel_gate_bindings b JOIN novels n ON n.id=b.novel_id WHERE b.novel_id<>?1").map_err(crate::native::sqlite::sqlite_error)?;let mut rows=statement.query([id.0]).map_err(crate::native::sqlite::sqlite_error)?;while let Some(row)=rows.next().map_err(crate::native::sqlite::sqlite_error)?{let prefix:String=row.get(0).map_err(crate::native::sqlite::sqlite_error)?;if physical_overlap(physical,&prefix)?{return Err(NarouError::Platform("novel_busy: another current record overlaps the physical namespace".into()));}}Ok(())
}
/// Metadata-only; the caller retains the actual metadata authority and original
/// OS/epoch proof. None means fresh authoritative YAML, including compat mode.
pub(crate) fn assert_physical_current_namespace(inventory:&crate::db::inventory::Inventory,records:Option<&Connection>,id:crate::platform::NovelId,prefix:&crate::platform::ObjectPrefix)->Result<()>{
    let physical=physical_target(inventory.root_dir(),prefix.as_ref())?;if let Some(records)=records{return assert_physical_current_conn(records,id,&physical);}let path=inventory.inventory_path("database",crate::db::inventory::InventoryScope::Local);let raw=match std::fs::read_to_string(path){Ok(raw)=>raw,Err(error)if error.kind()==std::io::ErrorKind::NotFound=>return Ok(()),Err(error)=>return Err(error.into())};if !raw.trim().is_empty(){let current:BTreeMap<i64,RecordIdentity>=serde_yaml::from_str(&raw)?;for(current_id,record)in current{if current_id==id.0{continue;}let current=NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if physical_overlap(&physical,current.prefix().as_ref())?{return Err(NarouError::Platform("novel_busy: another current record overlaps the physical namespace".into()));}}}Ok(())
}

/// Reuse the caller's already-authoritative current rows without reloading YAML.
pub(crate) fn assert_physical_records(inventory:&crate::db::inventory::Inventory,records:&BTreeMap<i64,NovelRecord>,id:crate::platform::NovelId,prefix:&crate::platform::ObjectPrefix)->Result<()>{
    let physical=physical_target(inventory.root_dir(),prefix.as_ref())?;for(current_id,record)in records{if *current_id==id.0{continue;}let current=NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if physical_overlap(&physical,current.prefix().as_ref())?{return Err(NarouError::Platform("novel_busy: another current record overlaps the physical namespace".into()));}}Ok(())
}
struct CurrentNamespaceOrigin{url:String,legacy:Option<String>}
fn proven_virtual_legacy_origin(id:i64,ncode:Option<&str>,current_url:&str)->Option<String>{let ncode=ncode?;if !crate::virtual_novel::is_portable_toc_url(current_url){return None;}let expected=crate::virtual_novel::ncode(id);if ncode!=expected.as_str(){return None;}Some(format!("https://narou.rs/virtual/{expected}"))}
fn repair_virtual_legacy_origin(root:&Path,connection:&Connection,prefix:&crate::platform::ObjectPrefix,epoch:u64,expected:&str,current:&str)->Result<()>{
    let sql:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='novel_access_gates')",[],|row|row.get(0)).map_err(crate::native::sqlite::sqlite_error)?;let sql=sql&&(root.as_os_str().is_empty()||crate::native::sqlite::state::active_for(&root.join(".narou")).is_some());if sql{let epoch=i64::try_from(epoch).map_err(|_|NarouError::Platform("namespace epoch exceeds SQLite integer range".into()))?;let changed=connection.execute("UPDATE novel_access_gates SET origin_url_fold=?1 WHERE prefix=?2 AND epoch=?3 AND closed=0 AND origin_url_fold=?4",rusqlite::params![current,prefix.as_ref().trim_end_matches('/'),epoch,expected]).map_err(crate::native::sqlite::sqlite_error)?;if changed!=1{return Err(NarouError::Platform("novel_busy: affected legacy origin lost its original OPEN epoch".into()));}Ok(())}else{crate::native::object_store::rebind_fs_gate_origin_locked(root,prefix,epoch,expected,current)}
}

fn epoch_from_sql(row:&rusqlite::Row<'_>,column:usize)->rusqlite::Result<u64>{
    let epoch=row.get::<_,i64>(column)?;
    u64::try_from(epoch).map_err(|error|rusqlite::Error::FromSqlConversionFailure(column,rusqlite::types::Type::Integer,Box::new(error)))
}

pub(super) const SCHEMA:&str="CREATE TABLE IF NOT EXISTS native_transfer_target_claims(prefix TEXT PRIMARY KEY,folded_url TEXT NOT NULL UNIQUE,owner TEXT NOT NULL UNIQUE,owner_kind TEXT NOT NULL CHECK(owner_kind IN ('transfer','ordinary')),generation INTEGER,epoch INTEGER NOT NULL CHECK(epoch>=0),liveness_key TEXT,empty_owned INTEGER NOT NULL DEFAULT 0 CHECK(empty_owned IN(0,1)),session_id TEXT NOT NULL,publication_id INTEGER NOT NULL,plan_revision TEXT NOT NULL,novel_id INTEGER NOT NULL UNIQUE,CHECK(owner_kind='transfer' OR (generation IS NOT NULL AND liveness_key IS NOT NULL)))";
struct Claim {prefix:String,url:String,owner:String,kind:String,generation:Option<i64>,epoch:u64,liveness:Option<String>,session:String,publication:u32,id:i64}
pub(crate) struct RecordClaimGuard {root:std::path::PathBuf,claims:Vec<Claim>,gates:BTreeMap<String,NamespaceGate>,unchanged:BTreeSet<[u8;32]>,portable_uris:BTreeMap<i64,String>,virtual_rebinds:BTreeMap<i64,(String,String)>,publication:Option<(String,u32,i64)>,authorized:BTreeSet<usize>}
fn identity(record:&NovelRecord)->Result<(String,String)>{Ok((crate::db::novel_codec::fold(&record.toc_url),NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?.prefix().as_ref().to_string()))}
fn fingerprint(id:i64,url:&str,prefix:&str)->[u8;32]{use sha2::Digest;let mut hash=sha2::Sha256::new();hash.update(id.to_le_bytes());hash.update((url.len() as u64).to_le_bytes());hash.update(url.as_bytes());hash.update(prefix.as_bytes());hash.finalize().into()}

#[derive(PartialEq,Eq)]
struct NamespaceGate {closed:bool,owner:Option<String>,origin:String,epoch:u64}
fn gate_blocks_namespace(gate_prefix:&str,closed:bool,owner:Option<&str>,origin:&str,prefix:&str,url:&str,allowed_owner:Option<&str>)->bool{
    let gate_prefix=gate_prefix.trim_end_matches('/');if !crate::platform::access_fence::namespace_prefixes_overlap(gate_prefix,prefix){return false;}
    if gate_prefix!=prefix{return closed||!origin.is_empty();}
    if !origin.is_empty()&&origin!=url{return true;}
    closed&&!owner.is_some_and(|owner|!owner.starts_with("remove-v1:")&&Some(owner)==allowed_owner)
}
fn assert_gate_namespace(root:&Path,connection:&Connection,prefix:&str,url:&str,physical:&Option<PhysicalTarget>)->Result<()>{
    crate::native::object_store::visit_namespace_gates(root,connection,|gate_prefix,closed,owner,_epoch,origin|{if gate_blocks_namespace(gate_prefix,closed,owner,origin,prefix,url,None)||(gate_prefix.trim_end_matches('/')!=prefix&&(closed||!origin.is_empty())&&physical_overlap(physical,gate_prefix)?){return Err(NarouError::Platform("novel_busy: destination overlaps a closed or retained namespace".into()));}Ok(())})
}
fn namespace_gates(root:&Path,connection:&Connection)->Result<BTreeMap<String,NamespaceGate>>{
    let mut gates=BTreeMap::new();crate::native::object_store::visit_namespace_gates(root,connection,|prefix,closed,owner,epoch,origin|{if !closed&&origin.is_empty(){return Ok(());}let gate=NamespaceGate{closed,owner:owner.map(str::to_owned),origin:origin.to_owned(),epoch};match gates.entry(prefix.trim_end_matches('/').to_owned()){std::collections::btree_map::Entry::Vacant(entry)=>{entry.insert(gate);},std::collections::btree_map::Entry::Occupied(entry)=>{if entry.get()!=&gate{return Err(NarouError::Platform("namespace gate snapshots disagree".into()));}}}Ok(())})?;Ok(gates)
}
fn gates_block_namespace(gates:&BTreeMap<String,NamespaceGate>,prefix:&str,url:&str,allowed_owner:Option<&str>)->bool{
    if gates.get(prefix).is_some_and(|gate|gate_blocks_namespace(prefix,gate.closed,gate.owner.as_deref(),&gate.origin,prefix,url,allowed_owner)){return true;}for(index,_)in prefix.match_indices('/'){if gates.contains_key(&prefix[..index]){return true;}}let lower=format!("{prefix}/");gates.range::<str,_>((std::ops::Bound::Included(lower.as_str()),std::ops::Bound::Unbounded)).next().is_some_and(|(candidate,_)|candidate.starts_with(&lower))
}

/// Retain fingerprints only for identities already colliding in the captured
/// metadata. Index scalar identity columns, never clone NovelRecord payloads.
fn current_record_collisions(root:&Path,records:&BTreeMap<i64,NovelRecord>,claims:&[Claim],gates:&BTreeMap<String,NamespaceGate>)->Result<BTreeSet<[u8;32]>>{
    let mut urls=BTreeMap::new();let mut prefixes:BTreeMap<String,([u8;32],Vec<[u8;32]>)>=BTreeMap::new();let mut collisions=BTreeSet::new();
    for(id,record)in records{let(url,prefix)=identity(record)?;let digest=fingerprint(*id,&url,&prefix);if let Some(previous)=urls.insert(url,digest){collisions.insert(previous);collisions.insert(digest);}match prefixes.entry(prefix){std::collections::btree_map::Entry::Vacant(entry)=>{entry.insert((digest,Vec::new()));},std::collections::btree_map::Entry::Occupied(mut entry)=>entry.get_mut().1.push(digest)}}
    let mut overlapping=BTreeSet::new();for(prefix,(_,duplicates))in &prefixes{if !duplicates.is_empty(){overlapping.insert(prefix.as_str());}for(index,_)in prefix.match_indices('/'){let ancestor=&prefix[..index];if prefixes.contains_key(ancestor){overlapping.insert(ancestor);overlapping.insert(prefix.as_str());}}}
    let mut namespaces=BTreeSet::new();for prefix in prefixes.keys(){namespaces.insert(prefix.as_str());}for claim in claims{namespaces.insert(claim.prefix.as_str());}for prefix in gates.keys(){namespaces.insert(prefix.as_str());}for prefix in physical_collisions(root,&namespaces)?{if prefixes.contains_key(prefix){overlapping.insert(prefix);}}
    for prefix in overlapping{let(first,duplicates)=prefixes.get(prefix).expect("prefix was collected from this index");collisions.insert(*first);collisions.extend(duplicates.iter().copied());}Ok(collisions)
}
impl RecordClaimGuard {
    pub(crate) fn authorize_ordinary(&mut self,id:i64,generation:i64,prefix:&crate::platform::ObjectPrefix,epoch:u64)->Result<()>{
        let owner=format!("ordinary:{id}:{generation}:{epoch}");let liveness=format!("{id}-{generation}");let index=self.claims.iter().position(|claim|claim.kind=="ordinary"&&claim.id==id&&claim.generation==Some(generation)&&claim.prefix==prefix.as_ref().trim_end_matches('/')&&claim.epoch==epoch&&claim.owner==owner&&claim.liveness.as_deref()==Some(liveness.as_str())).ok_or_else(||NarouError::Platform("novel_recovering: exact ordinary create claim is missing".into()))?;self.authorized.insert(index);Ok(())
    }
    /// Only the original-E0 virtual CAS primitive may call this, while it owns
    /// the actual metadata/OS authority and has proved any legacy-origin repair.
    /// Updating this captured view does not persist origin before metadata.
    pub(crate) fn authorize_virtual_rebind(&mut self,before:&BTreeMap<i64,NovelRecord>,fence:&crate::platform::WriteFence,id:crate::platform::NovelId,observed:&str,candidate:&str)->Result<()>{
        let record=before.get(&id.0).ok_or_else(||NarouError::Platform("virtual identity record disappeared".into()))?;let prefix=fence.prefix.as_ref().trim_end_matches('/');let ncode=crate::virtual_novel::ncode(id.0);let legacy=format!("https://narou.rs/virtual/{ncode}");if record.ncode.as_deref()!=Some(ncode.as_str())||NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?.prefix().as_ref()!=prefix||!crate::virtual_novel::is_portable_toc_url(candidate)||(record.toc_url!=observed&&record.toc_url!=candidate)||(crate::virtual_novel::is_portable_toc_url(&record.toc_url)&&record.toc_url!=candidate)||(observed!=candidate&&crate::db::novel_codec::fold(observed)!=legacy){return Err(NarouError::Platform("identity_conflict: virtual URI rebind does not match the exact current row".into()));}
        let observed=crate::db::novel_codec::fold(observed);let candidate_folded=crate::db::novel_codec::fold(candidate);if let Some(gate)=self.gates.get_mut(prefix){if gate.closed||gate.epoch!=fence.epoch||(!gate.origin.is_empty()&&gate.origin!=observed&&gate.origin!=candidate_folded){return Err(NarouError::Platform("novel_busy: virtual rebind lost its original open origin".into()));}gate.origin=candidate_folded;}
        self.portable_uris.insert(id.0,candidate.to_owned());self.virtual_rebinds.insert(id.0,(prefix.to_owned(),ncode));Ok(())
    }
    pub(crate) fn validate(&self,after:&BTreeMap<i64,NovelRecord>)->Result<()>{
        for(id,(prefix,ncode))in &self.virtual_rebinds{let record=after.get(id).ok_or_else(||NarouError::Platform("virtual identity rebind removed its current row".into()))?;if record.ncode.as_deref()!=Some(ncode.as_str())||NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?.prefix().as_ref()!=prefix.as_str(){return Err(NarouError::Platform("identity_conflict: virtual rebind moved its exact current prefix or ncode".into()));}}
        for(id,uri)in &self.portable_uris{if after.get(id).is_some_and(|record|record.toc_url!=*uri){return Err(NarouError::Platform("identity_conflict: a portable virtual URI is immutable".into()));}}
        if current_record_collisions(&self.root,after,&self.claims,&self.gates)?.iter().any(|digest|!self.unchanged.contains(digest)){return Err(NarouError::Platform("identity_conflict: a changed ID overlaps another current URL or namespace".into()));}
        for(id,record)in after{let(url,prefix)=identity(record)?;let fingerprint=fingerprint(*id,&url,&prefix);
            let publication_owner=self.publication.as_ref().and_then(|(session,publication,assigned)|self.claims.iter().find(|claim|claim.kind=="transfer"&&claim.session==*session&&claim.publication==*publication&&claim.id==*assigned&&claim.id==*id&&claim.prefix==prefix&&claim.url==url).map(|claim|claim.owner.as_str()));if !self.unchanged.contains(&fingerprint)&&gates_block_namespace(&self.gates,&prefix,&url,publication_owner){return Err(NarouError::Platform("novel_busy: changed identity overlaps a closed or retained namespace".into()));}
            for(index,claim)in self.claims.iter().enumerate(){if claim.id!=*id&&claim.url!=url&&!crate::platform::access_fence::namespace_prefixes_overlap(&claim.prefix,&prefix){continue;}
                if self.unchanged.contains(&fingerprint){continue;}
                if claim.kind=="ordinary"&&self.authorized.contains(&index)&&claim.id==*id&&claim.url==url&&claim.prefix==prefix{continue;}
                if claim.kind=="transfer"&&claim.url==url&&claim.prefix==prefix&&self.publication.as_ref().is_some_and(|(session,publication,assigned)|session==&claim.session&&*publication==claim.publication&&*assigned==*id&&claim.id==*id){continue;}
                return Err(NarouError::Platform("novel_recovering: an active claim owns the destination ID, URL or prefix".into()));
            }
        }Ok(())
    }
}
pub(crate) fn record_claim_guard_conn(root:&Path,connection:&Connection,before:&BTreeMap<i64,NovelRecord>,marker:Option<&TransferCommitMarker>)->Result<RecordClaimGuard>{
    let exists:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='native_transfer_target_claims')",[],|row|row.get(0)).map_err(crate::native::sqlite::sqlite_error)?;
    let mut claims=Vec::new();if exists{let mut statement=connection.prepare("SELECT prefix,folded_url,owner,owner_kind,generation,epoch,liveness_key,session_id,publication_id,novel_id FROM native_transfer_target_claims ORDER BY owner").map_err(crate::native::sqlite::sqlite_error)?;let rows=statement.query_map([],|row|Ok(Claim{prefix:row.get(0)?,url:row.get(1)?,owner:row.get(2)?,kind:row.get(3)?,generation:row.get(4)?,epoch:epoch_from_sql(row,5)?,liveness:row.get(6)?,session:row.get(7)?,publication:row.get(8)?,id:row.get(9)?})).map_err(crate::native::sqlite::sqlite_error)?;for row in rows{claims.push(row.map_err(crate::native::sqlite::sqlite_error)?);}}
    let mut gates=namespace_gates(root,connection)?;for(id,record)in before{if let Some(legacy)=proven_virtual_legacy_origin(*id,record.ncode.as_deref(),&record.toc_url){let prefix=NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if let Some(gate)=gates.get_mut(prefix.prefix().as_ref())&&gate.origin==legacy{gate.origin=crate::db::novel_codec::fold(&record.toc_url);}}}let mut unchanged=current_record_collisions(root,before,&claims,&gates)?;if !claims.is_empty()||!gates.is_empty(){for(id,record)in before{let(url,prefix)=identity(record)?;if claims.iter().any(|claim|claim.id==*id||claim.url==url||crate::platform::access_fence::namespace_prefixes_overlap(&claim.prefix,&prefix))||gates_block_namespace(&gates,&prefix,&url,None){unchanged.insert(fingerprint(*id,&url,&prefix));}}}
    let publication=marker.map(|marker|{let receipt:crate::application::transfer::PublicationReceipt=serde_json::from_slice(&marker.receipt).map_err(|error|NarouError::Platform(error.to_string()))?;let id=marker.novel_id.0;if receipt.assigned_id!=Some(id)||receipt.novel_ordinal!=marker.publication_id||!matches!((receipt.action,receipt.outcome.as_str()),(crate::application::transfer::PlanAction::Create,"created")|(crate::application::transfer::PlanAction::Overwrite,"overwritten")){return Err(NarouError::Platform("transfer commit receipt identity mismatch".into()));}Ok((marker.session_id.clone(),marker.publication_id,id))}).transpose()?;
    let portable_uris=before.iter().filter(|(_,record)|crate::virtual_novel::is_portable_toc_url(&record.toc_url)).map(|(id,record)|(*id,record.toc_url.clone())).collect();Ok(RecordClaimGuard{root:root.to_path_buf(),claims,gates,unchanged,portable_uris,virtual_rebinds:BTreeMap::new(),publication,authorized:BTreeSet::new()})
}

#[derive(serde::Deserialize)]
pub(super) struct RecordIdentity {pub toc_url:String,pub sitename:String,pub file_title:String,#[serde(default)]ncode:Option<String>,#[serde(default)]pub use_subdirectory:bool}
pub(super) fn assert_current_identity(inventory:&crate::db::inventory::Inventory,connection:Option<&Connection>,control:&Connection,id:i64,url:&str,prefix:&str,create:bool)->Result<()>{
    let folded=crate::db::novel_codec::fold(url);
    let physical=physical_target(inventory.root_dir(),prefix)?;assert_gate_namespace(inventory.root_dir(),control,prefix,&folded,&physical)?;
    if let Some(connection)=connection{
        assert_physical_current_conn(connection,crate::platform::NovelId(id),&physical)?;let exists=crate::db::Database::assert_current_identity_conn(connection,crate::platform::NovelId(id),&crate::platform::ObjectPrefix::new(prefix)?,&folded)?;
        if create&&exists{return Err(NarouError::Platform("plan_stale: reserved ID was already published".into()));}
        if !create&&!exists{return Err(NarouError::Platform("plan_stale: publication target was deleted".into()));}Ok(())
    }else{
        let mut found=false;let mut check=|current_id:i64,current:RecordIdentity|->Result<()>{let current_url=crate::db::novel_codec::fold(&current.toc_url);let current_prefix=NovelObjectKeys::new(&current.sitename,&current.file_title,current.use_subdirectory)?.prefix().as_ref().to_string();if current_id==id{found=current_url==folded&&current_prefix==prefix;if create||!found{return Err(NarouError::Platform("plan_stale: reserved ID or target identity changed".into()));}}else if current_url==folded||crate::platform::access_fence::namespace_prefixes_overlap(&current_prefix,prefix)||physical_overlap(&physical,&current_prefix)?{return Err(NarouError::Platform("plan_stale: destination identity was created since planning".into()));}Ok(())};let path=inventory.inventory_path("database",crate::db::inventory::InventoryScope::Local);match std::fs::read_to_string(path){Ok(yaml)if !yaml.trim().is_empty()=>{let records:BTreeMap<i64,RecordIdentity>=serde_yaml::from_str(&yaml)?;for(id,record)in records{check(id,record)?;}},Ok(_)=>{},Err(error)if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(error.into())}if !create&&!found{return Err(NarouError::Platform("plan_stale: publication target was deleted".into()));}Ok(())
    }
}

/// Read only namespace identity columns under the caller's metadata authority;
/// source and removal callers apply their own access capability afterwards.
fn current_namespace_origin(inventory:&crate::db::inventory::Inventory,records:Option<&Connection>,id:crate::platform::NovelId,prefix:&crate::platform::ObjectPrefix)->Result<Option<CurrentNamespaceOrigin>>{
    let physical=physical_target(inventory.root_dir(),prefix.as_ref())?;
    if let Some(records)=records{let exists=crate::db::Database::assert_current_namespace_conn(records,id,prefix)?;assert_physical_current_conn(records,id,&physical)?;if !exists{return Ok(None);}let(url,ncode):(String,Option<String>)=records.query_row("SELECT toc_url,ncode FROM novels WHERE id=?1",[id.0],|row|Ok((row.get(0)?,row.get(1)?))).map_err(crate::native::sqlite::sqlite_error)?;return Ok(Some(CurrentNamespaceOrigin{legacy:proven_virtual_legacy_origin(id.0,ncode.as_deref(),&url),url:crate::db::novel_codec::fold(&url)}));}
    let path=inventory.inventory_path("database",crate::db::inventory::InventoryScope::Local);let raw=match std::fs::read_to_string(path){Ok(raw)=>raw,Err(error)if error.kind()==std::io::ErrorKind::NotFound=>String::new(),Err(error)=>return Err(error.into())};let mut own=None;if !raw.trim().is_empty(){let current:BTreeMap<i64,RecordIdentity>=serde_yaml::from_str(&raw)?;for(current_id,record)in current{let current_prefix=NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?.prefix().as_ref().to_owned();if current_id==id.0{if current_prefix!=prefix.as_ref().trim_end_matches('/'){return Err(NarouError::Platform("identity_conflict: current record moved to another prefix".into()));}own=Some(CurrentNamespaceOrigin{legacy:proven_virtual_legacy_origin(current_id,record.ncode.as_deref(),&record.toc_url),url:crate::db::novel_codec::fold(&record.toc_url)});}else if crate::platform::access_fence::namespace_prefixes_overlap(&current_prefix,prefix.as_ref())||physical_overlap(&physical,&current_prefix)?{return Err(NarouError::Platform("novel_busy: another current record overlaps the namespace".into()));}}}Ok(own)
}

pub(super) fn assert_source_namespace(inventory:&crate::db::inventory::Inventory,records:Option<&Connection>,control:&Connection,id:crate::platform::NovelId,prefix:&crate::platform::ObjectPrefix,folded_url:&str)->Result<()>{
    let own=current_namespace_origin(inventory,records,id,prefix)?;if own.as_ref().map(|current|current.url.as_str())!=Some(folded_url)||folded_url.is_empty(){return Err(NarouError::Platform("source namespace identity changed; refresh the selection".into()));}let prefix=prefix.as_ref().trim_end_matches('/');let physical=physical_target(inventory.root_dir(),prefix)?;crate::native::object_store::visit_namespace_gates(inventory.root_dir(),control,|gate_prefix,closed,owner,_epoch,origin|{let gate_prefix=gate_prefix.trim_end_matches('/');if gate_prefix==prefix{if (!origin.is_empty()&&origin!=folded_url&&own.as_ref().and_then(|current|current.legacy.as_deref())!=Some(origin))||(closed&&owner.is_some_and(|owner|owner.starts_with("remove-v1:"))){return Err(NarouError::Platform("source namespace is retained by another identity or being removed".into()));}}else if (closed||!origin.is_empty())&&(crate::platform::access_fence::namespace_prefixes_overlap(gate_prefix,prefix)||physical_overlap(&physical,gate_prefix)?){return Err(NarouError::Platform("ambiguous source namespace: resolve overlapping saved folders before export".into()));}Ok(())})
}


/// Missing metadata never grants access: require a live, exact retained origin.
pub(super) fn assert_retained_source_namespace(inventory:&crate::db::inventory::Inventory,records:Option<&Connection>,control:&Connection,prefix:&crate::platform::ObjectPrefix)->Result<()>{
    let prefix=prefix.as_ref().trim_end_matches('/');let physical=physical_target(inventory.root_dir(),prefix)?;let conflict=|other:&str|->Result<bool>{Ok(crate::platform::access_fence::namespace_prefixes_overlap(other,prefix)||physical_overlap(&physical,other)?)};
    if let Some(records)=records{let mut statement=records.prepare("SELECT b.prefix FROM novel_gate_bindings b JOIN novels n ON n.id=b.novel_id").map_err(crate::native::sqlite::sqlite_error)?;let mut rows=statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;while let Some(row)=rows.next().map_err(crate::native::sqlite::sqlite_error)?{let other:String=row.get(0).map_err(crate::native::sqlite::sqlite_error)?;if conflict(&other)?{return Err(NarouError::Platform("novel_busy: retained backup source overlaps a current record".into()));}}}else{let path=inventory.inventory_path("database",crate::db::inventory::InventoryScope::Local);let raw=match std::fs::read_to_string(path){Ok(raw)=>raw,Err(error)if error.kind()==std::io::ErrorKind::NotFound=>String::new(),Err(error)=>return Err(error.into())};if !raw.trim().is_empty(){let current:BTreeMap<i64,RecordIdentity>=serde_yaml::from_str(&raw)?;for record in current.values(){let other=NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if conflict(other.prefix().as_ref())?{return Err(NarouError::Platform("novel_busy: retained backup source overlaps a current record".into()));}}}}
    let mut origin=None;crate::native::object_store::visit_namespace_gates(inventory.root_dir(),control,|other,closed,_owner,_epoch,retained|{let other=other.trim_end_matches('/');if other==prefix{if closed||retained.is_empty(){return Err(NarouError::Platform("novel_busy: retained source lacks an OPEN nonempty origin".into()));}
        if origin.as_deref().is_some_and(|previous|previous!=retained){return Err(NarouError::Platform("retained source origin snapshots disagree".into()));}origin=Some(retained.to_owned());}else if (closed||!retained.is_empty())&&conflict(other)?{return Err(NarouError::Platform("novel_busy: retained source overlaps another protected namespace".into()));}Ok(())})?;if origin.is_none(){return Err(NarouError::Platform("retained source has no durable origin proof".into()));}
    let exists:bool=control.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='native_transfer_target_claims')",[],|row|row.get(0)).map_err(crate::native::sqlite::sqlite_error)?;if exists{let mut statement=control.prepare("SELECT prefix FROM native_transfer_target_claims").map_err(crate::native::sqlite::sqlite_error)?;let mut rows=statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;while let Some(row)=rows.next().map_err(crate::native::sqlite::sqlite_error)?{let other:String=row.get(0).map_err(crate::native::sqlite::sqlite_error)?;if conflict(&other)?{return Err(NarouError::Platform("novel_busy: retained source overlaps an unpublished active claim".into()));}}}Ok(())
}

/// The caller holds metadata and the original OS lock through CLOSED persist.
pub(crate) fn assert_removal_identity(inventory:&crate::db::inventory::Inventory,records:Option<&Connection>,control:&Connection,fence:&crate::platform::WriteFence,id:crate::platform::NovelId)->Result<()>{
    let prefix=fence.prefix.as_ref().trim_end_matches('/');let current=current_namespace_origin(inventory,records,id,&fence.prefix)?;let mut own_url=current.as_ref().map(|current|current.url.clone());let mut retained_origin=None;let physical=physical_target(inventory.root_dir(),prefix)?;
    crate::native::object_store::visit_namespace_gates(inventory.root_dir(),control,|gate_prefix,closed,_owner,_epoch,origin|{let gate_prefix=gate_prefix.trim_end_matches('/');if gate_prefix==prefix{if !origin.is_empty(){if retained_origin.as_deref().is_some_and(|previous|previous!=origin){return Err(NarouError::Platform("namespace origin snapshots disagree".into()));}retained_origin=Some(origin.to_owned());}}else if (closed||!origin.is_empty())&&(crate::platform::access_fence::namespace_prefixes_overlap(gate_prefix,prefix)||physical_overlap(&physical,gate_prefix)?){return Err(NarouError::Platform("novel_busy: another retained or closed namespace overlaps removal".into()));}Ok(())})?;
    let exists:bool=control.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='native_transfer_target_claims')",[],|row|row.get(0)).map_err(crate::native::sqlite::sqlite_error)?;if exists{let mut statement=control.prepare("SELECT prefix,novel_id,owner_kind,epoch,folded_url FROM native_transfer_target_claims").map_err(crate::native::sqlite::sqlite_error)?;let mut rows=statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;
        while let Some(row)=rows.next().map_err(crate::native::sqlite::sqlite_error)?{let claimed_prefix:String=row.get(0).map_err(crate::native::sqlite::sqlite_error)?;if !crate::platform::access_fence::namespace_prefixes_overlap(&claimed_prefix,prefix)&&!physical_overlap(&physical,&claimed_prefix)?{continue;}let claimed_id:i64=row.get(1).map_err(crate::native::sqlite::sqlite_error)?;let kind:String=row.get(2).map_err(crate::native::sqlite::sqlite_error)?;let epoch=epoch_from_sql(row,3).map_err(crate::native::sqlite::sqlite_error)?;let url:String=row.get(4).map_err(crate::native::sqlite::sqlite_error)?;if kind!="ordinary"||claimed_id!=id.0||claimed_prefix!=prefix||epoch!=fence.epoch{return Err(NarouError::Platform("novel_busy: an active foreign claim protects the removal namespace".into()));}
            if own_url.as_deref().is_some_and(|current|current!=url){return Err(NarouError::Platform("identity_conflict: pending removal origin differs from current record".into()));}own_url=Some(url);}
    }
    if let Some(url)=own_url{if url.is_empty(){return Err(NarouError::Platform("identity_conflict: removal origin is empty".into()));}
        if let Some(legacy)=current.as_ref().and_then(|current|current.legacy.as_deref())&&retained_origin.as_deref()==Some(legacy){repair_virtual_legacy_origin(inventory.root_dir(),control,&fence.prefix,fence.epoch,legacy,&url)?;}crate::native::object_store::bind_namespace_origin(inventory.root_dir(),control,&fence.prefix,&url)?;}else if retained_origin.is_none(){return Err(NarouError::Platform("identity_conflict: removal has no proven current or pending origin".into()));}Ok(())
}

fn require_available_namespace(root:&Path,connection:&Connection,owner:&str,prefix:&str)->Result<()>{
    let mut ancestors=prefix.match_indices('/').map(|(index,_)|&prefix[..index]).collect::<Vec<_>>();ancestors.push(prefix);let ancestors=serde_json::to_string(&ancestors).map_err(|error|NarouError::Platform(error.to_string()))?;
    let mut statement=connection.prepare("SELECT prefix FROM native_transfer_target_claims WHERE owner<>?1 AND (prefix IN (SELECT value FROM json_each(?2)) OR (prefix>=?3 AND prefix<?4)) LIMIT 1").map_err(crate::native::sqlite::sqlite_error)?;
    let overlap:Option<String>=statement.query_row(rusqlite::params![owner,ancestors,format!("{prefix}/"),format!("{prefix}0")],|row|row.get(0)).optional().map_err(crate::native::sqlite::sqlite_error)?;if overlap.is_some(){return Err(NarouError::Platform("novel_recovering: an active claim overlaps the destination namespace".into()));}let physical=physical_target(root,prefix)?;if physical.is_some(){let mut statement=connection.prepare("SELECT prefix FROM native_transfer_target_claims WHERE owner<>?1").map_err(crate::native::sqlite::sqlite_error)?;let mut rows=statement.query([owner]).map_err(crate::native::sqlite::sqlite_error)?;while let Some(row)=rows.next().map_err(crate::native::sqlite::sqlite_error)?{let claimed:String=row.get(0).map_err(crate::native::sqlite::sqlite_error)?;if physical_overlap(&physical,&claimed)?{return Err(NarouError::Platform("novel_recovering: an active claim overlaps the physical destination".into()));}}}Ok(())
}

/// Borrowed publication-claim identity: `owner` is the derived unique claim
/// owner; session/publication/revision bind the durable row to the plan.
pub(super) struct PublicationClaim<'a>{pub owner:&'a str,pub session:&'a str,pub publication:u32,pub revision:&'a str}

pub(super) fn claim(root:&Path,connection:&Connection,claim:&PublicationClaim<'_>,id:i64,url:&str,prefix:&str)->Result<()>{
    let prior:Option<(String,String,i64)>=connection.query_row("SELECT prefix,folded_url,novel_id FROM native_transfer_target_claims WHERE owner=?1",[claim.owner],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(crate::native::sqlite::sqlite_error)?;let folded=crate::db::novel_codec::fold(url);if let Some((old_prefix,old_url,old_id))=prior{if old_prefix==prefix&&old_url==folded&&old_id==id{return Ok(());}return Err(NarouError::Platform("plan_stale: publication owner identity changed".into()));}
    require_available_namespace(root,connection,claim.owner,prefix)?;
    connection.execute("INSERT INTO native_transfer_target_claims(prefix,folded_url,owner,owner_kind,generation,epoch,liveness_key,session_id,publication_id,plan_revision,novel_id) VALUES(?1,?2,?3,'transfer',NULL,0,NULL,?4,?5,?6,?7)",rusqlite::params![prefix,folded,claim.owner,claim.session,claim.publication,claim.revision,id]).map_err(|error|NarouError::Platform(format!("novel_recovering: destination identity is claimed: {error}")))?;Ok(())
}

/// Caller proves OPEN E0, the exact numeric generation, and original slot
/// ownership under the common metadata and OS authority. A same-owner replay
/// retains its first empty-slot proof, even after its own writes create files.
pub(crate) fn reserve_ordinary_claim_conn(root:&Path,connection:&Connection,fence:&crate::platform::WriteFence,reservation:&crate::platform::NovelIdReservation,toc_url:&str,empty_owned:bool)->Result<()>{
    connection.execute_batch(SCHEMA).map_err(crate::native::sqlite::sqlite_error)?;let id=reservation.id().0;let generation=reservation.generation();let epoch=i64::try_from(fence.epoch).map_err(|_|NarouError::Platform("ordinary claim epoch overflow".into()))?;let owner=format!("ordinary:{id}:{generation}:{}",fence.epoch);let liveness=format!("{id}-{generation}");let prefix=fence.prefix.as_ref().trim_end_matches('/');let folded=crate::db::novel_codec::fold(toc_url);
    let physical=physical_target(root,prefix)?;assert_gate_namespace(root,connection,prefix,&folded,&physical)?;
    let previous:Option<(String,String,i64,i64,i64,String)>=connection.query_row("SELECT prefix,folded_url,novel_id,generation,epoch,liveness_key FROM native_transfer_target_claims WHERE owner=?1 AND owner_kind='ordinary'",[&owner],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).optional().map_err(crate::native::sqlite::sqlite_error)?;if let Some((old_prefix,old_url,old_id,old_generation,old_epoch,old_liveness))=previous{if old_prefix==prefix&&old_url==folded&&old_id==id&&old_generation==generation&&old_epoch==epoch&&old_liveness==liveness{return Ok(());}return Err(NarouError::Platform("novel_recovering: ordinary claim identity changed".into()));}
    require_available_namespace(root,connection,&owner,prefix)?;
    connection.execute("INSERT INTO native_transfer_target_claims(prefix,folded_url,owner,owner_kind,generation,epoch,liveness_key,empty_owned,session_id,publication_id,plan_revision,novel_id) VALUES(?1,?2,?3,'ordinary',?4,?5,?6,?7,'',0,'',?8)",rusqlite::params![prefix,folded,owner,generation,epoch,liveness,i64::from(empty_owned),id]).map_err(|error|NarouError::Platform(format!("novel_recovering: destination identity is claimed: {error}")))?;Ok(())
}
pub(crate) fn consume_ordinary_claim_conn(connection:&Connection,id:i64,generation:i64,prefix:&crate::platform::ObjectPrefix,epoch:u64)->Result<()>{
    let owner=format!("ordinary:{id}:{generation}:{epoch}");let epoch=i64::try_from(epoch).map_err(|_|NarouError::Platform("ordinary claim epoch overflow".into()))?;let changed=connection.execute("DELETE FROM native_transfer_target_claims WHERE owner_kind='ordinary' AND owner=?1 AND novel_id=?2 AND generation=?3 AND prefix=?4 AND epoch=?5 AND liveness_key=?6",rusqlite::params![owner,id,generation,prefix.as_ref().trim_end_matches('/'),epoch,format!("{id}-{generation}")]).map_err(crate::native::sqlite::sqlite_error)?;if changed!=1{return Err(NarouError::Platform("novel_recovering: exact ordinary claim cannot be consumed".into()));}Ok(())
}
/// A delayed release cannot delete a recycled ID's newer namespace claim.
pub(crate) fn release_ordinary_claim_conn(connection:&Connection,id:i64,generation:i64)->Result<()>{connection.execute_batch(SCHEMA).map_err(crate::native::sqlite::sqlite_error)?;connection.execute("DELETE FROM native_transfer_target_claims WHERE owner_kind='ordinary' AND novel_id=?1 AND generation=?2 AND liveness_key=?3",rusqlite::params![id,generation,format!("{id}-{generation}")]).map_err(crate::native::sqlite::sqlite_error)?;Ok(())}

/// Called under the backup's actual metadata authority before it strips private
/// allocator rows. Retained preexisting files cannot be excluded as a shortcut.
pub(crate) fn ensure_no_unpublished_creations_conn(connection:&Connection,mut is_registered:impl FnMut(i64)->bool)->Result<()>{
    let exists:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='native_transfer_target_claims')",[],|row|row.get(0)).map_err(crate::native::sqlite::sqlite_error)?;if !exists{return Ok(());}
    let mut statement=connection.prepare("SELECT novel_id,prefix FROM native_transfer_target_claims WHERE owner_kind='ordinary' ORDER BY novel_id").map_err(crate::native::sqlite::sqlite_error)?;let mut rows=statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;
    while let Some(row)=rows.next().map_err(crate::native::sqlite::sqlite_error)?{let id:i64=row.get(0).map_err(crate::native::sqlite::sqlite_error)?;if !is_registered(id){let prefix:String=row.get(1).map_err(crate::native::sqlite::sqlite_error)?;return Err(NarouError::Platform(format!("novel_busy: unpublished creation {id} owns {prefix}; finish or recover it before backup")));}}
    Ok(())
}

#[cfg(test)]
#[path="claim_tests.rs"]
mod tests;
