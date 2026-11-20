//! Native novel repository over the shared YAML database state.
//!
//! This is the native implementation of [`NovelRepository`]. It shares the
//! single `db::DATABASE` global with the legacy Inventory access — there is
//! exactly one in-memory `Database` per YAML file, so cache/index/save state
//! can never diverge (Phase 3 §14).
//!
//! All blocking work (YAML serialize, file lock, atomic rename, fsync) runs
//! inside `spawn_blocking` when a tokio runtime is available, so long
//! operations never block an async executor worker directly. When called
//! outside a runtime (sync CLI tests), the operation runs inline — the
//! `parking_lot` lock is never held across an `.await`.
//!
//! The repository only owns novel-record operations. Inventory/settings and
//! archive-root path access remain on `db::DATABASE` / `db::paths` (Phase 4
//! targets).

use crate::db::NovelRecord;
use crate::error::{NarouError, Result};
use crate::platform::repository::{NovelLookup, record_matches_filter};
use crate::platform::{
    NovelFilter, NovelId, NovelMutation, NovelQuery, NovelRepository, PlatformFuture,
};

fn initialize_attributes(mutations: &mut [NovelMutation]) {
    let sites = crate::downloader::site_setting::effective_site_settings();
    for mutation in mutations {
        if let NovelMutation::Upsert(record) = mutation {
            crate::application::attribute_tags::initialize_record(record, &sites);
        }
    }
}

fn current_store()->Result<crate::native::object_store::NativeStore>{
    // Do not retain DATABASE while acquiring a gate: synchronous callers also
    // acquire the gate before the authoritative metadata transaction.
    let root=crate::db::with_database(|db|db.archive_root().parent().map(std::path::Path::to_owned).ok_or_else(||NarouError::Platform("archive has no library root".into())))?;
    crate::native::object_store::NativeStore::for_narou_root(&root)
}
fn require_record_origin(record:&NovelRecord,fences:&[crate::platform::WriteFence])->Result<()>{
    let keys=crate::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;
    if !fences.iter().any(|fence|keys.prefix().as_ref()==fence.prefix.as_ref().trim_end_matches('/')){return Err(NarouError::Platform("record origin outside captured fence".into()));}Ok(())
}

fn load_current_yaml_records(inventory:&crate::db::inventory::Inventory)->Result<std::collections::BTreeMap<i64,NovelRecord>>{
    let path=inventory.inventory_path("database",crate::db::inventory::InventoryScope::Local);let raw=crate::db::inventory::read_optional_yaml_file(&path)?;
    let mut records:std::collections::BTreeMap<i64,NovelRecord>=if raw.trim().is_empty(){Default::default()}else{serde_yaml::from_str(&raw)?};for(id,record)in &mut records{record.id = *id;}Ok(records)
}
#[path="novel_repository_creation.rs"]
mod creation;

/// Native repository. Stateless: every operation locks the shared
/// [`crate::db::DATABASE`] global for the duration of its (short, blocking)
/// section.
pub struct NativeNovelRepository;

impl Default for NativeNovelRepository {
    fn default() -> Self {
        Self
    }

}

