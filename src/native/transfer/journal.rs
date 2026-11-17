//! Indexed transfer control rows. YAML canonical metadata remains journaled on
//! disk; its private control database supplies atomic receipts and pagination.
use std::{collections::BTreeMap, fs, path::{Path, PathBuf}, sync::Arc};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};
use serde::{Serialize, de::DeserializeOwned};
use crate::{application::transfer::*, platform::PlatformFuture};
type TR<T> = Result<T, TransferError>;

const UNFINISHED: &str = "json_extract(value,'$.state') NOT IN ('skipped','rolled_back') AND NOT (json_extract(value,'$.state')='committed' AND COALESCE(json_extract(value,'$.phase_cursor'),'')='finished')";
const ACTIVE: &str = "json_extract(value,'$.state') NOT IN ('pending','skipped','rolled_back') AND NOT (json_extract(value,'$.state')='committed' AND COALESCE(json_extract(value,'$.phase_cursor'),'')='finished')";
const LIVE: &str = "json_extract(value,'$.state') NOT IN ('completed','completed_with_errors','released','cancelled','failed','expired')";

#[derive(Clone)]
pub struct NativeTransferJournal {
    pub(super) root: PathBuf,
    pub(super) state: Option<crate::native::sqlite::state::StateDb>,
    /// `Some` when the library is configured for SQLite (`storage-backend`
    /// marker) but `active_for` could not configure the database. Mutations
    /// must never run on the silent YAML fallback: the FS gate consult still
    /// sees the SQLite marker and fails closed against the unmigrated
    /// schema, so transfer writes are rejected up front with the original
    /// `configure` error preserved as the reason.
    pub(super) authority_error: Option<String>,
    connection: Arc<parking_lot::Mutex<Connection>>,
    pub(super) active_locks: Arc<parking_lot::Mutex<BTreeMap<String, (fs::File,OperationId)>>>,
}

pub(super) struct Rows<'a>(pub(super) &'a Connection);

fn scope_key(scope:&AuthScope)->String { sha256_hex(format!("{}\0{}",scope.library_id,scope.principal).as_bytes()) }
fn number(value:u32)->String { format!("{value:010}") }
fn chunk_key(file:u32, chunk:u32)->String { format!("{}:{}",number(file),number(chunk)) }
fn catalog_key(ordinal:u32,path:&str)->String { format!("{}:{path}",number(ordinal)) }
fn cursor_decode(value:Option<&str>)->TR<Option<String>> {
    use base64::Engine;
    value.map(|value| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value)
        .map_err(|_|TransferError::invalid("invalid_cursor","invalid page cursor"))
        .and_then(|bytes|String::from_utf8(bytes).map_err(|_|TransferError::invalid("invalid_cursor","invalid page cursor")))).transpose()
}
fn cursor_encode(value:&str)->String { use base64::Engine; base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value) }

