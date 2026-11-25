//! Native gate transitions run under the stable OS lock and metadata authority.
use super::*;

pub(crate) fn bind_namespace_origin(root:&Path,connection:&rusqlite::Connection,prefix:&ObjectPrefix,folded_url:&str)->Result<()>{
    if folded_url.is_empty(){return Err(NarouError::Platform("empty namespace origin proof".into()));}
    let sql:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='novel_access_gates')",[],|row|row.get(0)).map_err(crate::native::sqlite::sqlite_error)?;let sql=sql&&(root.as_os_str().is_empty()||crate::native::sqlite::state::active_for(&root.join(".narou")).is_some());
    if sql{let changed=connection.execute("UPDATE novel_access_gates SET origin_url_fold=?1 WHERE prefix=?2 AND origin_url_fold IN ('',?1)",rusqlite::params![folded_url,prefix.as_ref().trim_end_matches('/')]).map_err(crate::native::sqlite::sqlite_error)?;if changed!=1{return Err(NarouError::Platform("namespace origin conflicts with current record".into()));}return Ok(());}
    super::gate_origin::bind_fs_gate_origin_locked(root,prefix,folded_url)
}
impl NativeStore{
    pub(crate) fn archive_root_path(&self)->&Path{match self{Self::Fs(store)=>store.root(),Self::Sqlite(store)=>store.archive_root()}}
    pub(crate) fn with_gate_lock<T>(&self,prefix:&ObjectPrefix,operation:impl FnOnce()->Result<T>)->Result<T>{match self{Self::Fs(store)=>store.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),operation),Self::Sqlite(store)=>store.with_gate_lock(prefix,operation)}}
    pub(crate) fn namespace_state_locked(&self,prefix:&ObjectPrefix)->Result<(GateInfo,String)>{match self{
        Self::Fs(store)=>{let mut gate:FsGate=serde_json::from_slice(&fs::read(store.gate_path(prefix)?)?).map_err(|error|NarouError::Platform(error.to_string()))?;gate.prune();if gate.migrated{return Err(af::gate_closed_error(prefix));}match store.live_sql_authority(prefix,gate.epoch,&gate.origin_url_fold)?{SqlGateAuthority::Superseded|SqlGateAuthority::Conflict=>return Err(af::gate_closed_error(prefix)),_=>{}}Ok((GateInfo{epoch:gate.epoch,content_revision:gate.content_revision,state:if gate.owner.is_some(){GateState::Closed}else{GateState::Open},owner:gate.owner,pending_readers:gate.readers.len() as u32,updated_at:gate.updated_at},gate.origin_url_fold))},
        Self::Sqlite(store)=>store.namespace_state_locked(prefix),
    }}
    pub(crate) fn persist_retirement_locked(&self,fence:&crate::platform::WriteFence,owner:&str,connection:Option<&rusqlite::Connection>)->Result<u64>{
        af::parse_removal_owner(owner)?;let next=fence.epoch.checked_add(1).ok_or_else(||NarouError::Platform("gate epoch exhausted".into()))?;
        match self{
            Self::Fs(store)=>{let path=store.gate_path(&fence.prefix)?;let mut gate:FsGate=serde_json::from_slice(&fs::read(&path)?).map_err(|error|NarouError::Platform(error.to_string()))?;
                if gate.migrated{return Err(af::gate_closed_error(&fence.prefix));}match store.live_sql_authority(&fence.prefix,gate.epoch,&gate.origin_url_fold)?{SqlGateAuthority::Superseded|SqlGateAuthority::Conflict=>return Err(af::gate_closed_error(&fence.prefix)),_=>{}}if gate.owner.as_deref()==Some(owner)&&gate.epoch==next{return Ok(next);}
                if gate.owner.is_some()||gate.epoch!=fence.epoch{return Err(af::gate_epoch_error(&fence.prefix,fence.epoch,gate.epoch));}
                gate.prefix=Some(fence.prefix.as_ref().trim_end_matches('/').into());gate.epoch=next;gate.owner=Some(owner.into());gate.updated_at=ts::now_secs();crate::native::object_store::write_gate_file(&store.root,&path,&serde_json::to_vec(&gate).map_err(|error|NarouError::Platform(error.to_string()))?)?;Ok(next)
            },
            Self::Sqlite(_)=>{let connection=connection.ok_or_else(||NarouError::Platform("SQL removal has no metadata transaction".into()))?;let next=i64::try_from(next).map_err(|_|NarouError::Platform("SQL gate epoch exhausted".into()))?;
                crate::native::sqlite::object_store::require_transfer_epoch(connection,&fence.prefix,None,fence.epoch,false)?;let changed=connection.execute("UPDATE novel_access_gates SET closed=1,owner=?1,epoch=?2,updated_at=unixepoch() WHERE prefix=?3 AND closed=0 AND epoch=?4",rusqlite::params![owner,next,fence.prefix.as_ref().trim_end_matches('/'),i64::try_from(fence.epoch).map_err(|_|NarouError::Platform("SQL gate epoch exhausted".into()))?]).map_err(crate::native::sqlite::sqlite_error)?;if changed!=1{return Err(af::gate_closed_error(&fence.prefix));}Ok(next as u64)
            },
        }
    }
    pub(crate) fn invalidate_creation_locked(&self,fence:&crate::platform::WriteFence,connection:Option<&rusqlite::Connection>)->Result<u64>{
        let next=fence.epoch.checked_add(1).ok_or_else(||NarouError::Platform("gate epoch exhausted".into()))?;match self{
            Self::Fs(store)=>{store.invalidate_writer_locked(fence)?;Ok(next)},
            Self::Sqlite(_)=>{let connection=connection.ok_or_else(||NarouError::Platform("SQL creation release has no metadata authority".into()))?;let next=i64::try_from(next).map_err(|_|NarouError::Platform("SQL gate epoch exhausted".into()))?;crate::native::sqlite::object_store::require_transfer_epoch(connection,&fence.prefix,None,fence.epoch,false)?;connection.execute("UPDATE novel_access_gates SET epoch=?1,updated_at=unixepoch() WHERE prefix=?2 AND closed=0 AND epoch=?3",rusqlite::params![next,fence.prefix.as_ref().trim_end_matches('/'),i64::try_from(fence.epoch).map_err(|_|NarouError::Platform("SQL gate epoch exhausted".into()))?]).map_err(crate::native::sqlite::sqlite_error)?;Ok(next as u64)}
        }
    }
    pub(crate) fn retire_checked(&self,fence:&crate::platform::WriteFence,owner:&str)->Result<u64>{
        let(id,_)=af::parse_removal_owner(owner)?;let next=fence.epoch.checked_add(1).ok_or_else(||NarouError::Platform("gate epoch exhausted".into()))?;
        self.with_gate_lock(&fence.prefix,||{let(state,_)=self.namespace_state_locked(&fence.prefix)?;if state.state==GateState::Closed&&state.owner.as_deref()==Some(owner)&&state.epoch==next{return Ok(next);}
            if state.state!=GateState::Open||state.epoch!=fence.epoch{return Err(af::gate_epoch_error(&fence.prefix,fence.epoch,state.epoch));}
            crate::db::with_database(|database|{if fs::canonicalize(database.archive_root())?!=fs::canonicalize(self.archive_root_path())?{return Err(NarouError::Platform("removal metadata belongs to another native library".into()));}database.with_removal_identity(fence,id,|connection|self.persist_retirement_locked(fence,owner,connection))})
        })
    }
}