impl NativeNovelRepository {
    pub fn new() -> Self {
        Self
    }
    pub fn get_current_sync(&self,id:NovelId)->Result<Option<NovelRecord>>{crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,connection|match connection.filter(|_|!db.metadata_is_yaml_in_snapshot(connection)){Some(connection)=>crate::native::sqlite::SqliteNovelRepository::point_snapshot_conn(connection,id),None=>Ok(load_current_yaml_records(inventory)?.remove(&id.0))}))}
    pub fn get_many_current_sync(&self,ids:&[NovelId])->Result<Vec<Option<NovelRecord>>>{if ids.is_empty(){return Ok(Vec::new());}crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,connection|match connection.filter(|_|!db.metadata_is_yaml_in_snapshot(connection)){Some(connection)=>crate::native::sqlite::SqliteNovelRepository::many_snapshot_conn(connection,ids),None=>Ok(crate::platform::repository::align_current_records(ids,load_current_yaml_records(inventory)?.into_values()))}))}
    pub fn query_current_sync(&self,query:&NovelQuery)->Result<Vec<NovelRecord>>{crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,connection|match connection.filter(|_|!db.metadata_is_yaml_in_snapshot(connection)){Some(connection)=>crate::native::sqlite::SqliteNovelRepository::query_snapshot_conn(connection,query),None=>Ok(crate::platform::repository::query_records(load_current_yaml_records(inventory)?.values(),query))}))}

    /// Metadata-only list snapshots share the publication/compatibility lock
    /// order so neither SQLite nor YAML/compat mode needs per-novel reader
    /// leases or `.gate` creation to render the library table.
    pub fn metadata_page_snapshot_sync(
        &self,
        query: &NovelQuery,
    ) -> Result<Option<crate::platform::repository::NovelMetadataPage>> {
        crate::db::with_database(|db| db.with_library_metadata_snapshot(|inventory, connection| {
            if let Some(connection) = connection && !db.metadata_is_yaml_in_snapshot(Some(connection)) {
                let transaction = rusqlite::Transaction::new_unchecked(
                    connection, rusqlite::TransactionBehavior::Deferred,
                ).map_err(crate::native::sqlite::sqlite_error)?;
                // Native freeze payloads stay canonical, including parse errors;
                // the relational table is only their status-search projection.
                use rusqlite::OptionalExtension;
                let raw: String = transaction.query_row(
                    "SELECT value_yaml FROM app_state WHERE scope='inv' AND key='freeze'",
                    [], |row| row.get(0),
                ).optional().map_err(crate::native::sqlite::sqlite_error)?.unwrap_or_default();
                let frozen: std::collections::HashMap<i64, serde_yaml::Value> = if raw.is_empty() {
                    Default::default()
                } else {
                    serde_yaml::from_str(&raw)?
                };
                let page = crate::native::sqlite::SqliteNovelRepository::metadata_page_snapshot_conn(
                    &transaction, query, frozen.into_keys().collect(),
                )?;
                transaction.commit().map_err(crate::native::sqlite::sqlite_error)?;
                return Ok(Some(page));
            }

            let records = load_current_yaml_records(inventory)?;
            let freeze_path = inventory.inventory_path("freeze", crate::db::inventory::InventoryScope::Local);
            let raw = if freeze_path.is_file() {
                crate::db::inventory::read_optional_yaml_file(&freeze_path)?
            } else if let Some(connection) = connection {
                use rusqlite::OptionalExtension;
                connection.query_row(
                    "SELECT value_yaml FROM app_state WHERE scope='inv' AND key='freeze'",
                    [], |row| row.get(0),
                ).optional().map_err(crate::native::sqlite::sqlite_error)?.unwrap_or_default()
            } else {
                String::new()
            };
            let frozen: std::collections::HashMap<i64, serde_yaml::Value> = if raw.is_empty() {
                Default::default()
            } else {
                serde_yaml::from_str(&raw)?
            };
            let frozen_ids: std::collections::HashSet<i64> = frozen.into_keys().collect();
            let mut effective_query = query.clone();
            effective_query.filter.frozen_ids = Some(frozen_ids.clone());
            let records_total = records.len() as u64;
            let records_filtered = if effective_query.filter.is_unrestricted() {
                records_total
            } else {
                records.values().filter(|record| record_matches_filter(record, &effective_query.filter)).count() as u64
            };
            let page_records = crate::platform::repository::query_records(records.values(), &effective_query);
            if !page_records.is_empty() {
                if let Some(connection) = connection {
                    crate::native::sqlite::SqliteNovelRepository::check_metadata_prefixes_open(
                        connection, &page_records,
                    )?;
                } else {
                    let archive_root = inventory.root_dir().join(crate::downloader::types::ARCHIVE_ROOT_DIR);
                    let gate_dir = archive_root.join(".objects/.narou-transfer/.gate");
                    if gate_dir.is_dir() && std::fs::read_dir(&gate_dir)?.next().is_some() {
                        let store = crate::native::object_store::NativeObjectStore::from_root(archive_root)?;
                        for record in &page_records {
                            let prefix = crate::platform::ObjectPrefix::from(
                                crate::platform::NovelObjectKeys::new(
                                    &record.sitename, &record.file_title, record.use_subdirectory,
                                )?.prefix().clone(),
                            );
                            if let Some(gate) = crate::native::object_store::read_durable_gate(&store.gate_path(&prefix)?)?
                                && (gate.owner.is_some() || gate.migrated)
                            {
                                return Err(crate::platform::access_fence::gate_closed_error(&prefix));
                            }
                        }
                    }
                }
            }
            Ok(Some(crate::platform::repository::NovelMetadataPage {
                records_total, records_filtered, records: page_records, frozen_ids,
            }))
        }))
    }
    pub fn find_by_toc_url_current_sync(&self,url:&str)->Result<Option<NovelRecord>>{crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,connection|{if let Some(connection)=connection.filter(|_|!db.metadata_is_yaml_in_snapshot(connection)){return crate::native::sqlite::SqliteNovelRepository::url_snapshot_conn(connection,url);}let folded=crate::db::novel_codec::fold(url);Ok(load_current_yaml_records(inventory)?.into_values().find(|record|crate::db::novel_codec::fold(&record.toc_url)==folded))}))}
    pub fn find_by_storage_prefix_current_sync(&self,prefix:&crate::platform::ObjectKey)->Result<Option<NovelRecord>>{crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,connection|{if let Some(connection)=connection.filter(|_|!db.metadata_is_yaml_in_snapshot(connection)){return crate::native::sqlite::SqliteNovelRepository::prefix_snapshot_conn(connection,prefix);}for record in load_current_yaml_records(inventory)?.into_values(){let keys=crate::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if keys.prefix()==prefix{return Ok(Some(record));}}Ok(None)}))}
    pub fn find_by_title_current_sync(&self,title:&str)->Result<Option<NovelRecord>>{crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,connection|{if let Some(connection)=connection.filter(|_|!db.metadata_is_yaml_in_snapshot(connection)){return crate::native::sqlite::SqliteNovelRepository::title_snapshot_conn(connection,title);}let folded=crate::db::novel_codec::fold(title);Ok(load_current_yaml_records(inventory)?.into_values().find(|record|crate::db::novel_codec::fold(&record.title)==folded))}))}
    pub fn find_by_ncode_current_sync(&self,ncode:&str)->Result<Option<NovelRecord>>{crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,connection|{if let Some(connection)=connection.filter(|_|!db.metadata_is_yaml_in_snapshot(connection)){return crate::native::sqlite::SqliteNovelRepository::ncode_snapshot_conn(connection,ncode);}let folded=crate::db::novel_codec::fold(ncode);let suffix=format!("/{folded}");Ok(load_current_yaml_records(inventory)?.into_values().find(|record|record.ncode.as_deref().is_some_and(|value|crate::db::novel_codec::fold(value)==folded)||crate::db::novel_codec::fold(&record.toc_url).trim_end_matches('/').ends_with(&suffix)))}))}

    /// Synchronous entry points for native-only callers (CLI commands).
    /// These never spawn blocking work — they lock the shared database
    /// state directly, matching the old `with_database` behaviour.
    pub fn get_sync(&self, id: NovelId) -> Result<Option<NovelRecord>> {
        crate::db::with_database(|db| Ok(db.get(id.0).cloned()))
    }

    /// Mutate an existing authoritative row with the original operation epoch.
    /// Lock order is per-novel OS gate(s), then DATABASE and metadata locks.
    pub fn update_existing_guarded_sync<F>(&self,fence:&crate::platform::WriteFence,id:NovelId,mutator:F)->Result<bool>
    where F:FnOnce(&mut NovelRecord)->Result<()>{
        let store=current_store()?;store.with_open_write(fence,||crate::db::with_database_mut(|db|db.update_records_with_fences(std::slice::from_ref(fence),|mut records|{
            let found=if let Some(record)=records.get_mut(&id.0){require_record_origin(record,std::slice::from_ref(fence))?;mutator(record)?;require_record_origin(record,std::slice::from_ref(fence))?;true}else{false};Ok((records,found))
        })))
    }

    pub fn find_by_toc_url_sync(&self, url: &str) -> Result<Option<NovelRecord>> {
        crate::db::with_database(|db| Ok(db.get_by_toc_url(url).cloned()))
    }

    pub fn find_by_title_sync(&self, title: &str) -> Result<Option<NovelRecord>> {
        crate::db::with_database(|db| Ok(db.find_by_title(title).cloned()))
    }

    pub fn find_by_ncode_sync(&self, ncode: &str) -> Result<Option<NovelRecord>> {
        Self::find_by_ncode_sync_impl(ncode)
    }


    pub fn apply_batch_sync(&self, mut mutations: Vec<NovelMutation>) -> Result<()> {
        initialize_attributes(&mut mutations);
        crate::db::with_database_mut(|db| {
            db.update_records(
                |mut records: std::collections::BTreeMap<i64, NovelRecord>| {
                    for mutation in &mutations {
                        match mutation {
                            NovelMutation::Upsert(record) => {
                                let id = record.id;
                                records.insert(id, record.clone());
                            }
                            NovelMutation::Remove(id) => {
                                records.remove(&id.0);
                            }
                        }
                    }
                    Ok((records, ()))
                },
            )
        })
    }

    pub fn query_sync(&self, query: &NovelQuery) -> Result<Vec<NovelRecord>> {
        crate::db::with_database(|db| {
            Ok(crate::platform::repository::query_records(db.all_records().values(), query))
        })
    }

    pub fn scan_ids_sync(
        &self,
        filter: &NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> Result<Vec<NovelId>> {
        crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,connection|match connection.filter(|_|!db.metadata_is_yaml_in_snapshot(connection)){
            Some(connection)=>crate::native::sqlite::SqliteNovelRepository::scan_snapshot_conn(connection,filter,after_id,limit),
            None=>{let after=after_id.map_or(i64::MIN,|id|id.0);Ok(load_current_yaml_records(inventory)?.values().filter(|record|record.id>after&&record_matches_filter(record,filter)).take(limit).map(|record|NovelId(record.id)).collect())}
        }))
    }

    pub fn count_sync(&self, filter: &NovelFilter) -> Result<u64> {
        crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,connection|match connection.filter(|_|!db.metadata_is_yaml_in_snapshot(connection)){Some(connection)=>crate::native::sqlite::SqliteNovelRepository::count_snapshot_conn(connection,filter),None=>Ok(load_current_yaml_records(inventory)?.values().filter(|record|record_matches_filter(record,filter)).count() as u64)}))
    }

    /// Resolve by ncode with the legacy `toc_url` suffix fallback used by
    /// `commands::update::resolve_ncode_to_id`.
    fn find_by_ncode_sync_impl(ncode: &str) -> Result<Option<NovelRecord>> {
        let ncode = ncode.to_lowercase();
        crate::db::with_database(|db| {
            Ok(db.all_records().values().find(|record| {
                record.ncode.as_deref().is_some_and(|nc| nc.eq_ignore_ascii_case(&ncode))
                    || record
                        .toc_url
                        .to_lowercase()
                        .trim_end_matches('/')
                        .ends_with(&format!("/{ncode}"))
            }).cloned())
        })
    }
}

