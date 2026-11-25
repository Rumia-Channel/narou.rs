//! Origin changes preserve every other durable JSON field verbatim.
use super::*;
use serde_json::value::RawValue;
use std::collections::BTreeMap;
fn json_error(error:serde_json::Error)->NarouError{NarouError::Platform(error.to_string())}
fn load(root:&Path,prefix:&ObjectPrefix)->Result<Option<(PathBuf,Vec<u8>)>>{
    if root.as_os_str().is_empty(){return Err(NarouError::Platform("filesystem gate has no native library root".into()));}
    let archive=root.join(crate::downloader::types::ARCHIVE_ROOT_DIR);let path=archive.join(".objects/.narou-transfer/.gate").join(hex::encode(sha2::Sha256::digest(prefix.as_ref().trim_end_matches('/').as_bytes())));
    if !path.try_exists()?{return Ok(None);}let path=crate::db::paths::ensure_within_archive_root(&path,&archive)?;match fs::read(&path){Ok(bytes)=>Ok(Some((path,bytes))),Err(error)if error.kind()==std::io::ErrorKind::NotFound=>Ok(None),Err(error)=>Err(error.into())}
}
fn string_field(fields:&BTreeMap<String,&RawValue>,name:&str)->Result<Option<String>>{fields.get(name).map(|value|serde_json::from_str::<Option<String>>(value.get()).map_err(json_error)).transpose().map(Option::flatten)}
pub(crate) fn read_fs_gate_origin(root:&Path,prefix:&ObjectPrefix)->Result<Option<String>>{
    let Some((_,bytes))=load(root,prefix)?else{return Ok(None);};let fields:BTreeMap<String,&RawValue>=serde_json::from_slice(&bytes).map_err(json_error)?;
    if string_field(&fields,"prefix")?.as_deref().is_some_and(|stored|stored!=prefix.as_ref().trim_end_matches('/')){return Err(NarouError::Platform("filesystem gate prefix mismatch".into()));}
    let origin=string_field(&fields,"origin_url_fold")?.unwrap_or_default();
    // A retired gate file holds no authoritative origin: the committed FS→SQL
    // handoff superseded it and the SQL gate row carries the origin from now
    // on. Only the SQLite backend retires a gate file, and only with a live
    // database behind it; a marker without that authority is unproven
    // provenance and stays fail-closed. The caller proves the per-prefix SQL
    // gate row itself before this read (the writer epoch check), so the
    // retired file never becomes the only origin proof.
    let migrated:bool=fields.get("migrated").map(|value|serde_json::from_str::<bool>(value.get()).map_err(json_error)).transpose()?.unwrap_or(false);
    if migrated{
        let narou_dir=root.join(".narou");
        let retired=crate::native::sqlite::state::read_mode(&narou_dir)==crate::native::sqlite::state::StorageMode::Sqlite
            &&narou_dir.join(crate::native::sqlite::state::DB_FILE_NAME).exists();
        if !retired{return Err(af::gate_closed_error(prefix));}
        return Ok(None);
    }
    Ok(Some(origin))
}
pub(crate) fn rebind_fs_gate_origin_locked(root:&Path,prefix:&ObjectPrefix,fence_epoch:u64,expected:&str,new:&str)->Result<()>{
    if new.is_empty(){return Err(NarouError::Platform("virtual identity rebind requires a proven nonempty new origin".into()));}update(root,prefix,Some((fence_epoch,expected)),new)
}
pub(super) fn bind_fs_gate_origin_locked(root:&Path,prefix:&ObjectPrefix,new:&str)->Result<()>{if new.is_empty(){return Err(NarouError::Platform("empty namespace origin proof".into()));}update(root,prefix,None,new)}
fn update(root:&Path,prefix:&ObjectPrefix,rebind:Option<(u64,&str)>,new:&str)->Result<()>{
    let(path,bytes)=load(root,prefix)?.ok_or_else(||af::gate_closed_error(prefix))?;let mut fields:BTreeMap<String,&RawValue>=serde_json::from_slice(&bytes).map_err(json_error)?;let stored_prefix=string_field(&fields,"prefix")?;
    if stored_prefix.as_deref().is_some_and(|stored|stored!=prefix.as_ref().trim_end_matches('/')){return Err(NarouError::Platform("filesystem gate prefix mismatch".into()));}
    let origin=string_field(&fields,"origin_url_fold")?.unwrap_or_default();
    let migrated:bool=fields.get("migrated").map(|value|serde_json::from_str::<bool>(value.get()).map_err(json_error)).transpose()?.unwrap_or(false);
    if migrated{return Err(af::gate_closed_error(prefix));}
    // A committed FS→SQL handoff supersedes this file even when its
    // `migrated` marker never landed: origin binding must not revive it.
    {
        let current:u64=fields.get("epoch").map(|value|serde_json::from_str(value.get()).map_err(json_error)).transpose()?.unwrap_or(0);
        match super::live_sql_gate_authority(&root.join(".narou"),prefix,current,&origin)?{
            SqlGateAuthority::Superseded|SqlGateAuthority::Conflict=>return Err(af::gate_closed_error(prefix)),
            _=>{}
        }
    }
    if let Some((epoch,expected))=rebind{
        if stored_prefix.is_none(){return Err(NarouError::Platform("virtual identity gate has no exact prefix proof".into()));}
        let current:u64=serde_json::from_str(fields.get("epoch").ok_or_else(||NarouError::Platform("filesystem gate epoch is absent".into()))?.get()).map_err(json_error)?;
        let owner:Option<String>=serde_json::from_str(fields.get("owner").ok_or_else(||NarouError::Platform("filesystem gate admission state is absent".into()))?.get()).map_err(json_error)?;
        if current!=epoch||owner.is_some(){return Err(af::gate_epoch_error(prefix,epoch,current));}
        if origin!=expected&&origin!=new{return Err(NarouError::Platform("virtual identity gate origin changed".into()));}
    }else if !origin.is_empty()&&origin!=new{return Err(NarouError::Platform("namespace origin conflicts with current record".into()));}
    if origin==new&&stored_prefix.is_some(){return Ok(());}
    let encoded_origin=RawValue::from_string(serde_json::to_string(new).map_err(json_error)?).map_err(json_error)?;let encoded_prefix=RawValue::from_string(serde_json::to_string(prefix.as_ref().trim_end_matches('/')).map_err(json_error)?).map_err(json_error)?;
    fields.insert("origin_url_fold".into(),&encoded_origin);if stored_prefix.is_none(){fields.insert("prefix".into(),&encoded_prefix);}
    super::write_gate_file(&root.join(crate::downloader::types::ARCHIVE_ROOT_DIR),&path,&serde_json::to_vec(&fields).map_err(json_error)?)
}