/// Ephemeral, root-relative physical identity. Ancestor views share storage.
#[derive(Clone)]
pub(crate) struct NativeNamespaceKey{units:Arc<Vec<u16>>,end:usize}
impl std::fmt::Debug for NativeNamespaceKey{fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.debug_struct("NativeNamespaceKey").field("units",&self.end).finish_non_exhaustive()}}
impl NativeNamespaceKey{
    fn from_units(units:Vec<u16>)->Self{let end=units.len();Self{units:Arc::new(units),end}}
    pub(crate) fn ancestor_keys(&self)->impl Iterator<Item=Self>+'_ {self.units[..self.end].iter().enumerate().filter(|(_,unit)|**unit==47).map(|(end,_)|Self{units:self.units.clone(),end}).chain(std::iter::once(self.clone()))}
    fn prefix_of(&self,other:&Self)->bool{self.end<=other.end&&(self.end==other.end||other.units[self.end]==47)&&self==&Self{units:other.units.clone(),end:self.end}}
    pub(crate) fn overlaps(&self,other:&Self)->bool{self.prefix_of(other)||other.prefix_of(self)}
}
impl PartialEq for NativeNamespaceKey{fn eq(&self,other:&Self)->bool{self.cmp(other)==std::cmp::Ordering::Equal}}
impl Eq for NativeNamespaceKey{}
impl PartialOrd for NativeNamespaceKey{fn partial_cmp(&self,other:&Self)->Option<std::cmp::Ordering>{Some(self.cmp(other))}}
impl Ord for NativeNamespaceKey{fn cmp(&self,other:&Self)->std::cmp::Ordering{
    let left=&self.units[..self.end];let right=&other.units[..other.end];
    #[cfg(windows)]
    {let result=unsafe{windows_sys::Win32::Globalization::CompareStringOrdinal(left.as_ptr(),left.len() as i32,right.as_ptr(),right.len() as i32,1)};match result{windows_sys::Win32::Globalization::CSTR_LESS_THAN=>std::cmp::Ordering::Less,windows_sys::Win32::Globalization::CSTR_EQUAL=>std::cmp::Ordering::Equal,windows_sys::Win32::Globalization::CSTR_GREATER_THAN=>std::cmp::Ordering::Greater,_=>panic!("Windows ordinal comparison rejected validated native path lengths: {}",std::io::Error::last_os_error())}}
    #[cfg(not(windows))]
    {left.cmp(right)}
}}
/// Reuse one resolver per metadata capture; never acquire DATABASE/SQL again.
pub(crate) struct NativeNamespacePaths{store:NativeObjectStore,root:PathBuf}
impl NativeNamespacePaths{
    pub(crate) fn new(root:&Path)->Result<Option<Self>>{if root.as_os_str().is_empty(){return Ok(None);}let store=NativeObjectStore::from_root(root.join(crate::downloader::types::ARCHIVE_ROOT_DIR))?;
        // Keep the identity root on the same canonical spelling
        // `ensure_within_archive_root` already emits through `resolve_path`:
        // `fs::canonicalize` returns a `\\?\` verbatim path on Windows while
        // resolved object paths keep the stripped form, so `key`'s
        // `strip_prefix` would reject every namespace.
        let root=crate::db::paths::ensure_within_archive_root(store.root(),store.root())?;Ok(Some(Self{store,root}))}
    pub(crate) fn key(&self,prefix:&str)->Result<NativeNamespaceKey>{
        let path=self.store.resolve_prefix_dir(prefix)?;let relative=path.strip_prefix(&self.root).map_err(|_|NarouError::Platform("physical namespace is outside its library root".into()))?;let mut units=Vec::with_capacity(relative.as_os_str().len());
        for component in relative.components(){let std::path::Component::Normal(component)=component else{return Err(NarouError::Platform("non-relative native namespace component".into()));};if !units.is_empty(){units.push(47);}
            #[cfg(windows)]{use std::os::windows::ffi::OsStrExt;units.extend(component.encode_wide());}
            #[cfg(not(windows))]{units.extend(component.to_str().ok_or_else(||NarouError::Platform("native namespace cannot be represented in portable metadata".into()))?.encode_utf16());}
        }
        if units.is_empty()||units.len()>=i32::MAX as usize{return Err(NarouError::Platform("invalid physical namespace length".into()));}Ok(NativeNamespaceKey::from_units(units))
    }
}
#[cfg(test)]
pub(crate) fn namespace_physical_overlap(root:&Path,left:&str,right:&str)->Result<bool>{let Some(resolver)=NativeNamespacePaths::new(root)?else{return Ok(false);};Ok(resolver.key(left)?.overlaps(&resolver.key(right)?))}