/// Run a blocking database operation, isolating it on a blocking thread when
/// a tokio runtime is available. Never holds the `parking_lot` database lock
/// across an `.await`.
async fn run_db<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle
            .spawn_blocking(f)
            .await
            .map_err(|e| NarouError::Database(format!("repository task failed: {e}")))?,
        Err(_) => f(),
    }
}

/// Count tags and find their earliest novel without decoding whole SQLite rows.
pub(crate) type TagInventory = std::collections::HashMap<String, (usize, i64)>;

pub(crate) async fn sqlite_tag_inventory(attributes: bool) -> Result<Option<TagInventory>> {
    use crate::db::novel_query::{ATTRIBUTE_NAMES_SQL, TAG_NAMES_SQL};
    run_db(move || {
        crate::db::with_database(|db| db.with_library_metadata_snapshot(|_, connection| {
            let Some(conn) = connection.filter(|_| !db.metadata_is_yaml_in_snapshot(connection)) else {
                return Ok(None);
            };
            use crate::native::sqlite::sqlite_error;
            let combined;
            let query = if attributes {
                // The normal aggregate's ORDER BY stays inside its subquery.
                combined = format!("SELECT * FROM ({TAG_NAMES_SQL}) UNION ALL {ATTRIBUTE_NAMES_SQL}");
                combined.as_str()
            } else {
                TAG_NAMES_SQL
            };
            let mut statement = conn.prepare(query).map_err(sqlite_error)?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
            }).map_err(sqlite_error)?;
            let mut inventory = TagInventory::new();
            for row in rows {
                let (tag, count, first) = row.map_err(sqlite_error)?;
                let info = inventory.entry(tag).or_insert((0, first));
                info.0 += count as usize;
                info.1 = info.1.min(first);
            }
            Ok(Some(inventory))
        }))
    }).await
}