impl NativeTransferJournal {
    pub fn open(root:&Path)->TR<Self> {
        let root=root.join(".narou/transfer"); fs::create_dir_all(&root).map_err(TransferError::platform)?;
        let narou_dir=root.parent().expect("transfer directory has parent");
        let state=crate::native::sqlite::state::active_for(narou_dir);
        // `active_for` just attempted `configure`, so `configured_failure` is
        // the fresh cause when the configured SQLite authority is missing.
        let authority_error=if state.is_none(){
            crate::native::sqlite::state::configured_failure(narou_dir)
        }else{None};
        let connection=if let Some(state)=&state {state.conn_ref().clone()} else {
            let connection=Connection::open(root.join("control.sqlite")).map_err(TransferError::platform)?;
            connection.busy_timeout(std::time::Duration::from_secs(10)).map_err(TransferError::platform)?;
            connection.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;").map_err(TransferError::platform)?;
            Arc::new(parking_lot::Mutex::new(connection))
        };
        {
            let connection=connection.lock();
            connection.execute_batch("CREATE TABLE IF NOT EXISTS transfer_native_rows(kind TEXT NOT NULL,owner TEXT NOT NULL,key TEXT NOT NULL,value BLOB NOT NULL,sort_key TEXT NOT NULL,PRIMARY KEY(kind,owner,key));
                CREATE TABLE IF NOT EXISTS transfer_backend_marks(key TEXT PRIMARY KEY,value BLOB NOT NULL);
                CREATE INDEX IF NOT EXISTS transfer_native_by_key ON transfer_native_rows(kind,key,owner);
                CREATE INDEX IF NOT EXISTS transfer_native_session_order ON transfer_native_rows(kind,owner,sort_key,key) WHERE kind='session';
                CREATE INDEX IF NOT EXISTS transfer_native_state ON transfer_native_rows(kind,owner,json_extract(value,'$.state'),key) WHERE kind IN ('session','file','publication');
                CREATE INDEX IF NOT EXISTS transfer_native_expiry ON transfer_native_rows(kind,owner,json_extract(value,'$.expires_at'),key) WHERE kind='session';
                CREATE INDEX IF NOT EXISTS transfer_native_staged ON transfer_native_rows(kind,owner,(json_extract(value,'$.stage') IS NOT NULL OR json_extract(value,'$.pin') IS NOT NULL),key) WHERE kind='file';
                CREATE INDEX IF NOT EXISTS transfer_native_selected ON transfer_native_rows(kind,owner,json_extract(value,'$.selected'),key) WHERE kind='catalog';
                CREATE INDEX IF NOT EXISTS transfer_native_selection_order ON transfer_native_rows(owner,CAST(json_extract(value,'$.novel_ordinal') AS TEXT),json_extract(value,'$.logical_path')) WHERE kind='catalog';") .map_err(TransferError::platform)?;
            connection.execute_batch(super::claims::SCHEMA).map_err(TransferError::platform)?;
        }
        Ok(Self {root,state,authority_error,connection,active_locks:Arc::new(parking_lot::Mutex::new(BTreeMap::new()))})
    }
    /// Reject when the configured SQLite storage authority is unavailable.
    /// Called at mutating entry points *before* any journal or store write so
    /// a `storage-backend=sqlite` library whose `db.sqlite` could not be
    /// configured never silently executes transfer work on YAML semantics.
    pub(super) fn require_authority(&self)->TR<()> {
        if let Some(reason)=&self.authority_error {
            return Err(TransferError::Unsupported{code:"storage_unavailable",reason:reason.clone()});
        }
        Ok(())
    }
    /// Mutating journal transaction. Reads stay on `run`; every path that can
    /// insert/update/delete durable transfer rows goes through this gate.
    pub(super) fn run_write<T>(&self, operation:impl FnOnce(&mut Rows<'_>)->TR<T>)->TR<T> {
        self.require_authority()?;
        self.run(operation)
    }
    pub(super) fn run<T>(&self, operation:impl FnOnce(&mut Rows<'_>)->TR<T>)->TR<T> {
        let mut connection=self.connection.lock();
        let transaction=connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(TransferError::platform)?;
        let result=operation(&mut Rows(&transaction))?;
        transaction.commit().map_err(TransferError::platform)?; Ok(result)
    }
    pub(super) fn get<T:DeserializeOwned>(&self,kind:&str,owner:&str,key:&str)->TR<Option<T>> {self.run(|rows|rows.get(kind,owner,key))}
    pub(super) fn put<T:Serialize>(&self,kind:&str,owner:&str,key:&str,value:&T,insert_only:bool)->TR<bool> {
        self.require_authority()?;
        self.run(|rows| {if insert_only&&rows.raw(kind,owner,key)?.is_some(){return Ok(false);}rows.put(kind,owner,key,value)?;Ok(true)})
    }
    pub(super) fn page<T:DeserializeOwned+Serialize>(&self,kind:&str,owner:&str,cursor:Option<&str>,limit:usize,predicate:&str)->TR<Page<T>> {
        self.run(|rows|rows.page(kind,owner,cursor,limit,predicate))
    }
    pub fn backend_get<T:DeserializeOwned>(&self,owner:&str,key:&str)->TR<Option<T>> {self.get("backend",owner,key)}
    pub fn backend_put<T:Serialize>(&self,owner:&str,key:&str,value:&T)->TR<()> {self.put("backend",owner,key,value,false).map(|_|())}
    pub(super) fn backend_page<T:DeserializeOwned+Serialize>(&self,owner:&str,cursor:Option<&str>,limit:usize)->TR<Page<T>> {self.page("backend",owner,cursor,limit,"1")}
    pub(super) fn session_for_work(&self,id:&SessionId)->TR<SessionRow> {self.run(|rows|rows.session(id).map(|(_,session)|session))}
    pub(super) fn executing_operation(&self,id:&SessionId)->TR<OperationId>{self.active_locks.lock().get(&id.0).map(|(_,operation)|operation.clone()).ok_or_else(||TransferError::Busy{code:"operation_lease_required",message:"durable operation ownership is required".into()})}
    pub(super) fn control_connection(&self)->Arc<parking_lot::Mutex<Connection>>{self.connection.clone()}
}

impl Rows<'_> {
    pub(super) fn raw(&self,kind:&str,owner:&str,key:&str)->TR<Option<Vec<u8>>> {
        self.0.query_row("SELECT value FROM transfer_native_rows WHERE kind=?1 AND owner=?2 AND key=?3",rusqlite::params![kind,owner,key],|row|row.get(0)).optional().map_err(TransferError::platform)
    }
    pub(super) fn get<T:DeserializeOwned>(&self,kind:&str,owner:&str,key:&str)->TR<Option<T>> {self.raw(kind,owner,key)?.map(|bytes|serde_json::from_slice(&bytes).map_err(TransferError::platform)).transpose()}
    pub(super) fn put<T:Serialize>(&mut self,kind:&str,owner:&str,key:&str,value:&T)->TR<()> {self.write(kind,owner,key,Some(serde_json::to_vec(value).map_err(TransferError::platform)?))}
    pub(super) fn write(&mut self,kind:&str,owner:&str,key:&str,value:Option<Vec<u8>>)->TR<()> {
        if let Some(value)=value {let sort=if kind=="session"{#[derive(serde::Deserialize)]struct Time{updated_at:DateTime<Utc>}serde_json::from_slice::<Time>(&value).map_err(TransferError::platform)?.updated_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos,true)}else{String::new()};self.0.execute("INSERT INTO transfer_native_rows(kind,owner,key,value,sort_key) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(kind,owner,key) DO UPDATE SET value=excluded.value,sort_key=excluded.sort_key",rusqlite::params![kind,owner,key,value,sort]).map_err(TransferError::platform)?;}
        else {self.0.execute("DELETE FROM transfer_native_rows WHERE kind=?1 AND owner=?2 AND key=?3",rusqlite::params![kind,owner,key]).map_err(TransferError::platform)?;} Ok(())
    }
    fn session(&self,id:&SessionId)->TR<(String,SessionRow)> {
        self.0.query_row("SELECT owner,value FROM transfer_native_rows WHERE kind='session' AND key=?1",[&id.0],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Vec<u8>>(1)?))).optional().map_err(TransferError::platform)?
            .map(|(owner,bytes)|serde_json::from_slice(&bytes).map(|session|(owner,session)).map_err(TransferError::platform)).transpose()?.ok_or(TransferError::SessionNotFound)
    }
    fn page<T:DeserializeOwned+Serialize>(&self,kind:&str,owner:&str,cursor:Option<&str>,limit:usize,predicate:&str)->TR<Page<T>> {
        let after=cursor_decode(cursor)?;let (after_sort,after_key)=if kind=="session"{after.as_deref().map(|value|value.split_once('\0').map(|(sort,key)|(Some(sort),Some(key))).ok_or_else(||TransferError::invalid("invalid_cursor","session ordering cursor is malformed"))).transpose()?.unwrap_or((None,None))}else{(None,after.as_deref())};
        let advance=if kind=="session"{"sort_key>?3 OR (sort_key=?3 AND key>?4)"}else{"key>?4"};let order=if kind=="session"{"sort_key,key"}else{"key"};
        let sql=format!("SELECT key,value,sort_key FROM transfer_native_rows WHERE kind=?1 AND owner=?2 AND (?4 IS NULL OR ({advance})) AND ({predicate}) ORDER BY {order} LIMIT ?5");
        let mut statement=self.0.prepare(&sql).map_err(TransferError::platform)?;
        let mut query=statement.query(rusqlite::params![kind,owner,after_sort,after_key,limit.clamp(1,MAX_PAGE_ITEMS) as i64+1]).map_err(TransferError::platform)?;
        let mut items=Vec::new();let mut bytes=128usize;let mut last=None;
        while let Some(row)=query.next().map_err(TransferError::platform)? {
            let key:String=row.get(0).map_err(TransferError::platform)?;let raw:Vec<u8>=row.get(1).map_err(TransferError::platform)?;
            let value:T=serde_json::from_slice(&raw).map_err(TransferError::platform)?;
            let size=serde_json::to_vec(&value).map_err(TransferError::platform)?.len()+1;
            if items.len()==limit.clamp(1,MAX_PAGE_ITEMS)||bytes+size>MAX_PAGE_BYTES-1024 {
                if items.is_empty(){return Err(TransferError::invalid("metadata_row_too_large","one public row exceeds256KiB page budget"));}
                return Ok(Page{items,next_cursor:last.map(|last:String|cursor_encode(&last))});
            }
            bytes+=size;last=Some(if kind=="session"{format!("{}\0{key}",row.get::<_,String>(2).map_err(TransferError::platform)?)}else{key});items.push(value);
        }
        Ok(Page{items,next_cursor:None})
    }
}

impl TransferJournal for NativeTransferJournal {
    fn create_session<'a>(&'a self,row:&'a SessionRow)->PlatformFuture<'a,TR<()>> {Box::pin(async move {self.run_write(|rows| {
        let owner=scope_key(&row.scope);if rows.raw("session",&owner,&row.session_id.0)?.is_some(){return Ok(());}
        let count:u32=rows.0.query_row(&format!("SELECT COUNT(*) FROM transfer_native_rows WHERE kind='session' AND owner=?1 AND ({LIVE})"),[&owner],|row|row.get(0)).map_err(TransferError::platform)?;
        if count>=DEFAULT_MAX_SESSIONS_PER_SCOPE{return Err(TransferError::TooManySessions{max:DEFAULT_MAX_SESSIONS_PER_SCOPE});}
        rows.put("session",&owner,&row.session_id.0,row)
    })})}
    fn session<'a>(&'a self,scope:&'a AuthScope,id:&'a SessionId)->PlatformFuture<'a,TR<Option<SessionRow>>>{Box::pin(async move{self.get("session",&scope_key(scope),&id.0)})}
    fn update_session<'a>(&'a self,row:&'a SessionRow,expected:u64)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.run_write(|rows|{let owner=scope_key(&row.scope);let old:SessionRow=rows.get("session",&owner,&row.session_id.0)?.ok_or(TransferError::SessionNotFound)?;if old.revision!=expected{return Err(TransferError::RevisionMismatch{current:old.revision});}rows.put("session",&owner,&row.session_id.0,row)})})}
    fn list_sessions<'a>(&'a self,scope:&'a AuthScope,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,TR<Page<SessionRow>>>{Box::pin(async move{self.page("session",&scope_key(scope),cursor,limit,&format!("({LIVE}) OR json_extract(value,'$.next_action')='advance'"))})}
    fn count_live_sessions<'a>(&'a self,scope:&'a AuthScope)->PlatformFuture<'a,TR<u32>>{Box::pin(async move{self.run(|rows|rows.0.query_row(&format!("SELECT COUNT(*) FROM transfer_native_rows WHERE kind='session' AND owner=?1 AND ({LIVE})"),[scope_key(scope)],|row|row.get(0)).map_err(TransferError::platform))})}
    fn find_idle_sessions<'a>(&'a self,scope:&'a AuthScope,now:DateTime<Utc>,limit:usize)->PlatformFuture<'a,TR<Vec<SessionRow>>>{Box::pin(async move{
        let timestamp=now.to_rfc3339_opts(chrono::SecondsFormat::AutoSi,true);
        Ok(self.page("session",&scope_key(scope),None,limit,&format!("json_extract(value,'$.expires_at')<='{timestamp}' AND (COALESCE(json_extract(value,'$.resume_phase'),'')<>'publishing') AND json_extract(value,'$.state') NOT IN ('publishing','cancelling') AND (({LIVE}) OR json_extract(value,'$.next_action')='advance')"))?.items)
    })}
    fn sessions_with_pending_publications<'a>(&'a self,scope:&'a AuthScope,limit:usize)->PlatformFuture<'a,TR<Vec<SessionRow>>>{Box::pin(async move{self.run(|rows|{
        let sql=format!("SELECT s.value FROM transfer_native_rows s WHERE s.kind='session' AND s.owner=?1 AND (EXISTS(SELECT 1 FROM transfer_native_rows p WHERE p.kind='publication' AND p.owner=s.key AND ({})) OR (json_extract(s.value,'$.state') IN ('publishing','cancelling') OR json_extract(s.value,'$.resume_phase')='publishing') AND EXISTS(SELECT 1 FROM transfer_native_rows p WHERE p.kind='publication' AND p.owner=s.key AND json_extract(p.value,'$.state')='pending')) ORDER BY s.key LIMIT ?2",ACTIVE.replace("json_extract(value,","json_extract(p.value,"));
        let mut statement=rows.0.prepare(&sql).map_err(TransferError::platform)?;
        let values=statement.query_map(rusqlite::params![scope_key(scope),limit.min(MAX_PAGE_ITEMS) as i64],|row|row.get::<_,Vec<u8>>(0)).map_err(TransferError::platform)?;
        values.map(|value|serde_json::from_slice(&value.map_err(TransferError::platform)?).map_err(TransferError::platform)).collect()
    })})}
    fn find_create<'a>(&'a self,scope:&'a AuthScope,id:&'a str)->PlatformFuture<'a,TR<Option<CreateRow>>>{Box::pin(async move{self.get("create",&scope_key(scope),id)})}
    fn put_create<'a>(&'a self,row:&'a CreateRow)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.put("create",&scope_key(&row.scope),&row.request_id,row,true).map(|_|())})}
    fn update_create<'a>(&'a self,row:&'a CreateRow)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.put("create",&scope_key(&row.scope),&row.request_id,row,false).map(|_|())})}
    fn operation<'a>(&'a self,id:&'a SessionId,op:&'a OperationId)->PlatformFuture<'a,TR<Option<OperationRow>>>{Box::pin(async move{self.get("operation",&id.0,&op.0)})}
    fn begin_operation<'a>(&'a self,row:&'a OperationRow)->PlatformFuture<'a,TR<bool>>{Box::pin(async move{
        // Reject before taking the OS invocation lock: an unavailable
        // configured SQLite authority must never acquire transfer leases.
        self.require_authority()?;
        let mut locks=self.active_locks.lock();if locks.contains_key(&row.session_id.0){return Err(TransferError::Busy{code:"operation_busy",message:"another invocation holds this transfer session".into()});}
        let directory=self.root.join("invocations");fs::create_dir_all(&directory).map_err(TransferError::platform)?;
        let file=fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(directory.join(format!("{}.lock",sha256_hex(row.session_id.0.as_bytes())))).map_err(TransferError::platform)?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|error|if error.kind()==std::io::ErrorKind::WouldBlock||error.raw_os_error()==fs2::lock_contended_error().raw_os_error(){TransferError::Busy{code:"operation_busy",message:"another process holds this transfer session".into()}}else{TransferError::platform(error)})?;
        let inserted=self.put("operation",&row.session_id.0,&row.operation_id.0,row,true)?;locks.insert(row.session_id.0.clone(),(file,row.operation_id.clone()));Ok(inserted)
    })}
    fn finish_operation<'a>(&'a self,row:&'a OperationRow)->PlatformFuture<'a,TR<()>>{Box::pin(async move{let result=self.put("operation",&row.session_id.0,&row.operation_id.0,row,false).map(|_|());let mut locks=self.active_locks.lock();if locks.get(&row.session_id.0).is_some_and(|(_,operation)|operation==&row.operation_id){locks.remove(&row.session_id.0);}result})}
    fn put_file<'a>(&'a self,row:&'a FileRow)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.put("file",&row.session_id.0,&number(row.file_id),row,true).map(|_|())})}
    fn file<'a>(&'a self,id:&'a SessionId,file:u32)->PlatformFuture<'a,TR<Option<FileRow>>>{Box::pin(async move{self.get("file",&id.0,&number(file))})}
    fn files<'a>(&'a self,id:&'a SessionId,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,TR<Page<FileRow>>>{Box::pin(async move{self.page("file",&id.0,cursor,limit,"json_extract(value,'$.file_id')<>4294967295")})}
    fn update_file<'a>(&'a self,row:&'a FileRow)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.put("file",&row.session_id.0,&number(row.file_id),row,false).map(|_|())})}
    fn pending_files<'a>(&'a self,id:&'a SessionId,limit:usize)->PlatformFuture<'a,TR<Vec<FileRow>>>{Box::pin(async move{Ok(self.page("file",&id.0,None,limit,"json_extract(value,'$.file_id')<>4294967295 AND json_extract(value,'$.state') IN ('pending','receiving','failed','finalizing')")?.items)})}
    fn finalizing_files<'a>(&'a self,id:&'a SessionId,limit:usize)->PlatformFuture<'a,TR<Vec<FileRow>>>{Box::pin(async move{Ok(self.page("file",&id.0,None,limit,"json_extract(value,'$.state')='finalizing'")?.items)})}
    fn staged_files<'a>(&'a self,id:&'a SessionId,limit:usize)->PlatformFuture<'a,TR<Vec<FileRow>>>{Box::pin(async move{Ok(self.page("file",&id.0,None,limit,"json_extract(value,'$.stage') IS NOT NULL OR json_extract(value,'$.pin') IS NOT NULL")?.items)})}
    fn reset_file<'a>(&'a self,row:&'a FileRow)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.run_write(|rows|{
        if row.state!=FileState::Pending||row.confirmed_bytes!=0||row.next_chunk_index!=0||row.stage.is_some()||row.pin.is_some(){return Err(TransferError::invalid("invalid_file_reset","restart must be an empty pending file"));}
        let old:FileRow=rows.get("file",&row.session_id.0,&number(row.file_id))?.ok_or_else(||TransferError::invalid("file_missing","restart file missing"))?;
        if !matches!(old.state,FileState::Pending|FileState::Receiving|FileState::Failed){return Err(TransferError::invalid("verified_file_reset","verified or sealed files cannot be reset"));}
        let(owner,mut session)=rows.session(&row.session_id)?;
        session.confirmed_bytes=session.confirmed_bytes.checked_sub(old.confirmed_bytes).ok_or_else(||TransferError::invalid("journal_corrupt","confirmed counter underflow"))?;
        session.revision+=1;session.updated_at=Utc::now();session.expires_at=session.updated_at+chrono::Duration::seconds(SESSION_IDLE_TTL_SECS);
        if row.file_id==MANIFEST_FILE_ID&&let SessionExt::Import(ext)=&mut session.ext{ext.manifest_confirmed=0;}
        let start=format!("{}:",number(row.file_id));let end=format!("{};",number(row.file_id));
        rows.0.execute("DELETE FROM transfer_native_rows WHERE kind='chunk' AND owner=?1 AND key>=?2 AND key<?3",rusqlite::params![row.session_id.0,start,end]).map_err(TransferError::platform)?;
        rows.write("active_stage",&row.session_id.0,&number(row.file_id),None)?;
        rows.put("file",&row.session_id.0,&number(row.file_id),row)?;rows.put("session",&owner,&row.session_id.0,&session)
    })})}
    fn chunk<'a>(&'a self,id:&'a SessionId,file:u32,chunk:u32)->PlatformFuture<'a,TR<Option<ChunkRow>>>{Box::pin(async move{self.get("chunk",&id.0,&chunk_key(file,chunk))})}
    fn commit_chunk<'a>(&'a self,chunk:&'a ChunkRow,file:&'a FileRow,_confirmed:u64)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.run_write(|rows|{
        let key=chunk_key(chunk.file_id,chunk.chunk_index);if let Some(old)=rows.get::<ChunkRow>("chunk",&chunk.session_id.0,&key)?{if old!=*chunk{return Err(TransferError::ChunkConflict{code:"chunk_conflict",message:"chunk receipt differs".into()});}return Ok(());}
        let old:FileRow=rows.get("file",&file.session_id.0,&number(file.file_id))?.ok_or_else(||TransferError::invalid("file_missing","chunk file missing"))?;
        if old.confirmed_bytes!=chunk.offset||old.next_chunk_index!=chunk.chunk_index{return Err(TransferError::ChunkConflict{code:"unexpected_offset",message:"file advanced concurrently".into()});}
        let(owner,mut session)=rows.session(&chunk.session_id)?;session.confirmed_bytes=session.confirmed_bytes.checked_add(chunk.payload_size).ok_or_else(||TransferError::invalid("size_overflow","confirmed bytes overflow"))?;
        session.revision+=1;session.updated_at=Utc::now();session.expires_at=session.updated_at+chrono::Duration::seconds(SESSION_IDLE_TTL_SECS);
        if file.file_id==MANIFEST_FILE_ID&&let SessionExt::Import(ext)=&mut session.ext{ext.manifest_confirmed=file.confirmed_bytes;}
        if file.state==FileState::Finalizing{session.next_action=NextAction::FinalizeFile;session.operation_id=file.finalize_operation.clone();}
        rows.put("chunk",&chunk.session_id.0,&key,chunk)?;rows.put("file",&file.session_id.0,&number(file.file_id),file)?;rows.put("session",&owner,&session.session_id.0,&session)
    })})}
    fn put_publication<'a>(&'a self,row:&'a PublicationRow)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.run_write(|rows|{let key=number(row.publication_id);if let Some(old)=rows.get::<PublicationRow>("publication",&row.session_id.0,&key)?&&!matches!(old.state,PublicationState::Pending|PublicationState::Skipped|PublicationState::RolledBack){return Ok(());}rows.put("publication",&row.session_id.0,&key,row)})})}
    fn publication<'a>(&'a self,id:&'a SessionId,publication:u32)->PlatformFuture<'a,TR<Option<PublicationRow>>>{Box::pin(async move{self.get("publication",&id.0,&number(publication))})}
    fn update_publication<'a>(&'a self,row:&'a PublicationRow)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.run_write(|rows|{let key=number(row.publication_id);let old:Option<PublicationRow>=rows.get("publication",&row.session_id.0,&key)?;
        if old.as_ref().is_some_and(|old|(old.state==PublicationState::Committed&&old.phase_cursor.as_deref()==Some("finished"))||old.state==PublicationState::RolledBack){return Ok(());}
        if row.state==PublicationState::Committed&&row.phase_cursor.as_deref()==Some("finished")&&!old.as_ref().is_some_and(|old|old.state==PublicationState::Committed&&old.phase_cursor.as_deref()==Some("finished")){
            let(owner,mut session)=rows.session(&row.session_id)?;session.published_novels=session.published_novels.checked_add(1).ok_or_else(||TransferError::invalid("count_overflow","published count overflow"))?;rows.put("session",&owner,&row.session_id.0,&session)?;
        }rows.put("publication",&row.session_id.0,&key,row)
    })})}
    fn pending_publications<'a>(&'a self,id:&'a SessionId,limit:usize)->PlatformFuture<'a,TR<Vec<PublicationRow>>>{Box::pin(async move{Ok(self.page("publication",&id.0,None,limit,UNFINISHED)?.items)})}
    fn active_publications<'a>(&'a self,id:&'a SessionId,limit:usize)->PlatformFuture<'a,TR<Vec<PublicationRow>>>{Box::pin(async move{Ok(self.page("publication",&id.0,None,limit,ACTIVE)?.items)})}
    fn publications<'a>(&'a self,id:&'a SessionId,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,TR<Page<PublicationRow>>>{Box::pin(async move{self.page("publication",&id.0,cursor,limit,"1")})}
    fn put_journal_entry<'a>(&'a self,id:&'a SessionId,publication:u32,key:&'a str,value:&'a [u8])->PlatformFuture<'a,TR<()>>{Box::pin(async move{if value.len()>MAX_CONTROL_BODY_BYTES{return Err(TransferError::invalid("journal_row_too_large","journal fragment exceeds256KiB"));}self.run_write(|rows|rows.write("blob",&format!("{}:{}",id.0,number(publication)),key,Some(value.to_vec())))})}
    fn journal_entry<'a>(&'a self,id:&'a SessionId,publication:u32,key:&'a str)->PlatformFuture<'a,TR<Option<Vec<u8>>>>{Box::pin(async move{self.run(|rows|rows.raw("blob",&format!("{}:{}",id.0,number(publication)),key))})}
    fn delete_journal_entries<'a>(&'a self,id:&'a SessionId,publication:u32)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.run_write(|rows|{rows.0.execute("DELETE FROM transfer_native_rows WHERE kind='blob' AND owner=?1",[format!("{}:{}",id.0,number(publication))]).map_err(TransferError::platform)?;Ok(())})})}
    fn put_plan_entries<'a>(&'a self,id:&'a SessionId,_revision:&'a str,entries:&'a [PlanEntry])->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.run_write(|rows|{for entry in entries{rows.put("plan",&id.0,&number(entry.novel_ordinal),entry)?;}Ok(())})})}
    fn plan_entries<'a>(&'a self,id:&'a SessionId,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,TR<Page<PlanEntry>>>{Box::pin(async move{let mut page:Page<PlanEntry>=self.page("plan",&id.0,cursor,limit,"1")?;let mut bytes=128usize;let mut keep=0;for entry in &mut page.items{entry.receipt=self.get::<PublicationRow>("publication",&id.0,&number(entry.novel_ordinal))?.and_then(|publication|publication.receipt).map(|bytes|serde_json::from_slice(&bytes).map_err(TransferError::platform)).transpose()?;let size=serde_json::to_vec(entry).map_err(TransferError::platform)?.len()+1;if bytes+size>MAX_PAGE_BYTES-1024{break;}bytes+=size;keep+=1;}if keep<page.items.len(){if keep==0{return Err(TransferError::invalid("metadata_row_too_large","plan receipt exceeds page budget"));}page.items.truncate(keep);page.next_cursor=Some(cursor_encode(&number(page.items.last().expect("nonempty page").novel_ordinal)));}Ok(page)})}
    fn put_catalog_entries<'a>(&'a self,id:&'a SessionId,_revision:u64,entries:&'a [CatalogItem])->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.run_write(|rows|{for entry in entries{rows.put("catalog",&id.0,&catalog_key(entry.novel_ordinal,&entry.logical_path),entry)?;rows.put("selection_catalog",&id.0,&format!("{}\0{}",entry.novel_ordinal,entry.logical_path),entry)?;}Ok(())})})}
    fn catalog_entries<'a>(&'a self,id:&'a SessionId,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,TR<Page<CatalogItem>>>{Box::pin(async move{self.page("catalog",&id.0,cursor,limit,"1")})}
    fn selection_catalog_entries<'a>(&'a self,id:&'a SessionId,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,TR<Page<CatalogItem>>>{Box::pin(async move{self.page("selection_catalog",&id.0,cursor,limit,"1")})}
    fn selected_catalog_entries<'a>(&'a self,id:&'a SessionId,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,TR<Page<CatalogItem>>>{Box::pin(async move{self.page("catalog",&id.0,cursor,limit,"json_extract(value,'$.selected')=1")})}
    fn mark_catalog_selected<'a>(&'a self,id:&'a SessionId,keys:&'a [(u32,String)],selected:bool)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.run_write(|rows|{for(ordinal,path)in keys{let key=catalog_key(*ordinal,path);let mut item:CatalogItem=rows.get("catalog",&id.0,&key)?.ok_or_else(||TransferError::invalid("catalog_missing","catalog row missing"))?;item.selected=selected;rows.put("catalog",&id.0,&key,&item)?;}Ok(())})})}
}

/// Only for a dedicated VACUUM/backup COPY, never the live control connection.
/// Canonical ID generations, committed receipts and origin bindings survive.
pub(crate) fn sanitize_control_snapshot(connection:&mut Connection)->crate::error::Result<()>{
    if !connection.is_autocommit(){return Err(crate::error::NarouError::Platform("control snapshot sanitizer requires an isolated autocommit copy".into()));}
    connection.pragma_update(None,"secure_delete",true).map_err(crate::native::sqlite::sqlite_error)?;
    let transaction=connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(crate::native::sqlite::sqlite_error)?;
    for table in ["native_transfer_target_claims","native_transfer_id_owners","transfer_native_rows","transfer_backend_marks"]{let exists:bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",[table],|row|row.get(0)).map_err(crate::native::sqlite::sqlite_error)?;if exists{transaction.execute(&format!("DELETE FROM {table}"),[]).map_err(crate::native::sqlite::sqlite_error)?;}}
    transaction.commit().map_err(crate::native::sqlite::sqlite_error)?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE; VACUUM;").map_err(crate::native::sqlite::sqlite_error)
}

#[cfg(test)]
#[path="journal_tests.rs"]
mod tests;