/// Whether a durable SQL handoff proves the SQL gate row is canonical for
/// `prefix` over the given legacy FS gate state. Absent table = pre-cutover,
/// FS stays authoritative (never defaults SQL-ward).
pub(crate) fn read_sql_gate_handoff(connection:&rusqlite::Connection,prefix:&str,fs_epoch:u64,fs_origin_fold:&str)->Result<bool>{
    use rusqlite::OptionalExtension;
    let table:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='native_gate_handoffs')",[],|row|row.get(0)).map_err(crate::native::sqlite::sqlite_error)?;
    if !table{return Ok(false);}
    let fs_epoch_i64=i64::try_from(fs_epoch).map_err(|_|NarouError::Platform("FS gate epoch exceeds SQLite integer range".into()))?;
    let row:Option<(i64,String)>=connection.query_row("SELECT sql_epoch,fs_origin_fold FROM native_gate_handoffs WHERE prefix=?1 AND fs_epoch=?2",rusqlite::params![prefix,fs_epoch_i64],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(crate::native::sqlite::sqlite_error)?;
    let Some((sql_epoch,origin))=row else{return Ok(false);};
    Ok(sql_epoch>=0&&(sql_epoch as u64)>fs_epoch&&origin==fs_origin_fold)
}
/// Record the FS→SQL authority handoff for one prefix. `sql_epoch` must exceed
/// `fs_epoch` so stale FS actors lose by construction; called inside the
/// mode-cutover transaction that installs the SQL gate row.
pub(crate) fn record_sql_gate_handoff(connection:&rusqlite::Connection,prefix:&str,fs_epoch:u64,sql_epoch:u64,fs_origin_fold:&str)->Result<()>{
    if sql_epoch<=fs_epoch{return Err(NarouError::Platform("SQL gate handoff must advance beyond the FS epoch".into()));}
    connection.execute("INSERT INTO native_gate_handoffs(prefix,fs_epoch,sql_epoch,fs_origin_fold,recorded_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(prefix) DO UPDATE SET fs_epoch=excluded.fs_epoch,sql_epoch=excluded.sql_epoch,fs_origin_fold=excluded.fs_origin_fold,recorded_at=excluded.recorded_at WHERE native_gate_handoffs.fs_epoch<=excluded.fs_epoch",rusqlite::params![prefix,i64::try_from(fs_epoch).map_err(|_|NarouError::Platform("FS gate epoch exceeds SQLite integer range".into()))?,i64::try_from(sql_epoch).map_err(|_|NarouError::Platform("SQL gate epoch exceeds SQLite integer range".into()))?,fs_origin_fold,crate::platform::transfer_stage::now_secs()]).map_err(crate::native::sqlite::sqlite_error)?;Ok(())
}
/// Whether a durable FS handoff proves an FS gate file is canonical for
/// `prefix` over the given SQL gate state (the SQL→FS direction). Absent
/// table = never cut over this way, SQL stays authoritative.
/// Returns `(fs_epoch, fs_origin_fold)` of the recorded handoff so the caller
/// can match it against the actual FS gate it found.
#[cfg(test)]
pub(crate) fn read_fs_gate_handoff(connection:&rusqlite::Connection,prefix:&str,sql_epoch:u64)->Result<Option<(u64,String)>>{
    use rusqlite::OptionalExtension;
    let table:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='fs_gate_handoffs')",[],|row|row.get(0)).map_err(crate::native::sqlite::sqlite_error)?;
    if !table{return Ok(None);}
    let row:Option<(i64,String)>=connection.query_row("SELECT fs_epoch,fs_origin_fold FROM fs_gate_handoffs WHERE prefix=?1 AND sql_epoch=?2",rusqlite::params![prefix,i64::try_from(sql_epoch).map_err(|_|NarouError::Platform("SQL gate epoch exceeds SQLite integer range".into()))?],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(crate::native::sqlite::sqlite_error)?;
    let Some((fs_epoch,origin))=row else{return Ok(None);};
    if fs_epoch<0||(fs_epoch as u64)<=sql_epoch{return Err(NarouError::Platform("FS gate handoff regress below the SQL epoch".into()));}
    Ok(Some((fs_epoch as u64,origin)))
}
/// Raw SQL→FS handoff for `prefix` regardless of which epoch it covers —
/// used by idempotent adoption re-runs and sweep classification.
/// Returns `(sql_epoch, fs_epoch, fs_origin_fold)`.
pub(crate) fn read_fs_gate_handoff_raw(connection:&rusqlite::Connection,prefix:&str)->Result<Option<(u64,u64,String)>>{
    use rusqlite::OptionalExtension;
    let table:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='fs_gate_handoffs')",[],|row|row.get(0)).map_err(crate::native::sqlite::sqlite_error)?;
    if !table{return Ok(None);}
    let row:Option<(i64,i64,String)>=connection.query_row("SELECT sql_epoch,fs_epoch,fs_origin_fold FROM fs_gate_handoffs WHERE prefix=?1",[prefix],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(crate::native::sqlite::sqlite_error)?;
    let Some((sql_epoch,fs_epoch,origin))=row else{return Ok(None);};
    if sql_epoch<0||fs_epoch<0{return Err(NarouError::Platform("negative epoch in durable FS gate handoff".into()));}
    Ok(Some((sql_epoch as u64,fs_epoch as u64,origin)))
}
/// Record the SQL→FS authority handoff for one prefix. `fs_epoch` must exceed
/// `sql_epoch` so stale SQL actors lose by construction; called inside the
/// mode-cutover transaction that retires the SQL gate row.
pub(crate) fn record_fs_gate_handoff(connection:&rusqlite::Connection,prefix:&str,sql_epoch:u64,fs_epoch:u64,fs_origin_fold:&str)->Result<()>{
    if fs_epoch<=sql_epoch{return Err(NarouError::Platform("FS gate handoff must advance beyond the SQL epoch".into()));}
    connection.execute("INSERT INTO fs_gate_handoffs(prefix,sql_epoch,fs_epoch,fs_origin_fold,recorded_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(prefix) DO UPDATE SET sql_epoch=excluded.sql_epoch,fs_epoch=excluded.fs_epoch,fs_origin_fold=excluded.fs_origin_fold,recorded_at=excluded.recorded_at WHERE fs_gate_handoffs.sql_epoch<=excluded.sql_epoch",rusqlite::params![prefix,i64::try_from(sql_epoch).map_err(|_|NarouError::Platform("SQL gate epoch exceeds SQLite integer range".into()))?,i64::try_from(fs_epoch).map_err(|_|NarouError::Platform("FS gate epoch exceeds SQLite integer range".into()))?,fs_origin_fold,crate::platform::transfer_stage::now_secs()]).map_err(crate::native::sqlite::sqlite_error)?;Ok(())
}
/// Retire a previously recorded SQL→FS handoff when the prefix is adopted
/// back into SQL authority (the new SQL row epoch exceeds the handoff's
/// fs_epoch, so stale FS tokens still lose by construction).
pub(crate) fn clear_fs_gate_handoff(connection:&rusqlite::Connection,prefix:&str)->Result<()>{
    connection.execute("DELETE FROM fs_gate_handoffs WHERE prefix=?1",[prefix]).map_err(crate::native::sqlite::sqlite_error)?;Ok(())
}