impl NovelRepository for NativeNovelRepository {
    fn resume_id(&self,id:NovelId,generation:i64)->PlatformFuture<'_,Result<crate::platform::NovelIdReservation>>{Box::pin(run_db(move||crate::db::with_database_mut(|db|db.resume_id(id,generation))))}
    fn reserve_create<'a>(&'a self,fence:&'a crate::platform::WriteFence,reservation:&'a crate::platform::NovelIdReservation,toc_url:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{crate::native::object_store::run_scoped_blocking(||self.reserve_native_creation(fence,reservation,toc_url))})}
    fn apply_create_guarded<'a>(&'a self,fence:&'a crate::platform::WriteFence,reservation:&'a crate::platform::NovelIdReservation,mut record:NovelRecord)->PlatformFuture<'a,Result<()>>{Box::pin(async move{crate::native::object_store::run_scoped_blocking(||{
        crate::application::attribute_tags::initialize_record(&mut record, &crate::downloader::site_setting::effective_site_settings());
        let store=current_store()?;store.with_open_write(fence,||crate::db::with_database_mut(|db|db.apply_create_record(fence,reservation,record)))
    })})}
    fn remove_closed<'a>(&'a self,prefix:&'a crate::platform::ObjectPrefix,owner:&'a str,epoch:u64,id:NovelId)->PlatformFuture<'a,Result<()>>{let prefix=prefix.clone();let owner=owner.to_owned();Box::pin(run_db(move||{
        if crate::platform::access_fence::parse_removal_owner(&owner)?.0!=id{return Err(NarouError::Platform("removal owner ID mismatch".into()));}let store=current_store()?;store.with_closed_write(&prefix,&owner,epoch,||crate::db::with_database_mut(|db|db.remove_closed_record(&prefix,&owner,epoch,id)))
    }))}
    fn get_current<'a>(&'a self,id:NovelId)->PlatformFuture<'a,Result<Option<NovelRecord>>>{Box::pin(run_db(move||Self::new().get_current_sync(id)))}
    fn get_many_current<'a>(&'a self,ids:&'a [NovelId])->PlatformFuture<'a,Result<Vec<Option<NovelRecord>>>>{let ids=ids.to_vec();Box::pin(run_db(move||Self::new().get_many_current_sync(&ids)))}
    fn query_current<'a>(&'a self,query:&'a NovelQuery)->PlatformFuture<'a,Result<Vec<NovelRecord>>>{let query=query.clone();Box::pin(run_db(move||Self::new().query_current_sync(&query)))}
    fn metadata_page_snapshot<'a>(
        &'a self,
        query: &'a NovelQuery,
    ) -> PlatformFuture<'a, Result<Option<crate::platform::repository::NovelMetadataPage>>> {
        let query = query.clone();
        Box::pin(run_db(move || Self::new().metadata_page_snapshot_sync(&query)))
    }
    fn find_by_toc_url_current<'a>(&'a self,url:&'a str)->PlatformFuture<'a,Result<Option<NovelRecord>>>{let url=url.to_owned();Box::pin(run_db(move||Self::new().find_by_toc_url_current_sync(&url)))}
    fn find_by_storage_prefix_current<'a>(&'a self,prefix:&'a crate::platform::ObjectKey)->PlatformFuture<'a,Result<Option<NovelRecord>>>{let prefix=prefix.clone();Box::pin(run_db(move||Self::new().find_by_storage_prefix_current_sync(&prefix)))}
    fn apply_batch_guarded_many<'a>(&'a self,fences:&'a [crate::platform::WriteFence],mutations:Vec<NovelMutation>)->PlatformFuture<'a,Result<()>>{
        let fences=fences.to_vec();Box::pin(async move{run_db(move||{
            let mut mutations=mutations;initialize_attributes(&mut mutations);
            if fences.is_empty()&&!mutations.is_empty(){return Err(NarouError::Platform("record mutation has no writer fence".into()));}
            let store=current_store()?;
            store.with_open_writes(&fences,||crate::db::with_database_mut(|db|db.update_records_with_fences(&fences,|mut records|{
                    for mutation in &mutations{let id=match mutation{NovelMutation::Upsert(record)=>record.id,NovelMutation::Remove(id)=>id.0};if let Some(previous)=records.get(&id){let keys=crate::platform::NovelObjectKeys::new(&previous.sitename,&previous.file_title,previous.use_subdirectory)?;if !fences.iter().any(|fence|keys.prefix().as_ref()==fence.prefix.as_ref().trim_end_matches('/')){return Err(NarouError::Platform("previous record origin outside captured fence".into()));}}}
                    if mutations.iter().any(|mutation|matches!(mutation,NovelMutation::Remove(_)))&&let crate::native::object_store::NativeStore::Fs(fs)=&store{for fence in &fences{fs.invalidate_writer_locked(fence)?;}}
                    for mutation in &mutations{let record=match mutation{NovelMutation::Upsert(record)=>Some(record),NovelMutation::Remove(id)=>records.get(&id.0)};
                        if let Some(record)=record{let keys=crate::platform::NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;if !fences.iter().any(|fence|keys.prefix().as_ref()==fence.prefix.as_ref().trim_end_matches('/')){return Err(NarouError::Platform("record mutation outside captured fence".into()));}}
                        match mutation{NovelMutation::Upsert(record)=>{records.insert(record.id,record.clone());},NovelMutation::Remove(id)=>{records.remove(&id.0);}}
                    }Ok((records,()))
            })))
        }).await})
    }

    fn get<'a>(&'a self, id: NovelId) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        Box::pin(async move {
            run_db(move || {
                crate::db::with_database(|db| Ok(db.get(id.0).cloned()))
            })
            .await
        })
    }

    fn find_by_toc_url<'a>(
        &'a self,
        url: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let url = url.to_string();
        Box::pin(async move {
            run_db(move || {
                crate::db::with_database(|db| Ok(db.get_by_toc_url(&url).cloned()))
            })
            .await
        })
    }

    fn contains_toc_urls<'a>(&'a self, urls: &'a [String]) -> PlatformFuture<'a, Result<Vec<bool>>> {
        let urls = urls.to_vec();
        Box::pin(async move {
            run_db(move || crate::db::with_database(|db| Ok(urls.iter().map(|url| db.get_by_toc_url(url).is_some()).collect()))).await
        })
    }

    fn find_by_title<'a>(
        &'a self,
        title: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let title=title.to_owned();Box::pin(run_db(move||Self::new().find_by_title_current_sync(&title)))
    }

    fn find_by_ncode<'a>(
        &'a self,
        ncode: &'a str,
    ) -> PlatformFuture<'a, Result<Option<NovelRecord>>> {
        let ncode=ncode.to_owned();Box::pin(run_db(move||Self::new().find_by_ncode_current_sync(&ncode)))
    }

    fn count<'a>(
        &'a self,
        filter: &'a NovelFilter,
    ) -> PlatformFuture<'a, Result<u64>> {
        let filter = filter.clone();
        Box::pin(run_db(move||Self::new().count_sync(&filter)))
    }

    fn query<'a>(
        &'a self,
        query: &'a NovelQuery,
    ) -> PlatformFuture<'a, Result<Vec<NovelRecord>>> {
        let query = query.clone();
        Box::pin(async move {
            run_db(move || NativeNovelRepository.query_sync(&query)).await
        })
    }

    fn scan_ids<'a>(
        &'a self,
        filter: &'a NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<NovelId>>> {
        let filter = filter.clone();
        Box::pin(run_db(move||Self::new().scan_ids_sync(&filter,after_id,limit)))
    }

    fn allocate_id(&self) -> PlatformFuture<'_, Result<crate::platform::NovelIdReservation>> {
        Box::pin(async move {
            run_db(move || crate::db::with_database_mut(|db| db.allocate_id())).await
        })
    }

    fn release_id(&self, reservation: crate::platform::NovelIdReservation) -> PlatformFuture<'_, Result<()>> {
        Box::pin(async move {
            run_db(move || Self::new().release_native_creation(reservation)).await
        })
    }

    fn apply_batch<'a>(
        &'a self,
        mutations: Vec<NovelMutation>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            run_db(move || {
                let mut mutations = mutations;
                initialize_attributes(&mut mutations);
                crate::db::with_database_mut(|db| {
                    db.update_records(
                        |mut records: std::collections::BTreeMap<i64, NovelRecord>| {
                            for mutation in &mutations {
                                match mutation {
                                    NovelMutation::Upsert(record) => {
                                        let id = record.id;
                                        records.insert(id, record.clone());
                                    }
                                    NovelMutation::Remove(id) => {
                                        records.remove(&id.0);
                                    }
                                }
                            }
                            Ok((records, ()))
                        },
                    )
                })
            })
            .await
        })
    }
}

/// Synchronous adapter for `application::target_lookup` (native CLI paths).
impl NovelLookup for NativeNovelRepository {
    fn get(&self, id: NovelId) -> Result<Option<NovelRecord>> {
        self.get_current_sync(id)
    }

    fn find_by_toc_url(&self, url: &str) -> Result<Option<NovelRecord>> {
        self.find_by_toc_url_current_sync(url)
    }

    fn find_by_ncode(&self, ncode: &str) -> Result<Option<NovelRecord>> {
        self.find_by_ncode_current_sync(ncode)
    }

    fn find_by_title(&self, title: &str) -> Result<Option<NovelRecord>> {
        self.find_by_title_current_sync(title)
    }

    fn scan_ids(
        &self,
        filter: &NovelFilter,
        after_id: Option<NovelId>,
        limit: usize,
    ) -> Result<Vec<NovelId>> {
        self.scan_ids_sync(filter, after_id, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::NativeNovelRepository;
    use crate::db::{self, Database};
    use crate::db::novel_record::NovelRecord;
    use crate::platform::{NovelFilter, NovelMutation, NovelQuery, NovelRepository};
    use chrono::Utc;
    use std::collections::{BTreeMap, HashSet};
    use std::sync::Arc;

    struct DatabaseGuard(Option<Database>);

    impl Drop for DatabaseGuard {
        fn drop(&mut self) {
            *db::DATABASE.lock() = self.0.take();
        }
    }

    fn sample_record(id: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: format!("author-{id}"),
            title: format!("title-{id}"),
            file_title: format!("file-{id}"),
            toc_url: format!("https://example.com/{id}/"),
            sitename: "Example".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: vec!["test".to_string()],
            ncode: Some(format!("n{id}")),
            domain: Some("example.com".to_string()),
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: BTreeMap::new(),
        }
    }

    fn isolated_database() -> (tempfile::TempDir, crate::test_support::CurrentDirGuard, DatabaseGuard) {
        let temp = tempfile::tempdir().unwrap();
        let cwd_guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let mut slot = db::DATABASE.lock();
        let previous = slot.take();
        drop(slot);
        let guard = DatabaseGuard(previous);
        db::init_database().unwrap();
        (temp, cwd_guard, guard)
    }

    #[test]
    fn metadata_page_native_uses_atomic_snapshot_without_leases_across_all_modes() {
        let _global = crate::test_support::global_state_guard();
        for (mode, compat) in [("yaml", false), ("yaml", true), ("sqlite", true), ("sqlite", false)] {
            let temp = tempfile::tempdir().unwrap();
            let _cwd = crate::test_support::set_current_dir_for_test(temp.path());
            std::fs::create_dir(temp.path().join(".narou")).unwrap();
            std::fs::write(temp.path().join(".narou/storage-backend"), mode).unwrap();
            std::fs::write(
                temp.path().join(".narou/local_setting.yaml"),
                format!("narou-compat: {compat}\n"),
            ).unwrap();
            let _database = DatabaseGuard(db::DATABASE.lock().take());
            db::init_database().unwrap();
            let mut record = sample_record(1);
            record.title = format!("{mode}-{compat}");
            let (record, _write) = crate::native::materialization::create_test_record(record).unwrap();
            let inventory = Arc::new(crate::db::inventory::Inventory::new(temp.path().to_path_buf()));
            inventory.save(
                "freeze", crate::db::inventory::InventoryScope::Local,
                &BTreeMap::from([(record.id, false), (999, true)]),
            ).unwrap();
            let novels = NativeNovelRepository::new();
            let query = NovelQuery::page(NovelFilter::all(), Default::default(), 0, 50);
            let snapshot = novels.metadata_page_snapshot_sync(&query).unwrap()
                .unwrap_or_else(|| panic!("expected snapshot for {mode}, compat={compat}"));
            assert_eq!((snapshot.records_total, snapshot.records_filtered), (1, 1));
            assert_eq!(snapshot.records[0].title, record.title);
            // The native payload, not the FK-limited search projection, is
            // authoritative; presence counts even when its value is false.
            assert_eq!(snapshot.frozen_ids, HashSet::from([record.id, 999]));
            let library = crate::application::LibraryService::new(
                Arc::new(novels),
                Arc::new(crate::platform::clock::SystemClock),
                Arc::new(crate::native::application::NativeFreezeStore::new(inventory)),
                Arc::new(crate::application::events::EmptySiteTimezoneProvider),
            );
            let objects = crate::native::materialization::read_store(temp.path()).unwrap();
            let prefix = crate::platform::ObjectPrefix::from(
                crate::platform::NovelObjectKeys::new(
                    &record.sitename, &record.file_title, record.use_subdirectory,
                ).unwrap().prefix().clone(),
            );
            let request = crate::application::LibraryListRequest {
                search: None, start: 0, length: Some(50),
                sort_column: crate::application::LibrarySortColumn::Id,
                sort_order: crate::application::LibrarySortOrder::Ascending,
                hidden: None, include_hidden: true, exclude_ids: Vec::new(),
                attribute_tags_enabled: false,
            };
            let gate_dir = temp.path().join("小説データ/.objects/.narou-transfer/.gate");
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async {
                let mut scope = crate::application::novel_access::NovelReadScope::new(&objects, "native-list-test");
                let page = library.list(&mut scope, &request).await.unwrap();
                assert_eq!(page.data[0].title, record.title);
                assert!(page.data[0].frozen);
                let gate = objects.access_fence().unwrap().gate_info(&prefix).await.unwrap();
                assert_eq!(gate.pending_readers, 0, "no reader leases in {mode}, compat={compat}");
                scope.release_all().await.unwrap();

                let mut frozen_req = request.clone();
                frozen_req.search = Some("status:凍結".to_string());
                let mut scope = crate::application::novel_access::NovelReadScope::new(&objects, "native-frozen-filter");
                let frozen_page = library.list(&mut scope, &frozen_req).await.unwrap();
                assert_eq!(frozen_page.records_filtered, 1, "status:凍結 filter in {mode}, compat={compat}");
                scope.release_all().await.unwrap();

                objects.access_fence().unwrap().close_gate(&prefix, "test-publication").await.unwrap();
                let mut scope = crate::application::novel_access::NovelReadScope::new(&objects, "native-closed-list");
                let error = library.list(&mut scope, &request).await.expect_err("selected closed namespace");
                assert!(error.to_string().contains("novel_recovering:"), "{error}");
                scope.release_all().await.unwrap();

                if mode == "yaml" {
                    let _ = std::fs::remove_dir_all(&gate_dir);
                    let mut scope = crate::application::novel_access::NovelReadScope::new(&objects, "native-no-gate");
                    let page = library.list(&mut scope, &request).await.unwrap();
                    assert_eq!(page.records_total, 1);
                    assert!(!gate_dir.exists(), "YAML metadata list must not create gate files");
                    scope.release_all().await.unwrap();
                }
            });
            if mode == "sqlite" && !compat {
                let state = crate::native::sqlite::state::active_for(&temp.path().join(".narou")).unwrap();
                state.conn().lock().execute(
                    "UPDATE app_state SET value_yaml='[' WHERE scope='inv' AND key='freeze'", [],
                ).unwrap();
                assert!(NativeNovelRepository::new().metadata_page_snapshot_sync(&query).is_err(),
                    "a stale relational freeze projection must not hide a malformed canonical payload");
            } else {
                std::fs::write(temp.path().join(".narou/freeze.yaml"), "[").unwrap();
                assert!(NativeNovelRepository::new().metadata_page_snapshot_sync(&query).is_err(),
                    "malformed freeze.yaml must fail closed in {mode}, compat={compat}");
            }
        }
    }