impl NativeObjectStore{
    pub(crate) fn list_window_blocking(&self,request:&ObjectListRequest,limit:usize)->Result<ObjectListPage>{
        fn walk(store:&NativeObjectStore,request:&ObjectListRequest,base:&Path,path:&Path,capacity:usize,rows:&mut std::collections::BTreeMap<String,ObjectMetadata>,ancestors:&mut Vec<PathBuf>)->Result<()>{
            let prefix=request.prefix.as_ref().trim_end_matches('/');
            let internal=if prefix.is_empty()||prefix=="novels"{path.strip_prefix(base).is_ok_and(|relative|relative.starts_with(".objects"))}else{!prefix.starts_with("novels/")};
            if internal&&path.file_name().is_some_and(|name|name.as_encoded_bytes().starts_with(ATOMIC_OBJECT_TEMP_PREFIX.as_bytes())){return Ok(());}
            let path=crate::db::paths::ensure_within_archive_root(path,store.root())?;
            if !path.starts_with(base){return Err(NarouError::Platform("namespace inventory crosses a physical book boundary".into()));}
            let metadata=fs::metadata(&path)?;if metadata.is_dir(){if ancestors.contains(&path){return Err(NarouError::Platform("cyclic native namespace link".into()));}ancestors.push(path.clone());let mut children=Vec::new();for entry in fs::read_dir(&path)?{children.push(entry?.path());}for child in children{walk(store,request,base,&child,capacity,rows,ancestors)?;}ancestors.pop();return Ok(());}
            if !metadata.is_file(){return Err(NarouError::Platform("unsupported native namespace entry type".into()));}let key=store.logical_key_for_listed_path(request.prefix.as_ref(),base,&path).ok_or_else(||NarouError::Platform("native file has no portable logical key".into()))?;
            if !request.prefix.matches(&key)||request.cursor.as_deref().is_some_and(|cursor|key.as_ref()<=cursor){return Ok(());}
            if rows.len()==capacity&&rows.last_key_value().is_some_and(|(last,_)|key.as_ref()>=last.as_str()){return Ok(());}
            if let Some(metadata)=store.metadata_for_path(&key,&path)?{rows.insert(key.to_string(),metadata);if rows.len()>capacity{rows.pop_last();}}Ok(())
        }
        let limit=limit.clamp(1,101);let base=self.resolve_prefix_dir(request.prefix.as_ref())?;let mut rows=std::collections::BTreeMap::new();if base.try_exists()?{walk(self,request,&base,&base,limit+1,&mut rows,&mut Vec::new())?;}
        let next_cursor=(rows.len()>limit).then(||rows.keys().nth(limit-1).expect("nonempty lookahead").clone());let objects=rows.into_values().take(limit).collect();Ok(ObjectListPage{objects,next_cursor})
    }
}