    #[test]
    fn yaml_round_trip_preserves_record_shape_and_unknown_fields() {
    let _global = crate::test_support::global_state_guard();
    let _legacy = crate::test_support::legacy_yaml_guard();
        let (temp, _cwd_guard, _db_guard) = isolated_database();
        std::fs::write(
            temp.path().join(".narou").join("database.yaml"),
            r#"0:
  id: 999
  author: author
  title: title
  file_title: file title
  toc_url: https://example.com/0/
  sitename: Example
  novel_type: 1
  end:
  last_update: 2026-04-20 00:00:00.000000000 +09:00
  suspend: "yes"
  is_narou: 1
  tags:
    - test
  raw_title: raw title
  custom_flag: true
  nested:
    answer: 42
"#,
        )
        .unwrap();
        db::with_database_mut(|db| db.refresh()).unwrap();

        let novels = NativeNovelRepository::new();
        let mut record = novels.get_sync(0.into()).unwrap().unwrap();
        assert_eq!(record.id, 0);
        assert!(!record.end);
        assert!(record.suspend);
        assert!(record.is_narou);
        assert_eq!(record.raw_title(), "raw title");
        assert_eq!(
            record.extra_fields.get("nested").and_then(|value| {
                value
                    .get("answer")
                    .and_then(serde_yaml::Value::as_i64)
            }),
            Some(42)
        );

        record.title = "updated".to_string();
        novels
            .apply_batch_sync(vec![NovelMutation::Upsert(record)])
            .unwrap();

        let raw = db::with_database(|db| {
            db.inventory()
                .load_raw("database", crate::db::inventory::InventoryScope::Local)
        })
        .unwrap();
        assert!(raw.contains("custom_flag: true"));
        assert!(raw.contains("answer: 42"));
        assert!(raw.contains("title: updated"));

        *db::DATABASE.lock() = None;
        db::init_database().unwrap();
        let reloaded = novels.get_sync(0.into()).unwrap().unwrap();
        assert_eq!(reloaded.title, "updated");
        assert!(!reloaded.end);
        assert!(reloaded.suspend);
        assert_eq!(reloaded.raw_title(), "raw title");
    }

    #[test]
    fn async_allocations_are_unique_and_persisted_under_concurrency() {
        let (_temp, _cwd_guard, _db_guard) = isolated_database();
        let novels = Arc::new(NativeNovelRepository::new());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();

        let ids = runtime.block_on(async {
            futures::future::join_all((0..32).map(|_| {
                let novels = Arc::clone(&novels);
                async move {
                    let reservation = novels.allocate_id().await.map_err(|err| err.to_string())?;
                    let id = reservation.id();
                    novels
                        .apply_batch(vec![NovelMutation::Upsert(sample_record(id.0))])
                        .await
                        .map_err(|err| err.to_string())?;
                    Ok::<i64, String>(id.0)
                }
            }))
            .await
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
        })
        .unwrap();
        drop(runtime);

        let unique: HashSet<_> = ids.iter().copied().collect();
        assert_eq!(unique.len(), 32);
        assert_eq!(unique, (0..32).collect());

        let records = novels
            .query_sync(&NovelQuery::page(
                NovelFilter::all(),
                Default::default(),
                0,
                64,
            ))
            .unwrap();
        assert_eq!(records.len(), 32);
    }
}
