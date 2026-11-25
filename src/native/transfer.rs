//! Native local archive codec and client-driven transfer v2 adapters.
//!
//! CLI ZIP processing stays local. Its readers use coherent private leased
//! snapshots and canonical writes retain the original writer epoch.
//! Web v2 exposes only bounded metadata and raw-byte sessions; snapshot,
//! publication, recovery and owner GC use durable indexed journals.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::io::{Read, Write};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::db::{self, NovelRecord};
use crate::downloader::settings::{DownloaderSettings, NativeDownloaderSettings};
use crate::downloader::site_setting;
use crate::downloader::persistence::TocMetadata;
use crate::downloader::util::{load_length_limit, sanitize_filename_with_limit};
use crate::error::{NarouError, Result};
use crate::native::materialization::block_on;
use crate::native::novel_repository::NativeNovelRepository;
use crate::platform::{AssetStore, AssetStream, NovelId, NovelMutation, NovelObjectKeys, ObjectKey, StagedAsset};
use crate::platform::NovelRepository;
use crate::application::novel_access::{NovelReadLease, NovelWriteGuard};
use crate::transfer::{
    ArchiveFile, ArchiveNovel, ConflictPolicy, ExistingNovel, ImportAction, StagedNovel,
    StagedArchive, bounded_asset_stream, plan_manifest,
};
use crate::version;

#[path = "transfer/journal.rs"]
mod journal;
pub(crate) use journal::sanitize_control_snapshot;
#[path = "transfer/claims.rs"]
mod claims;
pub(crate) use claims::{RecordClaimGuard,record_claim_guard_conn,reserve_ordinary_claim_conn,consume_ordinary_claim_conn,release_ordinary_claim_conn,ensure_no_unpublished_creations_conn,assert_removal_identity,assert_physical_current_namespace,assert_physical_records};

/// Top-level novel-directory entries that are never exported.
///
/// `backup/` is what `narou backup` itself excludes (`compat::create_backup`);
/// lock and temporary files are runtime artifacts of the inventory writers and
/// would make the archive non-deterministic.
const EXCLUDED_TOP_LEVEL_DIRS: &[&str] = &["backup"];

/// The single file every novel directory must contain for a transfer.
const TOC_FILE: &str = "toc.yaml";

/// Summary of one exported novel (used for CLI output).
#[derive(Debug, Clone)]
pub struct ExportedNovelInfo {
    pub id: i64,
    pub title: String,
    pub file_count: usize,
}

/// Outcome of an import run.
///
/// Per-novel failures are collected in `errors`; the run continues so one
/// broken archive entry does not hide the rest. Callers must treat a non-empty
/// `errors` as a failed command (non-zero exit / `success: false`).
#[derive(Debug, Clone, Default)]
pub struct ImportOutcome {
    pub created: usize,
    pub overwritten: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

impl ImportOutcome {
    pub fn success(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Stream a complete archive to an explicitly chosen native sink.
pub fn export_archive_to(ids: &[i64], sink: &mut dyn Write) -> Result<Vec<ExportedNovelInfo>> {
    export_to(ids, sink, &mut |_, _, _| Ok(()))
}

fn export_to(ids:&[i64],sink:&mut dyn Write,report:&mut ImportProgress<'_>)->Result<Vec<ExportedNovelInfo>>{
    if ids.is_empty(){return Err(NarouError::InvalidTarget("書き出す小説が指定されていません".into()));}
    let repository=Arc::new(NativeNovelRepository::new());let inventory=crate::db::inventory::Inventory::with_default_root()?;let archive_root=inventory.root_dir().join("小説データ");let mut seen=HashSet::new();let ids=ids.iter().copied().filter(|id|seen.insert(*id)).collect::<Vec<_>>();
    // Identity initialization is a write, and precedes every retained admission.
    for id in &ids{let record=block_on(repository.get_current(NovelId(*id)))?.ok_or_else(||NarouError::NotFound(format!("ID:{id}")))?;if crate::virtual_novel::is_virtual_record(&record){crate::native::virtual_novel::ensure_portable_identity(*id)?;}}
    let objects:Arc<dyn crate::platform::ObjectStore>=Arc::new(crate::native::object_store::NativeStore::for_current_root()?);let content=crate::application::NovelContentService::new(repository,objects.clone());let mut scope=crate::application::novel_access::NovelReadScope::new(&objects,"cli-transfer-export");
    let result=(||->Result<Vec<ExportedNovelInfo>>{
        // Admit all requested parents before consuming their membership metadata.
        for id in &ids{let record=block_on(async{content.read_record(&mut scope,NovelId(*id)).await.map_err(|error|NarouError::Platform(error.to_string()))})?.ok_or_else(||NarouError::NotFound(format!("ID:{id}")))?;assert_source_namespace(&record)?;}
        let membership=crate::native::virtual_novel::load()?;let mut ordinary=Vec::new();let mut parents=Vec::new();let mut expanded=HashSet::new();
        for id in &ids{
            let record=block_on(async{content.read_record(&mut scope,NovelId(*id)).await.map_err(|error|NarouError::Platform(error.to_string()))})?.ok_or_else(||NarouError::NotFound(format!("ID:{id}")))?;
            if !crate::virtual_novel::is_virtual_record(&record){if expanded.insert(*id){ordinary.push(*id);}continue;}
            let members=membership.get(*id).ok_or_else(||NarouError::NotFound(format!("ID:{id} のバーチャル小説にメンバーがありません")))?;
            for member in members{let record=block_on(async{content.read_record(&mut scope,NovelId(*member)).await.map_err(|error|NarouError::Platform(error.to_string()))})?.ok_or_else(||NarouError::NotFound(format!("メンバー ID:{member}")))?;if crate::virtual_novel::is_virtual_record(&record){return Err(NarouError::InvalidTarget("バーチャル小説をメンバーにはできません".into()));}assert_source_namespace(&record)?;if expanded.insert(*member){ordinary.push(*member);}}
            parents.push((*id,members.to_vec()));
        }
        let total=ordinary.len()+parents.len();let mut writer=crate::transfer::ArchiveStream::new(sink,version::VERSION.into(),Utc::now().to_rfc3339_opts(SecondsFormat::Secs,true));let mut info=Vec::new();
        for id in ordinary{
            let record=block_on(async{content.read_record(&mut scope,NovelId(id)).await.map_err(|error|NarouError::Platform(error.to_string()))})?.ok_or_else(||NarouError::NotFound(format!("ID:{id}")))?;let keys=NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;let mut lease=block_on(scope.lease_for(&crate::platform::ObjectPrefix::from(keys.prefix().clone())))?;
            ensure_exportable(&archive_root,&db::existing_novel_dir_for_record(&archive_root,&record))?;let snapshot=block_on(crate::native::materialization::snapshot_leased(&objects,&mut lease,inventory.root_dir()))?;
            let current=block_on(async{content.read_record(&mut scope,NovelId(id)).await.map_err(|error|NarouError::Platform(error.to_string()))})?.ok_or_else(||NarouError::NotFound(format!("ID:{id}")))?;if current!=record{return Err(NarouError::Platform("novel_busy: metadata changed during archive snapshot".into()));}
            let toc_path=snapshot.path().join(TOC_FILE);if fs::metadata(&toc_path)?.len()>16*1024*1024{return Err(NarouError::Platform("TOC metadata exceeds16MiB".into()));}let story=crate::downloader::persistence::deserialize_toc_story(&fs::read(toc_path)?)?;
            writer.begin_novel(archive_novel_from_record(&record,Vec::new(),story))?;let mut file_count=0;for(relative,path)in collect_novel_files(snapshot.path())?{writer.add_reader(&relative,&mut fs::File::open(path)?)?;file_count+=1;}
            info.push(ExportedNovelInfo{id,title:record.title.clone(),file_count});report(info.len(),total,&record.title)?;block_on(scope.refresh_all())?;
        }
        for(id,members)in &parents{
            let record=block_on(async{content.read_record(&mut scope,NovelId(*id)).await.map_err(|error|NarouError::Platform(error.to_string()))})?.ok_or_else(||NarouError::NotFound(format!("ID:{id}")))?;if !crate::virtual_novel::is_portable_toc_url(&record.toc_url){return Err(NarouError::Platform("novel_busy: parent identity changed".into()));}let mut urls=Vec::with_capacity(members.len());let mut bytes=record.title.len()+record.author.len()+record.toc_url.len();
            for member in members{let current=block_on(async{content.read_record(&mut scope,NovelId(*member)).await.map_err(|error|NarouError::Platform(error.to_string()))})?.ok_or_else(||NarouError::NotFound(format!("メンバー ID:{member}")))?;bytes=bytes.checked_add(current.toc_url.len()+64).filter(|size|*size<=crate::application::transfer::MAX_METADATA_BYTES).ok_or_else(||NarouError::Platform("archive member metadata exceeds24MiB".into()))?;urls.push(current.toc_url);}
            let keys=NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;let mut lease=block_on(scope.lease_for(&crate::platform::ObjectPrefix::from(keys.prefix().clone())))?;let(temporary,files)=collect_virtual_input_files(&record,&objects,&mut lease)?;writer.begin_novel(archive_virtual_from_record(&record,&record.toc_url,urls))?;for(relative,path)in &files{writer.add_reader(relative,&mut fs::File::open(path)?)?;}info.push(ExportedNovelInfo{id:*id,title:record.title.clone(),file_count:files.len()});report(info.len(),total,&record.title)?;drop(temporary);block_on(scope.refresh_all())?;
        }
        let current=crate::native::virtual_novel::load()?;for(id,members)in &parents{if current.get(*id)!=Some(members.as_slice()){return Err(NarouError::Platform("novel_busy: parent membership changed during export".into()));}}
        writer.finish()?;Ok(info)
    })();let released=block_on(scope.release_all());match result{Ok(value)=>{released?;Ok(value)},Err(error)=>Err(error)}
}

/// Source admission is metadata-only; callers retain their original read scope
/// for all subsequent payload access. Legacy nested roots are not annexed.
pub(crate) fn assert_source_namespace(record:&NovelRecord)->Result<()>{
    let prefix=crate::platform::ObjectPrefix::from(NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?.prefix().clone());let folded=crate::db::novel_codec::fold(&record.toc_url);crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,primary|{let records=if db.metadata_is_yaml_in_snapshot(primary){None}else{primary};let check=|control:&rusqlite::Connection|claims::assert_source_namespace(inventory,records,control,NovelId(record.id),&prefix,&folded);if let Some(primary)=primary{check(primary)}else{let control=control_connection(inventory.root_dir())?;let control=control.lock();check(&control)}}))
}

/// Whole-library backup may include unregistered retained bytes only with an
/// exact durable origin and its caller's already-admitted original lease.
pub(crate) fn assert_retained_source_namespace(prefix:&crate::platform::ObjectPrefix)->Result<()>{
    crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,primary|{let records=if db.metadata_is_yaml_in_snapshot(primary){None}else{primary};let check=|control:&rusqlite::Connection|claims::assert_retained_source_namespace(inventory,records,control,prefix);if let Some(primary)=primary{check(primary)}else{let control=control_connection(inventory.root_dir())?;let control=control.lock();check(&control)}}))
}

/// Materializes the per-novel inputs of a virtual parent.
///
/// A parent owns no body: its members keep the text. Only the settings, the
/// replacement rules, the converter options and the cover travel with it.
/// Each input is copied into a private directory, so the archive reads it as a
/// stream instead of holding the whole file in memory.
fn collect_virtual_input_files(
    record:&NovelRecord,store:&Arc<dyn crate::platform::ObjectStore>,lease:&mut NovelReadLease,
) -> Result<(tempfile::TempDir, Vec<(String, PathBuf)>)> {
    let keys = NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    let temporary = tempfile::Builder::new()
        .prefix(".narou-virtual-input-")
        .tempdir_in(inventory.root_dir())
        .map_err(NarouError::Io)?;
    let mut files = Vec::new();
        for name in crate::transfer::VIRTUAL_INPUT_FILES {
            let key=keys.prefix().join(name)?;let destination=temporary.path().join(name);
            if block_on(crate::native::materialization::copy_asset(store,lease,&key,&destination,None))?{files.push((name.to_string(),destination));}
        }
    Ok((temporary, files))
}

/// Reports completed works at novel boundaries; an error stops further imports.
pub type ImportProgress<'a> = dyn FnMut(usize, usize, &str) -> Result<()> + 'a;

/// Receive into an unpublished durable stage, then validate every entry before
/// applying the conflict plan. The reader is never collected into one Vec.
pub fn import_archive_reader<R: Read + Send + 'static>(
    reader: R, policy: ConflictPolicy, mut progress: Option<&mut ImportProgress<'_>>,
    cancelled: Option<Arc<AtomicBool>>,
) -> Result<ImportOutcome> {
    // The archive carries files, so a storage backend that stores nothing on
    // disk cannot represent the result. Refuse instead of publishing records
    // whose body would be missing.
    if !crate::native::sqlite::state::mirror_files_enabled() {
        return Err(NarouError::Platform(
            "このライブラリは sqlite.mirror-files=false (実ファイルを保存しない構成) です。\
             取り込みはファイルを書き出すため、この構成では実行できません。"
                .to_string(),
        ));
    }

    let stores = crate::native::object_store::NativeStores::for_current_root()?;
    let write_guards=capture_import_writers(&stores)?;
    let assets = stores.assets;
    let mut cleanup = StageCleanup { assets: assets.clone(), keys: Vec::new() };
    let source = block_on(async {
        let cancel = cancelled.clone();
        let stream: AssetStream = Box::pin(futures::stream::try_unfold((reader, cancel), |(mut reader, cancel)| async move {
            if cancel.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return Err(NarouError::Platform("transfer cancelled".into()));
            }
            let mut bytes = vec![0; 64 * 1024];
            let count = reader.read(&mut bytes)?;
            if count == 0 { return Ok(None); }
            bytes.truncate(count);
            Ok(Some((bytes, (reader, cancel))))
        }));
        assets.stage_stream(bounded_asset_stream(stream, u64::MAX)).await
    })?;
    cleanup.keys.push(source.key.clone());
    let parsed = block_on(async {
        StagedArchive::read(assets.clone(), &source, &mut cleanup.keys, cancelled.clone()).await.map_err(NarouError::from)
    })?;
    let repository = NativeNovelRepository::new();
    let (existing, records_by_url) = load_library_index(&repository, &parsed.novels)?;
    let plan = plan_manifest(&parsed.manifest, &existing, policy);
    let archive_root = db::with_database(|database| Ok(database.archive_root().to_path_buf()))?;

    let mut outcome = ImportOutcome::default();
    // A single archive may repeat a toc_url; the plan is computed against the
    // library as it was before the run, so track what this run already handled.
    let mut handled_urls: HashSet<String> = HashSet::new();
    // Archive member URL -> the destination record ID of an imported member.
    // A parent is published with these IDs, never with the source ones.
    let mut destinations: HashMap<String, i64> = HashMap::new();

    // The core plan already lists the ordinary members before the parents that
    // reference them, whatever order the manifest used.
    let total = plan.entries.len();
    for (position, entry) in plan.entries.iter().enumerate() {
        if cancelled.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(NarouError::Platform("transfer cancelled".into()));
        }
        let novel = &parsed.novels[entry.index];
        let metadata = &parsed.manifest.novels[entry.index];
        let is_parent = metadata.virtual_members.is_some();
        if let Some(report) = progress.as_mut() {
            report(position, total, &novel.title)?;
        }

        if !handled_urls.insert(novel.toc_url.clone()) {
            outcome.errors.push(format!(
                "{}: 同じ toc_url がアーカイブ内で重複しています",
                describe(novel)
            ));
            continue;
        }

        // A virtual parent and an ordinary work never replace each other.
        if let Some(record) = records_by_url.get(&novel.toc_url)
            && is_parent != crate::virtual_novel::is_virtual_record(record)
        {
            outcome.errors.push(format!(
                "{}: ライブラリにある作品と種類が違います",
                describe(novel)
            ));
            continue;
        }

        let result = match entry.action {
            ImportAction::Skip => {
                outcome.skipped += 1;
                // An existing member still maps to its destination ID, so a
                // parent can reference a member this run did not import.
                if let Some(record) = records_by_url.get(&novel.toc_url) {
                    destinations.insert(novel.toc_url.clone(), record.id);
                }
                continue;
            }
            ImportAction::Create if is_parent => {
                let member_ids = match member_destinations(metadata, &destinations) {
                    Ok(ids) => ids,
                    Err(message) => {
                        outcome.errors.push(format!("{}: {message}", describe(novel)));
                        continue;
                    }
                };
                create_virtual_parent(&repository,&archive_root,metadata,novel,&member_ids)
                    .map(|id| {
                        outcome.created += 1;
                        destinations.insert(novel.toc_url.clone(), id);
                    })
            }
            ImportAction::Create => create_novel(&repository,&archive_root,metadata,novel,assets.as_ref())
                .map(|id| {
                    outcome.created += 1;
                    destinations.insert(novel.toc_url.clone(), id);
                }),
            ImportAction::Overwrite if is_parent => {
                let Some(record) = records_by_url.get(&novel.toc_url) else {
                    outcome.errors.push(format!("{}: ライブラリにありません", describe(novel)));
                    continue;
                };
                let Some(write)=write_guards.get(&novel.toc_url)else{outcome.errors.push(format!("{}: import開始後に保存先が変わりました",describe(novel)));continue;};
                let member_ids = match member_destinations(metadata, &destinations) {
                    Ok(ids) => ids,
                    Err(message) => {
                        outcome.errors.push(format!("{}: {message}", describe(novel)));
                        continue;
                    }
                };
                let id = record.id;
                overwrite_virtual_parent(write,&archive_root, record, metadata, novel, &member_ids)
                    .map(|_| {
                        outcome.overwritten += 1;
                        destinations.insert(novel.toc_url.clone(), id);
                    })
            }
            ImportAction::Overwrite => {
                let Some(record) = records_by_url.get(&novel.toc_url) else {
                    outcome.errors.push(format!("{}: ライブラリにありません", describe(novel)));
                    continue;
                };
                let Some(write)=write_guards.get(&novel.toc_url)else{outcome.errors.push(format!("{}: import開始後に保存先が変わりました",describe(novel)));continue;};
                let id = record.id;
                overwrite_novel(write,&repository, &archive_root, record, metadata, novel, assets.as_ref())
                    .map(|_| {
                        outcome.overwritten += 1;
                        destinations.insert(novel.toc_url.clone(), id);
                    })
            }
        };

        if let Err(error) = result {
            outcome.errors.push(format!("{}: {error}", describe(novel)));
        }
    }
    if let Some(report) = progress.as_mut() {
        report(total, total, "")?;
    }

    Ok(outcome)
}

/// Maps a parent's member references to destination record IDs.
///
/// A member that this run did not import blocks the parent: a group is never
/// published with a missing member.
fn member_destinations(
    metadata: &ArchiveNovel,
    destinations: &HashMap<String, i64>,
) -> std::result::Result<Vec<i64>, String> {
    let members = metadata
        .virtual_members
        .as_ref()
        .ok_or_else(|| "バーチャル小説のメンバー構成がありません".to_string())?;
    let mut ids = Vec::with_capacity(members.len());
    for url in members {
        match destinations.get(url) {
            Some(id) => ids.push(*id),
            None => {
                return Err(format!("メンバー {url} が取り込まれていないため公開しません"));
            }
        }
    }
    Ok(ids)
}

fn describe(novel: &StagedNovel) -> String {
    if novel.title.is_empty() {
        novel.toc_url.clone()
    } else {
        format!("{} ({})", novel.title, novel.toc_url)
    }
}

/// Resolve only archive URLs; importing does not clone the entire library.
fn load_library_index(
    repository: &NativeNovelRepository,
    novels: &[StagedNovel],
) -> Result<(Vec<ExistingNovel>, HashMap<String, NovelRecord>)> {
    let mut existing = Vec::with_capacity(novels.len());
    let mut by_url = HashMap::with_capacity(novels.len());
    for novel in novels {
        if by_url.contains_key(&novel.toc_url) { continue; }
        let Some(record) = repository.find_by_toc_url_sync(&novel.toc_url)? else {
            continue;
        };
        // The first record wins, matching `plan_import`'s rule.
        by_url
            .entry(record.toc_url.clone())
            .or_insert_with(|| record.clone());
        existing.push(ExistingNovel {
            toc_url: record.toc_url,
            title: record.title,
        });
    }
    Ok((existing, by_url))
}

fn ensure_exportable(archive_root: &Path, novel_dir: &Path) -> Result<()> {
    if novel_dir.exists() {
        return Ok(());
    }
    // mirror-files=false leaves a valid managed novel without a folder; only a
    // novel that is absent from the store as well is an error.
    match crate::native::object_store::novel_content_stored(archive_root, novel_dir) {
        Ok(true) => Ok(()),
        Ok(false) => Err(NarouError::NotFound(format!(
            "{} が見つかりません",
            novel_dir.display()
        ))),
        Err(error) => Err(error),
    }
}

/// Lists regular archiveable files without reading their bodies.
///
/// Paths are relative to `novel_dir` and always use `/`. Files that the archive
/// format cannot represent (Windows-reserved names, control characters, …) are
/// skipped rather than failing the export of the whole library.
pub fn collect_novel_files(novel_dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    collect_files_into(novel_dir, novel_dir, &mut |relative, path| {
        files.push((relative.to_owned(), path.to_path_buf()));
        Ok(())
    })?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

fn collect_files_into(
    base: &Path, current: &Path,
    collect: &mut dyn FnMut(&str, &Path) -> Result<()>,
) -> Result<()> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(current)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(base) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if is_excluded_top_level(&relative) {
            continue;
        }

        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            // Never follow links out of the novel directory.
            continue;
        }
        if file_type.is_dir() {
            collect_files_into(base, &path, collect)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        if is_excluded_file_name(&name) {
            continue;
        }
        if !is_archivable_relative_path(&relative) {
            continue;
        }

        collect(&relative, &path)?;
    }
    Ok(())
}

fn is_excluded_top_level(relative: &str) -> bool {
    relative
        .split('/')
        .next()
        .is_some_and(|first| EXCLUDED_TOP_LEVEL_DIRS.contains(&first))
}

/// Runtime artifacts that must not travel: `*.lock` files from the inventory
/// writers and `*.tmp` partial writes (including our own `.<name>.<rand>.tmp`).
fn is_excluded_file_name(name: &str) -> bool {
    name.ends_with(".lock") || name.ends_with(".tmp")
}

/// Whether `path` (relative, `/` separated) can be stored in an archive.
///
/// This mirrors the validation in [`crate::transfer`] (which is not reachable
/// from here) so a single odd filename in a library does not abort an export.
pub fn is_archivable_relative_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 4096 || path.starts_with('/') {
        return false;
    }
    if path.contains('\\') || path.contains('\0') {
        return false;
    }
    for (index, component) in path.split('/').enumerate() {
        if component.is_empty() || component == "." || component == ".." {
            return false;
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return false;
        }
        if index == 0 {
            let bytes = component.as_bytes();
            if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                return false;
            }
        }
        for character in component.chars() {
            if character.is_control() {
                return false;
            }
            if matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                return false;
            }
        }
        let stem = component
            .split('.')
            .next()
            .unwrap_or(component)
            .trim_end_matches([' ', '.'])
            .to_ascii_uppercase();
        if is_reserved_device_name(&stem) {
            return false;
        }
    }
    true
}

fn is_reserved_device_name(stem: &str) -> bool {
    matches!(
        stem,
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) || (stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit())
}

/// Builds the manifest metadata for one record.
///
/// `files` are the collected novel files; `subtitle` is the story/catchphrase
/// read from `toc.yaml` (the archive format's `subtitle` slot).
pub fn archive_novel_from_record(
    record: &NovelRecord,
    files: Vec<ArchiveFile>,
    subtitle: Option<String>,
) -> ArchiveNovel {
    ArchiveNovel {
        toc_url: record.toc_url.clone(),
        title: record.title.clone(),
        author: record.author.clone(),
        sitename: record.sitename.clone(),
        domain: record.domain.clone(),
        tags: record.tags.clone(),
        attribute_tag: record.attribute_tag().map(str::to_string),
        novel_type: record.novel_type,
        files,
        ncode: record.ncode.clone(),
        subtitle,
        general_lastup: record.general_lastup.map(rfc3339),
        last_update: Some(rfc3339(record.last_update)),
        virtual_members: None,
    }
}

/// Builds the manifest metadata for one virtual parent.
///
/// The parent's own identity is the portable URL. The membership travels as
/// the members' `toc_url` in registration order, never as library IDs, and
/// the library-local ncode and internal tag stay behind.
fn archive_virtual_from_record(
    record: &NovelRecord,
    toc_url: &str,
    members: Vec<String>,
) -> ArchiveNovel {
    ArchiveNovel {
        toc_url: toc_url.to_string(),
        title: record.title.clone(),
        author: record.author.clone(),
        sitename: record.sitename.clone(),
        domain: record.domain.clone(),
        tags: record.tags.iter()
            .filter(|tag| !crate::virtual_novel::is_internal_tag(tag))
            .cloned()
            .collect(),
        attribute_tag: record.attribute_tag().map(str::to_string),
        novel_type: record.novel_type,
        files: Vec::new(),
        // A parent has no ncode of its own: the ncode is library-local.
        ncode: None,
        subtitle: None,
        general_lastup: record.general_lastup.map(rfc3339),
        last_update: Some(rfc3339(record.last_update)),
        virtual_members: Some(members),
    }
}

fn rfc3339(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse_rfc3339(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

/// Creates a brand new library entry from an archive novel.
///
/// The novel directory is written first; the record is published only after
/// every file is durable. A failure removes the files this run wrote, so an
/// interrupted import never leaves a half-novel behind.
fn create_novel(
    repository:&NativeNovelRepository,
    archive_root: &Path,
    metadata: &ArchiveNovel,
    novel: &StagedNovel,
    assets: &dyn AssetStore,
) -> Result<i64> {
    // Authoritative duplicate check: the plan ran against a snapshot, and the
    // index `find_by_toc_url` uses can lag behind `database.yaml`.
    let duplicate = db::with_database(|database| {
        Ok(database
            .all_records()
            .values()
            .find(|record| record.toc_url == novel.toc_url)
            .map(|record| record.id))
    })?;
    if let Some(existing) = duplicate {
        return Err(NarouError::Platform(format!(
            "既に ID:{existing} として登録されています"
        )));
    }
    if !novel.files.iter().any(|(path, _)| path == TOC_FILE) {
        return Err(NarouError::Platform(format!(
            "アーカイブに {TOC_FILE} がありません"
        )));
    }

    let toc = parse_archive_toc(assets, &novel.files)?;
    let title = toc
        .as_ref()
        .and_then(|toc|toc.title.clone())
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| metadata.title.clone());
    if title.trim().is_empty() {
        return Err(NarouError::Platform(
            "アーカイブに作品名がありません".to_string(),
        ));
    }
    let author = toc
        .as_ref()
        .and_then(|toc|toc.author.clone())
        .filter(|author| !author.trim().is_empty())
        .unwrap_or_else(|| metadata.author.clone());

    let sitename = metadata.sitename.trim();
    if sitename.is_empty() {
        return Err(NarouError::Platform(
            "アーカイブにサイト名がありません".to_string(),
        ));
    }

    let setting = find_site_setting(&novel.toc_url);
    let domain = metadata
        .domain
        .clone()
        .filter(|domain| !domain.trim().is_empty())
        .or_else(|| setting.as_ref().map(|setting| setting.domain.clone()))
        .or_else(|| host_of(&novel.toc_url));
    let ncode = metadata
        .ncode
        .clone()
        .filter(|ncode| !ncode.trim().is_empty())
        .or_else(|| extract_ncode(&novel.toc_url));
    let append_title = setting
        .as_ref()
        .is_some_and(|setting| setting.append_title_to_folder_name);
    let file_title = derive_file_title(ncode.as_deref(), &title, append_title);
    let use_subdirectory = NativeDownloaderSettings.download_use_subdirectory();
    let novel_type = if metadata.novel_type != 0 {
        metadata.novel_type
    } else {
        toc.as_ref().and_then(|toc| toc.novel_type).unwrap_or(1)
    };
    let general_all_no = toc
        .as_ref()
        .map(|toc|toc.chapter_count as i64)
        .filter(|count| *count > 0);

    let now = Utc::now();
    let mut record = NovelRecord {
        id: 0,
        author,
        title,
        file_title,
        toc_url: novel.toc_url.clone(),
        sitename: sitename.to_string(),
        novel_type,
        end: false,
        last_update: now,
        new_arrivals_date: None,
        use_subdirectory,
        general_firstup: None,
        novelupdated_at: None,
        general_lastup: parse_rfc3339(metadata.general_lastup.as_deref()),
        last_mail_date: None,
        tags: metadata.tags.clone(),
        ncode,
        domain,
        general_all_no,
        length: None,
        suspend: false,
        is_narou: setting.as_ref().is_some_and(|setting| setting.is_narou),
        last_check_date: None,
        convert_failure: false,
        requires_login: false,
        login_session: None,
        extra_fields: Default::default(),
    };
    // The classification is metadata, not a tag: it travels in its own field.
    // An archive without one leaves the record to the repository's own
    // initialization.
    if let Some(attribute) = metadata.attribute_tag.as_deref() {
        record.set_attribute_tag(attribute);
    }

    let novel_dir = db::novel_dir_for_record(archive_root, &record);
    if novel_dir.exists() {
        return Err(NarouError::Platform(format!(
            "保存先 {} が既に存在します。管理外のフォルダは上書きしません",
            novel_dir.display()
        )));
    }

    let reservation = db::with_database_mut(|database| database.allocate_id())?;
    record.id = reservation.id().0;
    let id = record.id;
    let result=(||{
        let write=capture_record_writer(&record)?;
        block_on(repository.reserve_create(write.fence(),&reservation,&record.toc_url))?;
        // The durable reservation owns partial-create cleanup, including its
        // original empty-slot proof. A path alone never proves ownership.
        write_novel_files(&write,&novel_dir,&novel.files,false)?;
        // Publication can fail after an irrevocable metadata commit. Exact
        // reservation cleanup, not blind file deletion, resolves that case.
        block_on(repository.apply_create_guarded(write.fence(),&reservation,record))
    })();
    if let Err(error)=result{
        return Err(import_failure_after_release(repository,reservation,error));
    }

    Ok(id)
}

fn import_failure_after_release(repository:&NativeNovelRepository,reservation:crate::platform::NovelIdReservation,error:NarouError)->NarouError{
    match block_on(repository.release_id(reservation)){
        Ok(())=>error,
        // The release commits its closed-removal ownership before it returns;
        // only the reader drain and the orphan sweep stay deferred. A failure
        // here is a real cleanup failure, and the import's own error stays
        // the reported cause.
        Err(cleanup)=>NarouError::Platform(format!("{error}; reservation cleanup failed: {cleanup}")),
    }
}

/// Replaces the files of an existing novel, keeping its record id and folder.
fn overwrite_novel(
    write:&NovelWriteGuard,
    repository: &NativeNovelRepository,
    archive_root: &Path,
    record: &NovelRecord,
    metadata: &ArchiveNovel,
    novel: &StagedNovel,
    assets: &dyn AssetStore,
) -> Result<()> {
    let novel_dir = db::existing_novel_dir_for_record(archive_root, record);
    if !novel_dir.exists() {
        // A deleted mirror folder is not an overwrite target: writing it would
        // resurrect one copy while the store still holds the old one.
        return Err(NarouError::Platform(format!(
            "保存先 {} が存在しません。上書きは既存のフォルダにだけ行います",
            novel_dir.display()
        )));
    }

    let toc = parse_archive_toc(assets, &novel.files)?;
    write_novel_files(write,&novel_dir,&novel.files,true)?;

    // Only fill fields the live record does not have. Changing title/author of
    // a managed novel is the downloader's job, and changing sitename/file_title
    // would move the folder out from under the files just written.
    let toc_title = toc
        .as_ref()
        .and_then(|toc|toc.title.clone())
        .filter(|title| !title.trim().is_empty());
    let toc_author = toc
        .as_ref()
        .and_then(|toc|toc.author.clone())
        .filter(|author| !author.trim().is_empty());
    let mut updated = record.clone();
    let mut changed = false;
    if updated.author.trim().is_empty()
        && let Some(author) = toc_author
            .or_else(|| (!metadata.author.trim().is_empty()).then(|| metadata.author.clone()))
    {
        updated.author = author;
        changed = true;
    }
    if updated.title.trim().is_empty()
        && let Some(title) = toc_title
            .or_else(|| (!metadata.title.trim().is_empty()).then(|| metadata.title.clone()))
    {
        updated.title = title;
        changed = true;
    }
    if updated.ncode.is_none()
        && let Some(ncode) = metadata
            .ncode
            .clone()
            .filter(|ncode| !ncode.trim().is_empty())
    {
        updated.ncode = Some(ncode);
        changed = true;
    }
    if updated.domain.is_none()
        && let Some(domain) = metadata.domain.clone()
    {
        updated.domain = Some(domain);
        changed = true;
    }
    if updated.general_all_no.is_none()
        && let Some(count) = toc
            .as_ref()
            .map(|toc|toc.chapter_count as i64)
            .filter(|count| *count > 0)
    {
        updated.general_all_no = Some(count);
        changed = true;
    }
    if updated.general_lastup.is_none()
        && let Some(lastup) = parse_rfc3339(metadata.general_lastup.as_deref())
    {
        updated.general_lastup = Some(lastup);
        changed = true;
    }
    // The classification is metadata the archive carries explicitly, so an
    // overwrite restores it. An archive without one leaves the live
    // classification untouched.
    if let Some(attribute) = metadata.attribute_tag.as_deref()
        && updated.attribute_tag() != Some(attribute)
    {
        updated.set_attribute_tag(attribute);
        changed = true;
    }

    if changed {
        block_on(repository.apply_batch_guarded(write.fence(),vec![NovelMutation::Upsert(updated)]))?;
    }
    Ok(())
}

/// Creates a new virtual parent from one archive entry.
///
/// The parent owns no body: only its per-novel inputs are stored. The files
/// become durable first, then the record and the membership are published in
/// one database update, so a failed member never leaves a visible group.
fn create_virtual_parent(
    repository:&NativeNovelRepository,
    archive_root: &Path,
    metadata: &ArchiveNovel,
    novel: &StagedNovel,
    member_ids: &[i64],
) -> Result<i64> {
    if !crate::virtual_novel::is_portable_toc_url(&novel.toc_url) {
        return Err(NarouError::Platform(
            "バーチャル小説の保存先 URL が移植できません".into(),
        ));
    }
    // Authoritative duplicate check: the plan ran against a snapshot.
    let duplicate = db::with_database(|database| {
        Ok(database
            .all_records()
            .values()
            .find(|record| record.toc_url == novel.toc_url)
            .map(|record| record.id))
    })?;
    if let Some(existing) = duplicate {
        return Err(NarouError::Platform(format!(
            "既に ID:{existing} として登録されています"
        )));
    }
    let title = metadata.title.trim().to_string();
    if title.is_empty() {
        return Err(NarouError::Platform("アーカイブに作品名がありません".into()));
    }
    // The record needs a non-empty sitename because it names the key prefix.
    let sitename = metadata.sitename.trim().to_string();
    let sitename = if sitename.is_empty() {
        host_of(&novel.toc_url).unwrap_or_else(|| "narou.rs".to_string())
    } else {
        sitename
    };
    let use_subdirectory = NativeDownloaderSettings.download_use_subdirectory();
    let reservation = db::with_database_mut(|database| database.allocate_id())?;
    let id = reservation.id().0;
    let ncode = crate::virtual_novel::ncode(id);
    // The folder name is derived from the new local ID, so a group with the
    // same title never shares a folder with an existing one.
    let file_title = ncode.clone();
    let mut record = NovelRecord {
        id,
        author: metadata.author.clone(),
        title,
        file_title,
        // The archive's identity is kept; the local ncode is new.
        toc_url: novel.toc_url.clone(),
        sitename,
        novel_type: metadata.novel_type,
        end: true,
        last_update: Utc::now(),
        new_arrivals_date: None,
        use_subdirectory,
        general_firstup: None,
        novelupdated_at: None,
        general_lastup: None,
        last_mail_date: None,
        tags: metadata
            .tags
            .iter()
            .filter(|tag| !crate::virtual_novel::is_internal_tag(tag))
            .cloned()
            .collect(),
        ncode: Some(ncode),
        domain: metadata.domain.clone(),
        general_all_no: None,
        length: None,
        suspend: false,
        is_narou: false,
        last_check_date: None,
        convert_failure: false,
        requires_login: false,
        login_session: None,
        extra_fields: Default::default(),
    };
    // The classification travels as metadata; a mixed group keeps the value
    // the archive carries.
    if let Some(attribute) = metadata.attribute_tag.as_deref() {
        record.set_attribute_tag(attribute);
    }

    let novel_dir = db::novel_dir_for_record(archive_root, &record);
    if novel_dir.exists() {
        let error=NarouError::Platform(format!("保存先 {} が既に存在します。管理外のフォルダは上書きしません",novel_dir.display()));
        return Err(import_failure_after_release(repository,reservation,error));
    }

    let result=(||{
        let write=capture_record_writer(&record)?;
        block_on(repository.reserve_create(write.fence(),&reservation,&record.toc_url))?;
        write_novel_files(&write,&novel_dir,&novel.files,false)?;
        // The record and membership use the same original create generation.
        crate::native::virtual_novel::publish_imported_parent(
            &write,&reservation,record,member_ids.to_vec(),parse_rfc3339(metadata.general_lastup.as_deref()),
        )
    })();
    if let Err(error)=result{
        return Err(import_failure_after_release(repository,reservation,error));
    }
    Ok(id)
}

/// Replaces an existing virtual parent with the archive's structure.
///
/// The record ID and the folder stay; the membership order, the supplied
/// per-novel inputs and the derived metadata are replaced. Files that the
/// archive does not carry are left alone.
fn overwrite_virtual_parent(
    write:&NovelWriteGuard,
    archive_root: &Path,
    record: &NovelRecord,
    metadata: &ArchiveNovel,
    novel: &StagedNovel,
    member_ids: &[i64],
) -> Result<()> {
    if !crate::virtual_novel::is_virtual_record(record) {
        return Err(NarouError::Platform(
            "ライブラリにある作品はバーチャル小説ではなくなっています".into(),
        ));
    }
    let novel_dir = db::existing_novel_dir_for_record(archive_root, record);
    // Only the archived inputs are replaced; other files stay untouched.
    write_novel_files(write,&novel_dir,&novel.files,true)?;

    // The live row is read again inside the write boundary, so a field that
    // changed meanwhile is kept and only the archived metadata is applied.
    crate::native::virtual_novel::replace_imported_parent(
        write,
        record.id,
        &crate::native::virtual_novel::ImportedParentMetadata {
            author: metadata.author.clone(),
            title: metadata.title.clone(),
            domain: metadata.domain.clone(),
            tags: metadata.tags.clone(),
            attribute_tag: metadata.attribute_tag.clone(),
            general_lastup: parse_rfc3339(metadata.general_lastup.as_deref()),
        },
        member_ids.to_vec(),
    )
}

fn parse_archive_toc(assets:&dyn AssetStore,files:&[(String,StagedAsset)])->Result<Option<TocMetadata>>{
    let Some((_, asset)) = files.iter().find(|(path, _)| path == TOC_FILE) else { return Ok(None); };
    block_on(async {
        use futures::StreamExt;
        let mut stream = assets.read_stream(&asset.key).await?
            .ok_or_else(|| NarouError::NotFound(asset.key.to_string()))?;
        if asset.size>16*1024*1024{return Err(NarouError::Platform("TOC metadata exceeds16MiB".into()));}let mut bytes=Vec::with_capacity(asset.size as usize);
        while let Some(chunk)=stream.next().await{let chunk=chunk?;if bytes.len()+chunk.len()>16*1024*1024{return Err(NarouError::Platform("TOC decoder exceeds16MiB".into()));}bytes.extend_from_slice(&chunk);}
        let toc=crate::downloader::persistence::deserialize_toc_metadata(&bytes)?;if toc.chapter_count>i64::MAX as u64{return Err(NarouError::Platform("TOC chapter count exceeds native record capacity".into()));}Ok(Some(toc))
    })
}

fn find_site_setting(url: &str) -> Option<site_setting::SiteSetting> {
    site_setting::effective_site_settings()
        .into_iter()
        .find(|setting| setting.matches_url(url))
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

/// Extracts an `n1234ab`-style work id from a table-of-contents URL.
fn extract_ncode(url: &str) -> Option<String> {
    let pattern = regex::Regex::new(r"(?i)[/?](n\d+[a-z]+)").ok()?;
    pattern
        .captures(url)
        .and_then(|captures| captures.get(1))
        .map(|matched| matched.as_str().to_lowercase())
}

/// Same rule as the downloader's `compute_file_title` for a new novel.
fn derive_file_title(ncode: Option<&str>, title: &str, append_title: bool) -> String {
    let limit = load_length_limit("folder-length-limit", Some(50));
    if let Some(ncode) = ncode.filter(|ncode| !ncode.is_empty()) {
        if !append_title {
            return ncode.to_string();
        }
        let combined = format!("{ncode} {title}");
        let sanitized = sanitize_filename_with_limit(&combined, limit);
        return if sanitized.is_empty() {
            ncode.to_string()
        } else {
            sanitized
        };
    }
    sanitize_filename_with_limit(title, limit)
}

/// Writes archive files below `novel_dir` through the native object store.
///
/// Going through the store instead of writing the filesystem directly keeps
/// every backend consistent:
///
/// * YAML mode uses the filesystem store, which already writes atomically.
/// * SQLite mode updates the `objects` rows *and* the mirror. Writing only the
///   mirror would not be enough there: a stored row wins over the mirror, so an
///   overwrite would stay invisible to the rest of the application.
/// * An S3 illustration backend routes `挿絵/…` keys to S3 via `SplitStore`.
///
/// `parse_archive` already rejected unsafe paths; they are re-validated here
/// because this routine is also reachable with in-memory data from other front
/// ends and because the destination is a real filesystem.
fn write_novel_files(
    write:&NovelWriteGuard,
    novel_dir: &Path,
    files: &[(String, StagedAsset)],
    overwrite: bool,
) -> Result<()> {
    for (relative, asset) in files {
        let target = safe_join(novel_dir, relative)?;
        if !overwrite && target.exists() {
            return Err(NarouError::Platform(format!(
                "{} は既に存在します",
                target.display()
            )));
        }
        let key=ObjectKey::try_new(format!("{}/{relative}",write.prefix().as_ref().trim_end_matches('/')))?;
        block_on(write.install_staged(&asset.key,&key))?;
    }
    Ok(())
}

fn safe_join(base: &Path, relative: &str) -> Result<PathBuf> {
    if !is_archivable_relative_path(relative) {
        return Err(NarouError::Platform(format!(
            "安全でないファイルパスです: {relative:?}"
        )));
    }
    Ok(base.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))
}



struct StageCleanup { assets: Arc<dyn AssetStore>, keys: Vec<ObjectKey> }
impl Drop for StageCleanup {
    fn drop(&mut self) {
        let assets = self.assets.clone();
        let keys = std::mem::take(&mut self.keys);
        let _ = block_on(async move {
            for key in keys { let _ = assets.delete(&key).await; }
            Ok(())
        });
    }
}

fn capture_record_writer(record:&NovelRecord)->Result<NovelWriteGuard>{
    let stores=crate::native::object_store::NativeStores::for_current_root()?;
    let keys=NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?;
    block_on(NovelWriteGuard::capture_for(&stores.objects,Some(&stores.assets),&keys))
}

fn capture_import_writers(stores:&crate::native::object_store::NativeStores)->Result<HashMap<String,NovelWriteGuard>>{
    let targets=db::with_database(|database|database.all_records().values().map(|record|Ok((record.toc_url.clone(),NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory)?))).collect::<Result<Vec<_>>>())?;
    let mut guards=HashMap::with_capacity(targets.len());
    for(url,keys)in targets{match block_on(NovelWriteGuard::capture_for(&stores.objects,Some(&stores.assets),&keys)){Ok(guard)=>{guards.entry(url).or_insert(guard);},Err(NarouError::Platform(message))if message.starts_with("novel_recovering:")=>{},Err(error)=>return Err(error)}}
    Ok(guards)
}

#[cfg(test)]
mod transfer_archive_tests {
    use super::*;

    fn record() -> NovelRecord {
        use chrono::TimeZone;
        NovelRecord {
            id: 7,
            author: "作者".to_string(),
            title: "作品名".to_string(),
            file_title: "n1234ab 作品名".to_string(),
            toc_url: "https://example.test/n1234ab/".to_string(),
            sitename: "ExampleSite".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 4, 20, 0, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: vec!["ファンタジー".to_string()],
            ncode: Some("n1234ab".to_string()),
            domain: Some("example.test".to_string()),
            general_all_no: Some(3),
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn transfer_archive_manifest_metadata_comes_from_the_record() {
        let metadata = archive_novel_from_record(
            &record(),
            vec![ArchiveFile::new("toc.yaml", b"title".to_vec())],
            Some("あらすじ".to_string()),
        );
        assert_eq!(metadata.toc_url, "https://example.test/n1234ab/");
        assert_eq!(metadata.title, "作品名");
        assert_eq!(metadata.author, "作者");
        assert_eq!(metadata.sitename, "ExampleSite");
        assert_eq!(metadata.domain.as_deref(), Some("example.test"));
        assert_eq!(metadata.tags, vec!["ファンタジー".to_string()]);
        assert_eq!(metadata.novel_type, 1);
        assert_eq!(metadata.ncode.as_deref(), Some("n1234ab"));
        assert_eq!(metadata.subtitle.as_deref(), Some("あらすじ"));
        assert_eq!(
            metadata.last_update.as_deref(),
            Some("2026-04-20T00:00:00Z")
        );
        assert_eq!(metadata.files.len(), 1);
    }

    #[test]
    fn transfer_archive_collection_skips_backup_locks_and_temp_files() {
        let temp = tempfile::tempdir().unwrap();
        let novel_dir = temp.path().join("novel");
        fs::create_dir_all(novel_dir.join("本文")).unwrap();
        fs::create_dir_all(novel_dir.join("backup")).unwrap();
        fs::create_dir_all(novel_dir.join("本文").join("nested")).unwrap();
        fs::write(novel_dir.join("toc.yaml"), b"toc").unwrap();
        fs::write(novel_dir.join("本文").join("1 第一話.yaml"), b"body").unwrap();
        fs::write(
            novel_dir.join("本文").join("nested").join("2 第二話.yaml"),
            b"body2",
        )
        .unwrap();
        fs::write(novel_dir.join("setting.ini"), b"[global]").unwrap();
        fs::write(novel_dir.join("database.yaml.lock"), b"lock").unwrap();
        fs::write(novel_dir.join(".toc.yaml.1234.abcd.tmp"), b"tmp").unwrap();
        fs::write(novel_dir.join("backup").join("old.zip"), b"zip").unwrap();

        let files = collect_novel_files(&novel_dir).unwrap();
        let paths: Vec<&str> = files.iter().map(|file| file.0.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "setting.ini",
                "toc.yaml",
                "本文/1 第一話.yaml",
                "本文/nested/2 第二話.yaml",
            ]
        );
        assert!(
            paths.iter().all(|path| !path.starts_with("backup/")),
            "{paths:?}"
        );
        assert_eq!(
            files
                .iter()
                .find(|file| file.0 == "toc.yaml")
                .map(|file| fs::read(&file.1).unwrap()),
            Some(b"toc".to_vec())
        );
    }

    #[test]
    fn transfer_archive_path_validation_matches_the_archive_rules() {
        for path in [
            "toc.yaml",
            "本文/1 第一話.yaml",
            "挿絵/abc.png",
            "sub dir/file.txt",
        ] {
            assert!(is_archivable_relative_path(path), "{path}");
        }
        for path in [
            "",
            "/absolute",
            "../escape",
            "dir/../../escape",
            "a\\b",
            "C:/windows",
            "dir//empty",
            "CON",
            "con.txt",
            "COM1.log",
            "trailing.",
            "trailing ",
            "bad:name",
            "bad?name",
            "bad\u{1}name",
        ] {
            assert!(!is_archivable_relative_path(path), "{path}");
        }
    }

    #[test]
    fn transfer_archive_file_title_follows_the_downloader_rule() {
        assert_eq!(derive_file_title(Some("n1234ab"), "作品", false), "n1234ab");
        assert_eq!(
            derive_file_title(Some("n1234ab"), "作品", true),
            "n1234ab 作品"
        );
        assert_eq!(derive_file_title(None, "作品名", false), "作品名");
        assert_eq!(derive_file_title(None, "a/b:c", false), "a_b_c");
    }

    #[test]
    fn transfer_archive_ncode_is_derived_from_the_toc_url() {
        assert_eq!(
            extract_ncode("https://ncode.syosetu.com/n1234ab/").as_deref(),
            Some("n1234ab")
        );
        assert_eq!(
            extract_ncode("https://novel18.syosetu.com/N9999zz/").as_deref(),
            Some("n9999zz")
        );
        assert_eq!(extract_ncode("https://example.test/novel/1/"), None);
    }
}


/// Commit marker installed with record, tag and membership metadata.
#[derive(Debug, Clone)]
pub struct TransferCommitMarker {
    pub session_id: String,
    pub publication_id: u32,
    pub novel_id: crate::platform::NovelId,
    pub receipt: Vec<u8>,
    pub yaml_commit_path: PathBuf,
}

pub mod v2 {
    use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc};
    use chrono::Utc;
    use serde::{Serialize, Deserialize};
    use rusqlite::OptionalExtension;
    use crate::application::transfer::*;
    use crate::platform::{PlatformFuture, AssetStore, ObjectStore, TransferStageStore, NovelAccessFence, ObjectKey, ObjectPrefix, NovelObjectKeys, NovelId};
    use crate::native::object_store::{NativeStore, NativeStores};
    type TR<T> = std::result::Result<T, TransferError>;

    pub use super::journal::NativeTransferJournal;

    fn scope_key(scope: &AuthScope) -> String { sha256_hex(format!("{}\0{}",scope.library_id,scope.principal).as_bytes()) }
    fn number(value:u32)->String { format!("{value:010}") }

    pub fn native_scope()->TR<AuthScope>{let root=crate::db::inventory::find_narou_root().map_err(TransferError::platform)?;Ok(AuthScope{library_id:sha256_hex(root.to_string_lossy().as_bytes()),principal:"admin".into()})}

    pub struct NativeTransferBackend { root:PathBuf, journal:Arc<NativeTransferJournal>, primary:NativeStore, filesystem:NativeStore, stores:NativeStores, assets:Arc<dyn AssetStore>, objects:Arc<dyn ObjectStore> }

    #[derive(Debug,Clone,Default,Serialize,Deserialize)]
    struct FrozenSelection { ordinary:Vec<i64>,parents:Vec<i64>,memberships:BTreeMap<i64,Vec<i64>>,seen:std::collections::BTreeSet<i64>,requested:usize,member:usize,pending_parent:Option<i64>,complete:bool,last_operation:Option<OperationId> }
    #[derive(Debug,Clone,Serialize,Deserialize)]
    struct SnapshotSource { entry:ExportNovelEntry, prefix:String, layout:crate::transfer::NovelJsonLayout }
    #[derive(Debug,Clone,Default,Serialize,Deserialize)]
    struct InventoryPosition { novel:usize,phase:u8,after:Option<String>,#[serde(default)]metadata_emitted:bool }
    #[derive(Debug,Clone,Serialize,Deserialize)]
    struct HashScan { identity:String,offset:u64,state:Option<Vec<u8>>,digest:Option<String> }
    #[derive(Debug,Clone,Serialize,Deserialize)]
    struct StageBinding { owner:String, file_id:u32, size:u64, epoch:u64 }
    #[derive(Debug,Clone,Serialize,Deserialize)]
    struct NativePublication { publication_id:u32,plan_fingerprint:String,attempt:String,record_id:i64,file_title:String,sitename:String,use_subdirectory:bool,is_narou:bool,prefix:String,owner:String,epoch:u64,content_revision:u64,prepared:u32,installed:u32,committed:bool }
    #[derive(Debug,Clone,Serialize,Deserialize)]
    struct FileUndo { path:String, old_stage:Option<String>, old_size:u64, old_sha256:Option<String>, old_generation:Option<String>, copied:u64, ready:bool, installed:bool, install_stage:Option<String>, install_offset:u64,restore_stage:Option<String>,restore_offset:u64 }

    include!("transfer/snapshot.rs");
    include!("transfer/publication.rs");
    include!("transfer/metadata.rs");

    impl NativeTransferBackend {
        pub fn open(root:PathBuf,journal:Arc<NativeTransferJournal>)->TR<Self>{let stores=NativeStores::for_narou_root(&root).map_err(TransferError::platform)?;let primary=stores.primary.as_ref().clone();let filesystem=NativeStore::Fs(crate::native::object_store::NativeObjectStore::from_root(root.join("小説データ")).map_err(TransferError::platform)?);let assets=stores.assets.clone();let objects=stores.objects.clone();Ok(Self{root,journal,primary,filesystem,stores,assets,objects})}
        fn archive_root(&self)->PathBuf{self.root.join("小説データ")}
        fn source_owner(session:&SessionId,id:i64)->String{format!("source:{}:{id}",session.0)}
        async fn publication_file(&self,spec:&PublicationSpec,index:u32)->TR<PublicationFile>{
            if index>=spec.file_count{return Err(TransferError::invalid("invalid_file","publication file index out of range"));}
            let id=spec.file_id_start.checked_add(index).ok_or_else(||TransferError::invalid("invalid_file","publication file ordinal overflow"))?;
            let file=self.journal.file(&spec.session_id,id).await?.ok_or_else(||TransferError::invalid("file_missing","publication file row missing"))?;
            if file.novel_ordinal!=spec.publication_id||!matches!(file.state,FileState::Verified|FileState::Reused){return Err(TransferError::invalid("file_unverified","publication file is not verified for this novel"));}
            Ok(PublicationFile{logical_path:file.logical_path,size:file.size,sha256:file.sha256,stage:file.stage,pin:file.pin})
        }
        fn frozen_selection(&self,scope:&AuthScope,session:&SessionId,requested:&[i64])->TR<Option<FrozenSelection>>{
            let session=&session.0;let status=self.journal.get::<SessionRow>("session",&scope_key(scope),session)?.ok_or(TransferError::SessionNotFound)?;let operation=status.operation_id.ok_or(TransferError::OperationConflict)?;
            let mut frozen:FrozenSelection=self.journal.backend_get(session,"export_selection")?.unwrap_or_default();
            if frozen.complete&&frozen.last_operation.as_ref()!=Some(&operation){return Ok(Some(frozen));}
            if frozen.last_operation.as_ref()==Some(&operation){return Ok(None);}
            let memberships=crate::native::virtual_novel::load().map_err(TransferError::platform)?;
            crate::db::with_database_mut(|db|{db.refresh()?;let mut processed=0;while processed<MAX_PAGE_ITEMS{
                if let Some(parent)=frozen.pending_parent{let members=&frozen.memberships[&parent];if frozen.member<members.len(){let id=members[frozen.member];let row=db.get(id).ok_or_else(||crate::error::NarouError::NotFound(format!("member ID:{id}")))?;if crate::virtual_novel::is_virtual_record(row){return Err(crate::error::NarouError::Platform("virtual parent contains a virtual member".into()));}
                    if frozen.seen.insert(id){frozen.ordinary.push(id);}frozen.member+=1;processed+=1;continue;}frozen.pending_parent=None;frozen.member=0;}
                let Some(id)=requested.get(frozen.requested).copied()else{frozen.complete=true;break;};let record=db.get(id).ok_or_else(||crate::error::NarouError::NotFound(format!("ID:{id}")))?;frozen.requested+=1;processed+=1;
                if crate::virtual_novel::is_virtual_record(record){if !frozen.memberships.contains_key(&id){let members=memberships.get(id).ok_or_else(||crate::error::NarouError::Platform("virtual parent has no members".into()))?.to_vec();frozen.parents.push(id);frozen.memberships.insert(id,members);frozen.pending_parent=Some(id);}}else if frozen.seen.insert(id){frozen.ordinary.push(id);}
            }Ok(())}).map_err(TransferError::platform)?;
            if frozen.ordinary.len()+frozen.parents.len()>10_000{return Err(TransferError::invalid("novel_limit","expanded export exceeds10000 novels"));}frozen.last_operation=Some(operation);self.journal.backend_put(session,"export_selection",&frozen)?;Ok(None)
        }
        fn source_record(&self,entry:&ExportNovelEntry)->TR<crate::db::NovelRecord>{crate::db::with_database_mut(|db|{db.refresh()?;db.get(entry.id.0).cloned().ok_or_else(||crate::error::NarouError::NotFound("export source was deleted".into()))}).map_err(TransferError::platform)}
        fn record_prefix(&self,record:&crate::db::NovelRecord)->TR<ObjectKey>{
            Ok(NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory).map_err(TransferError::platform)?.prefix().clone())
        }
        fn publication(&self,spec:&PublicationSpec)->TR<NativePublication>{
            let mut publication:NativePublication=self.journal.backend_get(&spec.session_id.0,&format!("publication:{}",spec.publication_id))?.ok_or_else(||TransferError::invalid("publication_missing","native publication owner missing"))?;
            if publication.owner!=Self::owner(spec){return Err(TransferError::PlanStale{message:"publication belongs to a different confirmed plan".into()});}
            if publication.epoch==0{self.journal.require_authority()?;let current=crate::native::materialization::block_on(self.primary.gate_info(&ObjectPrefix::new(&publication.prefix).map_err(TransferError::platform)?)).map_err(TransferError::platform)?;if current.owner.as_deref()==Some(&publication.owner){publication.epoch=current.epoch;self.save_publication(spec,&publication)?;}}
            Ok(publication)
        }
        fn save_publication(&self,spec:&PublicationSpec,publication:&NativePublication)->TR<()>{self.journal.run_write(|rows|{rows.put("backend",&spec.session_id.0,&format!("publication:{}",spec.publication_id),publication)?;rows.put("backend",&format!("publication_owners:{}",spec.session_id.0),&publication.owner,publication)})}
        fn publication_directory(&self,spec:&PublicationSpec)->PathBuf{self.root.join(".narou/transfer/publications").join(&spec.session_id.0).join(number(spec.publication_id))}
        fn owner(spec:&PublicationSpec)->String{format!("{}:{}:{}",spec.session_id.0,spec.publication_id,spec.plan_revision)}
        fn stage_binding(&self,handle:&str)->TR<StageBinding>{self.journal.backend_get("stages",handle)?.ok_or_else(||TransferError::invalid("stage_missing","transfer stage owner is missing"))}
        fn work_input<T:Serialize+serde::de::DeserializeOwned>(&self,session:&SessionId,key:&str,value:T)->TR<T>{
            let operation=self.journal.executing_operation(session)?;let key=format!("work:{}:{key}",operation.0);
            self.journal.run_write(|rows|{if let Some(existing)=rows.get("backend",&session.0,&key)?{return Ok(existing);}rows.put("backend",&session.0,&key,&value)?;Ok(value)})
        }
        async fn read_bytes(&self,key:&ObjectKey,range:std::ops::Range<u64>)->TR<Vec<u8>>{
            use futures::StreamExt;let expected=range.end.checked_sub(range.start).filter(|v|*v<=ADVANCE_BYTE_BUDGET).ok_or_else(||TransferError::invalid("invalid_range","read exceeds transfer window"))?;
            let mut stream=self.assets.read_stream_range(key,range).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("source_missing","stored bytes are missing"))?;let mut bytes=Vec::with_capacity(expected as usize);
            while let Some(chunk)=stream.next().await{let chunk=chunk.map_err(TransferError::platform)?;if bytes.len()+chunk.len()>expected as usize{return Err(TransferError::invalid("source_long_read","store exceeded requested range"));}bytes.extend_from_slice(&chunk);}if bytes.len() as u64!=expected{return Err(TransferError::invalid("source_short_read","store returned incomplete range"));}Ok(bytes)
        }
        async fn current_hash(&self,session:&SessionId,key:&ObjectKey)->TR<Option<(u64,String)>>{
            use sha2::Digest;
            if let Some(proof)=self.stores.transfer_asset_fingerprint(key).await.map_err(TransferError::platform)?{return Ok(Some(proof));}
            let Some(metadata)=self.assets.stat(key).await.map_err(TransferError::platform)? else{return Ok(None)};
            let generation=self.canonical_identity(key).await?.ok_or_else(||TransferError::RecoveryRequired{message:"canonical object has no stable generation for fingerprinting".into()})?;
            let identity=sha256_hex(generation.as_bytes());
            let record_key=sha256_hex(key.as_ref().as_bytes());
            let mut scan:HashScan=self.journal.backend_get("fingerprints",&record_key)?.filter(|s:&HashScan|s.identity==identity).unwrap_or(HashScan{identity,offset:0,state:None,digest:None});
            scan=self.work_input(session,&format!("fingerprint:{record_key}"),scan)?;
            if let Some(digest)=scan.digest{return Ok(Some((metadata.size,digest)));}
            let initialized=self.legacy_source(session,&format!("fingerprint_source:{record_key}:{}",sha256_hex(generation.as_bytes())),key,metadata.size,"fingerprinting").await?;let end=scan.offset.saturating_add(CHUNK_BYTES).min(metadata.size);let bytes=if let Some(stage)=initialized{self.stage_read_range(&stage,scan.offset..end).await?}else{self.read_bytes(key,scan.offset..end).await?};
            if self.canonical_identity(key).await?.as_deref()!=Some(generation.as_str()){return Err(TransferError::RecoveryRequired{message:"canonical generation changed during bounded fingerprint scan".into()});}
            let mut hash=if let Some(state)=&scan.state{crate::platform::decode_sha_state(state).map_err(TransferError::platform)?}else{sha2::Sha256::new()};hash.update(&bytes);scan.offset=end;scan.state=Some(crate::platform::encode_sha_state(&hash));
            if end==metadata.size{scan.digest=Some(hex::encode(hash.finalize()));}
            self.journal.backend_put("fingerprints",&record_key,&scan)?;
            match scan.digest{Some(digest)=>Ok(Some((metadata.size,digest))),None=>Err(TransferError::Busy{code:"fingerprinting",message:"bounded canonical fingerprint checkpoint saved".into()})}
        }
        fn record_revision(record:&crate::db::NovelRecord)->TR<String>{
            use sha2::Digest;
            struct HashWriter<'a>(&'a mut sha2::Sha256);
            impl std::io::Write for HashWriter<'_>{fn write(&mut self,bytes:&[u8])->std::io::Result<usize>{self.0.update(bytes);Ok(bytes.len())}fn flush(&mut self)->std::io::Result<()>{Ok(())}}
            let mut hash=sha2::Sha256::new();serde_json::to_writer(HashWriter(&mut hash),record).map_err(TransferError::platform)?;Ok(hex::encode(hash.finalize()))
        }
        fn content_revision(&self,record:&crate::db::NovelRecord)->TR<String>{
            let prefix=self.record_prefix(record)?;let info=crate::native::materialization::block_on(self.primary.gate_info(&ObjectPrefix::from(prefix))).map_err(TransferError::platform)?;Ok(info.content_revision.to_string())
        }
        fn ensure_scope(&self,scope:&AuthScope)->TR<()>{if scope!=&native_scope()?{return Err(TransferError::SessionNotFound);}Ok(())}
        fn ticket_key(&self)->TR<Vec<u8>>{
            self.journal.run(|r|{if let Some(value)=r.get::<Vec<u8>>("secret","library","ticket")?{return Ok(value);}let value=uuid::Uuid::new_v4().as_bytes().iter().chain(uuid::Uuid::new_v4().as_bytes()).copied().collect::<Vec<_>>();r.put("secret","library","ticket",&value)?;Ok(value)})
        }
        fn sign_ticket(&self,bytes:&[u8])->TR<String>{
            use sha2::Digest;let mut key=self.ticket_key()?;key.resize(64,0);let mut inner=[0x36u8;64];let mut outer=[0x5cu8;64];for i in 0..64{inner[i]^=key[i];outer[i]^=key[i];}let mut hash=sha2::Sha256::new();hash.update(inner);hash.update(bytes);let digest=hash.finalize();let mut hash=sha2::Sha256::new();hash.update(outer);hash.update(digest);Ok(hex::encode(hash.finalize()))
        }
        fn make_record(&self,spec:&PublicationSpec,id:i64,existing:Option<crate::db::NovelRecord>)->TR<crate::db::NovelRecord>{
            let metadata=&spec.metadata;
            if let Some(mut record)=existing {
                if record.title.trim().is_empty(){record.title=metadata.title.clone();}
                if record.author.trim().is_empty(){record.author=metadata.author.clone();}
                if record.ncode.is_none(){record.ncode=metadata.ncode.clone();}
                if record.domain.is_none(){record.domain=metadata.domain.clone();}
                if record.general_lastup.is_none(){record.general_lastup=super::parse_rfc3339(metadata.general_lastup.as_deref());}
                for tag in &metadata.tags{if !crate::virtual_novel::is_internal_tag(tag)&&!record.tags.contains(tag){record.tags.push(tag.clone());}}if let Some(a)=metadata.attribute_tag.as_deref(){record.set_attribute_tag(a);}return Ok(record);
            }
            let parent=spec.kind==NovelKind::VirtualParent;let setting=super::find_site_setting(&spec.toc_url);let ncode=if parent{Some(crate::virtual_novel::ncode(id))}else{metadata.ncode.clone().or_else(||super::extract_ncode(&spec.toc_url))};
            let file_title=if parent{crate::virtual_novel::ncode(id)}else{super::derive_file_title(ncode.as_deref(),&metadata.title,setting.as_ref().is_some_and(|s|s.append_title_to_folder_name))};
            let mut record=crate::db::NovelRecord{id,author:metadata.author.clone(),title:metadata.title.clone(),file_title,toc_url:spec.toc_url.clone(),sitename:if metadata.sitename.trim().is_empty(){if parent{"narou.rs".into()}else{return Err(TransferError::invalid("invalid_site","ordinary novel requires site name"));}}else{metadata.sitename.clone()},novel_type:metadata.novel_type,end:parent,last_update:Utc::now(),new_arrivals_date:None,use_subdirectory:crate::downloader::settings::DownloaderSettings::download_use_subdirectory(&crate::downloader::settings::NativeDownloaderSettings),general_firstup:None,novelupdated_at:None,general_lastup:super::parse_rfc3339(metadata.general_lastup.as_deref()),last_mail_date:None,tags:metadata.tags.iter().filter(|t|!crate::virtual_novel::is_internal_tag(t)).cloned().collect(),ncode,domain:metadata.domain.clone().or_else(||super::host_of(&spec.toc_url)),general_all_no:None,length:None,suspend:false,is_narou:!parent&&setting.as_ref().is_some_and(|s|s.is_narou),last_check_date:None,convert_failure:false,requires_login:false,login_session:None,extra_fields:Default::default()};if let Some(a)=metadata.attribute_tag.as_deref(){record.set_attribute_tag(a);}Ok(record)
        }
        fn create_publication(&self,spec:&PublicationSpec)->TR<NativePublication>{
            self.journal.require_authority()?;let target=&spec.target;let plan_fingerprint=sha256_hex(&serde_json::to_vec(&(&spec.plan_revision,target.action,target.existing_id,&target.record_revision,&target.content_revision,&target.membership_digest)).map_err(TransferError::platform)?);
            if let Some(value)=self.journal.backend_get::<NativePublication>(&spec.session_id.0,&format!("publication:{}",spec.publication_id))?{
                if value.plan_fingerprint==plan_fingerprint{return Ok(value);}
                let info=crate::native::materialization::block_on(self.primary.gate_info(&ObjectPrefix::new(&value.prefix).map_err(TransferError::platform)?)).map_err(TransferError::platform)?;
                if value.committed||info.state!=crate::platform::GateState::Open{return Err(TransferError::RecoveryRequired{message:"previous publication attempt must finish before replanning".into()});}
            }
            let existing=crate::db::with_database_mut(|db|{db.refresh()?;let folded=crate::db::novel_codec::fold(&spec.toc_url);Ok(db.all_records().values().find(|r|crate::db::novel_codec::fold(&r.toc_url)==folded).cloned())}).map_err(TransferError::platform)?;
            if matches!(spec.target.action,PlanAction::Create)&&existing.is_some()||matches!(spec.target.action,PlanAction::Overwrite)&&existing.as_ref().map(|r|r.id)!=spec.target.existing_id{return Err(TransferError::PlanStale{message:"publication target was created or deleted since planning".into()});}
            if let Some(record)=&existing&&(Some(Self::record_revision(record)?)!=spec.target.record_revision||Some(self.content_revision(record)?)!=spec.target.content_revision){return Err(TransferError::PlanStale{message:"record or content changed since planning".into()});}
            let owner=Self::owner(spec);let id=if let Some(record)=&existing{record.id}else{crate::db::with_database_mut(|db|db.allocate_transfer_id(&owner).map(|reservation|reservation.id().0)).map_err(TransferError::platform)?};
            let record=self.make_record(spec,id,existing.clone())?;let prefix=self.record_prefix(&record)?.as_ref().to_string();
            if existing.is_none(){let directory=crate::db::novel_dir_for_record(&self.archive_root(),&record);if directory.exists(){return Err(TransferError::PlanStale{message:"destination folder already exists and is not owned by this import".into()});}}
            let content_revision=self.content_revision(&record)?.parse().map_err(TransferError::platform)?;let publication=NativePublication{publication_id:spec.publication_id,plan_fingerprint,attempt:uuid::Uuid::new_v4().simple().to_string(),record_id:record.id,file_title:record.file_title,sitename:record.sitename,use_subdirectory:record.use_subdirectory,is_narou:record.is_narou,prefix,owner,epoch:0,content_revision,prepared:0,installed:0,committed:false};
            let control=self.journal.control_connection();crate::db::with_database(|db|db.with_library_metadata_snapshot(|inventory,primary|{
                let save=|connection:&rusqlite::Connection|->crate::error::Result<()>{
                    let transaction=rusqlite::Transaction::new_unchecked(connection,rusqlite::TransactionBehavior::Immediate).map_err(crate::native::sqlite::sqlite_error)?;
                    let identity_connection=if db.metadata_is_yaml_in_snapshot(primary){None}else{primary.map(|_|&*transaction)};super::claims::assert_current_identity(inventory,identity_connection,&transaction,publication.record_id,&spec.toc_url,&publication.prefix,spec.target.action==PlanAction::Create)?;
                    super::claims::claim(inventory.root_dir(),&transaction,&super::claims::PublicationClaim{owner:&publication.owner,session:&spec.session_id.0,publication:spec.publication_id,revision:&spec.plan_revision},publication.record_id,&spec.toc_url,&publication.prefix)?;
                    let mut rows=super::journal::Rows(&transaction);rows.put("backend",&spec.session_id.0,&format!("publication:{}",spec.publication_id),&publication).and_then(|_|rows.put("backend",&format!("publication_owners:{}",spec.session_id.0),&publication.owner,&publication)).map_err(|error|crate::error::NarouError::Platform(error.to_string()))?;
                    transaction.commit().map_err(crate::native::sqlite::sqlite_error)
                };if let Some(connection)=primary{save(connection)}else{save(&control.lock())}
            })).map_err(TransferError::platform)?;Ok(publication)
        }
        async fn check_plan(&self,spec:&PublicationSpec,p:&NativePublication)->TR<()>{
            self.journal.require_authority()?;let found=self.library_index(&native_scope()?,std::slice::from_ref(&spec.toc_url)).await?.into_iter().next();
            match (spec.target.action,found){(PlanAction::Create,None)=>{},(PlanAction::Overwrite,Some(m)) if Some(m.id.0)==spec.target.existing_id&&Some(m.record_revision.as_str())==spec.target.record_revision.as_deref()&&m.membership_digest==spec.target.membership_digest=>{},_=>return Err(TransferError::PlanStale{message:"library changed after plan confirmation".into()})}
            let prefix=ObjectPrefix::new(&p.prefix).map_err(TransferError::platform)?;
            let current=self.primary.gate_info(&prefix).await.map_err(TransferError::platform)?;
            if current.content_revision!=p.content_revision{return Err(TransferError::PlanStale{message:"canonical content changed outside this publication".into()});}
            self.primary.renew_gate(&prefix,&p.owner,p.epoch).await.map_err(TransferError::platform)
        }
    }

    impl TransferBackend for NativeTransferBackend {
        fn opaque_id(&self,prefix:&str)->String{format!("{prefix}_{}",uuid::Uuid::new_v4().simple())}
        fn storage_capabilities<'a>(&'a self,scope:&'a AuthScope)->PlatformFuture<'a,TR<StorageCapabilities>>{Box::pin(async move{self.ensure_scope(scope)?;let wasm=crate::web::frontend::transfer_asset_manifest().is_some();let gate=|allowed:bool,reason:Option<String>|CapabilityGate{allowed:allowed&&wasm,reason:if !wasm{Some("compiled transfer WASM assets are missing".into())}else{reason}};let mut limits=BTreeMap::new();limits.insert("max_file_bytes".into(),(i64::MAX as u64).to_string());limits.insert("max_indexed_url_bytes".into(),"8192".into());
            // The configured storage marker is the advertised method; when it
            // is unavailable the transfer refuses rather than silently
            // operating on the YAML fallback (an unmigrated schema is not a
            // YAML library, and the FS gate consult would still fail closed).
            let unavailable=self.journal.authority_error.clone();
            let methods=vec![if unavailable.is_some()||self.journal.state.is_some(){"sqlite"}else{"yaml"}.into()];
            Ok(StorageCapabilities{policy_version:1,methods,import:gate(unavailable.is_none(),unavailable.clone()),export:gate(unavailable.is_none(),unavailable),unpersistable_roles:Vec::new(),supports_reuse:false,available_bytes:fs2::available_space(&self.root).ok(),backend_limits:limits})})}
        fn issue_ticket<'a>(&'a self,scope:&'a AuthScope,valid:i64)->PlatformFuture<'a,TR<String>>{Box::pin(async move{use base64::Engine;self.ensure_scope(scope)?;let payload=serde_json::to_vec(&(scope,PROTOCOL_VERSION,Utc::now().timestamp()+valid,uuid::Uuid::new_v4().to_string())).map_err(TransferError::platform)?;Ok(format!("{}.{}",base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&payload),self.sign_ticket(&payload)?))})}
        fn verify_ticket<'a>(&'a self,scope:&'a AuthScope,ticket:&'a str)->PlatformFuture<'a,TR<bool>>{Box::pin(async move{use base64::Engine;use subtle::ConstantTimeEq;let Some((encoded,signature))=ticket.split_once('.')else{return Ok(false)};let Ok(payload)=base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded)else{return Ok(false)};if !bool::from(self.sign_ticket(&payload)?.as_bytes().ct_eq(signature.as_bytes())){return Ok(false);}let Ok((owner,version,expires,_))=serde_json::from_slice::<(AuthScope,u32,i64,String)>(&payload)else{return Ok(false)};Ok(owner==*scope&&version==PROTOCOL_VERSION&&expires>Utc::now().timestamp())})}
        fn library_index<'a>(&'a self,scope:&'a AuthScope,urls:&'a [String])->PlatformFuture<'a,TR<Vec<LibraryMatch>>>{Box::pin(async move{
            self.ensure_scope(scope)?;if urls.len()>MAX_PAGE_ITEMS{return Err(TransferError::invalid("lookup_limit","at most100 URLs per lookup"));}let members=crate::native::virtual_novel::load().map_err(TransferError::platform)?;
            crate::db::with_database_mut(|db|{db.refresh()?;let mut matches=Vec::new();for url in urls{let folded=crate::db::novel_codec::fold(url);let record=db.get_by_toc_url(url).filter(|record|crate::db::novel_codec::fold(&record.toc_url)==folded).or_else(||db.all_records().values().find(|record|crate::db::novel_codec::fold(&record.toc_url)==folded));if let Some(record)=record{let kind=if crate::virtual_novel::is_virtual_record(record){NovelKind::VirtualParent}else{NovelKind::Normal};let membership_digest=members.get(record.id).map(|ids|crate::platform::access_fence::membership_digest(ids.iter().map(i64::to_string)));matches.push(LibraryMatch{id:NovelId(record.id),title:record.title.chars().take(DISPLAY_TITLE_CHARS).collect(),kind,record_revision:Self::record_revision(record).map_err(|error|crate::error::NarouError::Platform(error.to_string()))?,content_revision:self.content_revision(record).map_err(|error|crate::error::NarouError::Platform(error.to_string()))?,membership_digest});}}Ok(matches)}).map_err(TransferError::platform)
        })}
        fn find_reusable<'a>(&'a self,_scope:&'a AuthScope,_session:&'a SessionRow,_file:&'a FileRow)->PlatformFuture<'a,TR<Option<PinRef>>>{Box::pin(async{Ok(None)})}
        fn release_pin<'a>(&'a self,id:&'a SessionId,pin:&'a str)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.stage_abort(id,pin).await})}
        fn stage_begin<'a>(&'a self,id:&'a SessionId,file_id:u32,size:u64)->PlatformFuture<'a,TR<String>>{Box::pin(async move{
            let session=self.journal.session_for_work(id)?;if session.state.is_terminal(){return Err(TransferError::SessionExpired{resumable:false});}
            let handle=self.journal.run_write(|rows|{if let Some(handle)=rows.get::<String>("active_stage",&id.0,&number(file_id))?{return Ok(handle);}let handle=self.opaque_id("stage");let binding=StageBinding{owner:id.0.clone(),file_id,size,epoch:1};rows.put("backend","stages",&handle,&binding)?;rows.put("active_stage",&id.0,&number(file_id),&handle)?;Ok(handle)})?;
            let binding=self.stage_binding(&handle)?;if binding.owner!=id.0||binding.file_id!=file_id||binding.size!=size{return Err(TransferError::invalid("stage_identity_conflict","stage declaration changed"));}
            let cursor=match self.primary.stage_open(&handle,&id.0,1,SESSION_IDLE_TTL_SECS).await{Ok(cursor)=>cursor,Err(crate::error::NarouError::Platform(message))if message.starts_with("stage_checkpoint_invalid:")=>{let cursor=self.primary.stage_cursor(&handle).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("stage_missing","corrupt stage ledger is missing"))?;if cursor.sealed{return Err(TransferError::RecoveryRequired{message:"sealed stage checkpoint is corrupt; immutable data cannot be reset".into()});}let file=self.journal.file(id,file_id).await?.ok_or_else(||TransferError::invalid("file_missing","corrupt unverified file row missing"))?;self.restart_unverified_stage(&session,file,&handle).await?;return Err(TransferError::invalid("file_restart_required","unverified stage checkpoint is corrupt; restart at offset0"));},Err(error)=>return Err(TransferError::platform(error))};
            if !cursor.sealed&&let Some(file)=self.journal.file(id,file_id).await?{
                if cursor.confirmed_bytes>0&&cursor.prefix_sha256().is_none()||cursor.confirmed_bytes<file.confirmed_bytes||cursor.confirmed_bytes==file.confirmed_bytes&&cursor.prefix_sha256()!=file.confirmed_prefix_sha256 {
                    self.restart_unverified_stage(&session,file,&handle).await?;return Err(TransferError::invalid("file_restart_required","unverified stage checkpoint was lost; restart this file at offset0"));
                }
                if cursor.confirmed_bytes!=file.confirmed_bytes{
                    let state=file.hash_checkpoint.as_ref().and_then(|checkpoint|checkpoint.strip_prefix(&[1])).map(<[u8]>::to_vec);
                    let checkpoint=crate::platform::StageCursor::from_sha_checkpoint(file.confirmed_bytes,state);
                    if let Err(error)=self.primary.stage_rewind(&handle,&id.0,1,&checkpoint).await{let corrupt=match &error{crate::error::NarouError::Io(error)=>error.kind()==std::io::ErrorKind::NotFound,crate::error::NarouError::Platform(message)=>message.starts_with("stage_checkpoint_invalid:")||matches!(message.as_str(),"rewind SHA checkpoint conflict"|"rewind checkpoint SHA conflict"|"rewind checkpoint missing"),_=>false};if !corrupt{return Err(TransferError::platform(error));}self.restart_unverified_stage(&session,file,&handle).await?;return Err(TransferError::invalid("file_restart_required","unverified rewind checkpoint is unavailable; restart at offset0"));}
                }
            }Ok(handle)
        })}
        fn stage_append<'a>(&'a self,handle:&'a str,offset:u64,data:&'a [u8])->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.journal.require_authority()?;let b=self.stage_binding(handle)?;let cursor=self.primary.stage_cursor(handle).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("stage_missing","stage ledger missing"))?;if cursor.sealed&&cursor.confirmed_bytes==b.size{return Ok(());}self.primary.stage_append(handle,&b.owner,b.epoch,offset,data,offset+data.len() as u64==b.size).await.map_err(TransferError::platform)?;Ok(())})}
        fn stage_seal<'a>(&'a self,handle:&'a str,size:u64,sha:&'a str)->PlatformFuture<'a,TR<String>>{Box::pin(async move{self.journal.require_authority()?;let b=self.stage_binding(handle)?;self.primary.stage_seal(handle,&b.owner,b.epoch,size,sha).await.map_err(TransferError::platform)?;Ok(handle.into())})}
        fn stage_needs_finalize(&self)->bool{false}
        fn stage_finalize<'a>(&'a self,handle:&'a str,_cursor:Option<&'a str>)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(async move{let c=self.primary.stage_cursor(handle).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("stage_missing","stage missing"))?;if !c.sealed{return Err(TransferError::invalid("stage_unsealed","stage must be sealed before finalization"));}Ok(PublicationStep{done:true,..Default::default()})})}
        fn stage_read_range<'a>(&'a self,handle:&'a str,range:std::ops::Range<u64>)->PlatformFuture<'a,TR<Vec<u8>>>{Box::pin(async move{use futures::StreamExt;let expected=range.end.checked_sub(range.start).filter(|v|*v<=ADVANCE_BYTE_BUDGET).ok_or_else(||TransferError::invalid("invalid_range","stage range exceeds window"))?;let mut stream=self.primary.stage_read(handle,range).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("stage_missing","sealed stage is missing"))?;let mut bytes=Vec::with_capacity(expected as usize);while let Some(b)=stream.next().await{let b=b.map_err(TransferError::platform)?;if bytes.len()+b.len()>expected as usize{return Err(TransferError::invalid("stage_long_read","stage range exceeded expected bytes"));}bytes.extend_from_slice(&b);}if bytes.len() as u64!=expected{return Err(TransferError::invalid("stage_short_read","stage range truncated"));}Ok(bytes)})}
        fn stage_abort<'a>(&'a self,id:&'a SessionId,handle:&'a str)->PlatformFuture<'a,TR<()>>{Box::pin(async move{self.journal.require_authority()?;let binding=self.stage_binding(handle)?;if binding.owner!=id.0{return Err(TransferError::SecretMismatch);}self.primary.stage_abort(handle,&binding.owner,binding.epoch).await.map_err(TransferError::platform)?;self.journal.run_write(|rows|{if rows.get::<String>("active_stage",&id.0,&number(binding.file_id))?.as_deref()==Some(handle){rows.write("active_stage",&id.0,&number(binding.file_id),None)?;}Ok(())})})}


        fn export_inventory<'a>(&'a self,scope:&'a AuthScope,session:&'a SessionId,ids:&'a [i64],cursor:Option<&'a str>,_limit:usize)->PlatformFuture<'a,TR<Page<ExportNovelEntry>>>{Box::pin(async move{
            use std::io::{BufRead,Seek,SeekFrom};
            self.ensure_scope(scope)?;
            let Some(frozen)=self.frozen_selection(scope,session,ids)?else{return Ok(Page{items:Vec::new(),next_cursor:Some(serde_json::to_string(&InventoryPosition::default()).map_err(TransferError::platform)?)});};
            let total=frozen.ordinary.len()+frozen.parents.len();
            let mut position:InventoryPosition=cursor.map(serde_json::from_str).transpose().map_err(TransferError::platform)?.unwrap_or_default();
            let id=frozen.ordinary.get(position.novel).or_else(||position.novel.checked_sub(frozen.ordinary.len()).and_then(|n|frozen.parents.get(n))).copied();
            let Some(id)=id else{if position.novel==total{return Ok(Page{items:Vec::new(),next_cursor:None});}return Err(TransferError::invalid("invalid_cursor","inventory novel cursor out of range"));};
            let first=!position.metadata_emitted;let mut metadata=None;
            let source=if first{
                let mut record=crate::db::with_database_mut(|db|{db.refresh()?;db.get(id).cloned().ok_or_else(||crate::error::NarouError::NotFound(format!("source ID:{id}")))}).map_err(TransferError::platform)?;
                let parent=crate::virtual_novel::is_virtual_record(&record);if parent{record.toc_url=crate::native::virtual_novel::ensure_portable_identity(record.id).map_err(TransferError::platform)?;}
                super::assert_source_namespace(&record).map_err(|error|TransferError::invalid("ambiguous_source_namespace",format!("{error}; resolve overlapping saved folders before export")))?;
                crate::web::safe_existing_novel_dir(&self.archive_root(),&record).map_err(|e|TransferError::invalid("unsafe_source",e))?;
                let entry=ExportNovelEntry{session_id:session.clone(),novel_ordinal:position.novel as u32,id:NovelId(id),toc_url:record.toc_url.clone(),title:record.title.chars().take(DISPLAY_TITLE_CHARS).collect(),kind:if parent{NovelKind::VirtualParent}else{NovelKind::Normal},membership_digest:frozen.memberships.get(&id).map(|ids|crate::platform::access_fence::membership_digest(ids.iter().map(i64::to_string))),record_revision:Self::record_revision(&record)?,content_revision:self.content_revision(&record)?,metadata:None,files:Vec::new(),files_complete:false};
                let prefix=self.record_prefix(&record)?;let layout=self.source_metadata_layout(&entry,&record,&prefix,frozen.memberships.get(&record.id).map(Vec::len)).await?;metadata=Some(layout.clone());let source=SnapshotSource{entry,prefix:prefix.as_ref().to_owned(),layout};
                if let Some(previous)=self.journal.backend_get::<SnapshotSource>(&format!("snapshot_by_ordinal:{}",session.0),&number(position.novel as u32))?&&(previous.entry!=source.entry||previous.prefix!=source.prefix||previous.layout!=source.layout){return Err(TransferError::PlanStale{message:"export source changed during metadata capture".into()});}
                self.save_snapshot_source(session,&source)?;position.metadata_emitted=true;source
            }else{self.snapshot_source(session,position.novel as u32)?};
            let parent=source.entry.kind==NovelKind::VirtualParent;let prefix=ObjectKey::try_new(&source.prefix).map_err(TransferError::platform)?;let mut files=Vec::new();
            if !first&&position.phase==0{
                let mut request=crate::platform::ObjectListRequest::new(ObjectPrefix::from(prefix.clone()),std::num::NonZeroUsize::new(MAX_PAGE_ITEMS).expect("nonzero page"));request.cursor=position.after.clone();
                let page=self.objects.list_page(&request).await.map_err(TransferError::platform)?;
                for object in page.objects{let Some(path)=object.key.as_ref().strip_prefix(&format!("{}/",prefix.as_ref()))else{continue};
                    if !super::is_archivable_relative_path(path)||super::is_excluded_top_level(path)||super::is_excluded_file_name(path.rsplit('/').next().unwrap_or(path))||parent&&!crate::transfer::is_virtual_input_file(path){continue;}
                    let local=self.checked_source_path(&source.prefix,path)?;let size=match fs::metadata(local){Ok(metadata)if metadata.is_file()=>metadata.len(),_=>object.size};
                    if size.div_ceil(CHUNK_BYTES)>u64::from(u32::MAX){return Err(TransferError::invalid("chunk_index_overflow","source exceeds representable chunk count"));}
                    files.push(SourceFileEntry{logical_path:path.into(),size,known_role:None,source:SourceRef::Readable});self.journal.backend_put(&Self::source_owner(session,id),&sha256_hex(path.as_bytes()),&object.key.as_ref().to_string())?;
                }
                position.after=page.next_cursor;if position.after.is_none(){position.phase=1;}
            }else if !first&&position.phase==1{
                if parent{position.phase=2;}else{
                    let spool=self.illustration_spool(session,position.novel as u32,&prefix).await?;let offset=position.after.as_deref().unwrap_or("0").parse::<u64>().map_err(TransferError::platform)?;
                    let mut input=std::io::BufReader::with_capacity(64*1024,fs::File::open(self.root.join(&spool.path)).map_err(TransferError::platform)?);input.seek(SeekFrom::Start(offset)).map_err(TransferError::platform)?;
                    for _ in 0..MAX_PAGE_ITEMS{
                        let mut filename=String::new();let count=input.read_line(&mut filename).map_err(TransferError::platform)?;if count==0{position.phase=2;break;}
                        if !filename.ends_with('\n')||filename.len()>4097{return Err(TransferError::invalid("invalid_illustration_index","filename spool is corrupt"));}filename.pop();
                        position.after=Some(input.stream_position().map_err(TransferError::platform)?.to_string());let path=format!("挿絵/{filename}");
                        if self.journal.backend_get::<String>(&Self::source_owner(session,id),&sha256_hex(path.as_bytes()))?.is_some(){continue;}
                        let mut key=crate::platform::illustration_key(&prefix,&filename).map_err(TransferError::platform)?;let mut object=self.assets.stat(&key).await.map_err(TransferError::platform)?;
                        if object.is_none(){let stem=filename.rsplit_once('.').map_or(filename.as_str(),|(stem,_)|stem);if crate::illustration_store::is_sha256_hex_stem(stem){key=ObjectKey::try_new(format!("illustrations/{stem}")).map_err(TransferError::platform)?;object=self.assets.stat(&key).await.map_err(TransferError::platform)?;}}
                        let object=object.ok_or_else(||TransferError::invalid("illustration_missing",format!("referenced illustration {filename} is missing")))?;
                        files.push(SourceFileEntry{logical_path:path.clone(),size:object.size,known_role:Some(FileRole::Illustration),source:SourceRef::Readable});self.journal.backend_put(&Self::source_owner(session,id),&sha256_hex(path.as_bytes()),&key.as_ref().to_string())?;
                    }
                    if position.after.as_deref().and_then(|offset|offset.parse::<u64>().ok())==Some(spool.size){position.phase=2;}
                }
            }
            let complete=position.phase==2;let mut entry=source.entry;entry.metadata=metadata;entry.files=files;entry.files_complete=complete;
            if complete{position.novel+=1;position.phase=0;position.after=None;position.metadata_emitted=false;}
            Ok(Page{items:vec![entry],next_cursor:(position.novel<total).then(||serde_json::to_string(&position).map_err(TransferError::platform)).transpose()?})
        })}
        fn export_metadata_page<'a>(&'a self,session:&'a SessionId,ordinal:u32,cursor:&'a MetadataCursor,limit:usize)->PlatformFuture<'a,TR<ExportMetadataPage>>{Box::pin(self.metadata_page(session,ordinal,cursor,limit))}
        fn snapshot_gate<'a>(&'a self,id:&'a SessionId,entry:&'a ExportNovelEntry,_release:bool)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(async move{
            if !self.admit_snapshot(id).await?{return Ok(PublicationStep::default());}
            // The group stays closed until every selected file and parent is
            // fixed; releasing one member early would unfreeze its parent.
            self.check_owned_source(entry).await?;Ok(PublicationStep{done:true,..Default::default()})
        })}
        fn snapshot_finish<'a>(&'a self,id:&'a SessionId)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(self.finish_snapshot(id))}
        fn pin_source<'a>(&'a self,id:&'a SessionId,entry:&'a ExportNovelEntry,file:&'a SourceFileEntry)->PlatformFuture<'a,TR<PinRef>>{Box::pin(async move{
            let key=format!("snapshot_pin:{}:{}",entry.novel_ordinal,sha256_hex(file.logical_path.as_bytes()));let handle:String=self.journal.backend_get(&id.0,&key)?.ok_or_else(||TransferError::invalid("source_not_pinned","source is not a verified immutable session pin"))?;let cursor=self.primary.stage_cursor(&handle).await.map_err(TransferError::platform)?.ok_or_else(||TransferError::invalid("snapshot_missing","pinned stage missing"))?;if !cursor.sealed||cursor.confirmed_bytes!=file.size{return Err(TransferError::invalid("snapshot_missing","pinned stage identity changed"));}Ok(PinRef{pin:handle,size:file.size,sha256:cursor.prefix_sha256().ok_or_else(||TransferError::invalid("hash_checkpoint_corrupt","pin hash is unavailable"))?})
        })}
        fn source_read_range<'a>(&'a self,entry:&'a ExportNovelEntry,file:&'a SourceFileEntry,range:std::ops::Range<u64>)->PlatformFuture<'a,TR<Vec<u8>>>{Box::pin(async move{
            use std::io::{Read,Seek,SeekFrom};let source=self.check_owned_source(entry).await?;let path=self.checked_source_path(&source.prefix,&file.logical_path)?;
            let length=range.end.checked_sub(range.start).filter(|length|*length<=ADVANCE_BYTE_BUDGET&&range.end<=file.size).ok_or_else(||TransferError::invalid("invalid_range","source range exceeds budget or file"))?;
            if path.is_file(){let key=ObjectKey::try_new(format!("{}/{}",source.prefix.trim_end_matches('/'),file.logical_path)).map_err(TransferError::platform)?;let identity=self.frozen_source_generation(entry,file,&key).await?;let mut input=fs::File::open(&path).map_err(TransferError::platform)?;input.seek(SeekFrom::Start(range.start)).map_err(TransferError::platform)?;let mut bytes=vec![0;length as usize];input.read_exact(&mut bytes).map_err(TransferError::platform)?;if self.readable_generation(&key).await?.as_deref()!=Some(identity.as_str()){return Err(TransferError::PlanStale{message:"source file changed during snapshot range read".into()});}self.check_owned_source(entry).await?;return Ok(bytes);}
            let key:String=self.journal.backend_get(&Self::source_owner(&entry.session_id,entry.id.0),&sha256_hex(file.logical_path.as_bytes()))?.ok_or_else(||TransferError::invalid("source_missing","catalogued source key missing"))?;let key=ObjectKey::try_new(key).map_err(TransferError::platform)?;let identity=self.frozen_source_generation(entry,file,&key).await?;
            let initialized=self.legacy_source(&entry.session_id,&format!("initialized_source:{}:{}",entry.novel_ordinal,sha256_hex(file.logical_path.as_bytes())),&key,file.size,"snapshot_preparing").await?;let bytes=if let Some(stage)=initialized{self.stage_read_range(&stage,range).await?}else{self.read_bytes(&key,range).await?};if self.source_generation(&key).await?.as_deref()!=Some(identity.as_str()){return Err(TransferError::PlanStale{message:"source generation changed during snapshot".into()});}self.check_owned_source(entry).await?;Ok(bytes)
        })}
        fn source_fingerprint<'a>(&'a self,entry:&'a ExportNovelEntry,file:&'a SourceFileEntry)->PlatformFuture<'a,TR<Option<String>>>{Box::pin(async move{self.check_owned_source(entry).await?;let key:String=self.journal.backend_get(&Self::source_owner(&entry.session_id,entry.id.0),&sha256_hex(file.logical_path.as_bytes()))?.ok_or_else(||TransferError::invalid("source_missing","source key missing"))?;self.source_generation(&ObjectKey::try_new(key).map_err(TransferError::platform)?).await})}
        fn publication_gate_close<'a>(&'a self,spec:&'a PublicationSpec,_cursor:Option<&'a str>)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(async move{let mut publication=self.create_publication(spec)?;let prefix=ObjectPrefix::new(&publication.prefix).map_err(TransferError::platform)?;publication.epoch=self.primary.close_gate(&prefix,&publication.owner).await.map_err(TransferError::platform)?;self.save_publication(spec,&publication)?;Ok(PublicationStep{done:true,assigned_id:Some(publication.record_id),..Default::default()})})}
        fn publication_drain<'a>(&'a self,spec:&'a PublicationSpec,_cursor:Option<&'a str>)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(async move{self.journal.require_authority()?;let p=self.publication(spec)?;let prefix=ObjectPrefix::new(&p.prefix).map_err(TransferError::platform)?;self.primary.renew_gate(&prefix,&p.owner,p.epoch).await.map_err(TransferError::platform)?;let info=self.primary.gate_info(&prefix).await.map_err(TransferError::platform)?;Ok(PublicationStep{done:info.pending_readers==0,..Default::default()})})}
        fn publication_prepare<'a>(&'a self,spec:&'a PublicationSpec,_cursor:Option<&'a str>)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(self.prepare_publication(spec))}
        fn publication_install<'a>(&'a self,spec:&'a PublicationSpec,_cursor:Option<&'a str>)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(self.install_publication(spec))}
        fn publication_commit<'a>(&'a self,spec:&'a PublicationSpec,_cursor:Option<&'a str>)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(async move{
            self.journal.require_authority()?;let mut p=self.publication(spec)?;let directory=self.publication_directory(spec);let receipt=PublicationReceipt{novel_ordinal:spec.publication_id,action:spec.target.action,outcome:if spec.target.action==PlanAction::Create{"created"}else{"overwritten"}.into(),assigned_id:Some(p.record_id),files_installed:spec.file_count,error:None};let bytes=serde_json::to_vec(&receipt).map_err(TransferError::platform)?;
            let marker=super::TransferCommitMarker{session_id:spec.session_id.0.clone(),publication_id:spec.publication_id,novel_id:NovelId(p.record_id),receipt:bytes.clone(),yaml_commit_path:directory.join("committed")};
            super::save_expected_metadata_receipt(&self.root,&marker).map_err(TransferError::platform)?;
            if self.committed(spec)?{p.committed=true;self.save_publication(spec,&p)?;return Ok(PublicationStep{done:true,committed:true,assigned_id:Some(p.record_id),receipt:Some(bytes),..Default::default()});}
            self.check_plan(spec,&p).await?;let member_ids=spec.member_ids.clone();let parent=spec.kind==NovelKind::VirtualParent;let target=spec.target.clone();
            self.primary.with_closed_write(&ObjectPrefix::new(&p.prefix).map_err(TransferError::platform)?,&p.owner,p.epoch,||crate::converter::clear_section_convert_cache(p.record_id)).map_err(TransferError::platform)?;
            // New-record settings/site lookups may read SQLite; resolve them before its metadata transaction.
            let new_record=if target.action==PlanAction::Create{Some(self.make_record(spec,p.record_id,None)?)}else{None};
            let result=crate::db::with_database_mut(|db|db.update_transfer_records(&marker,|mut records,mut members|{
                let current=records.get(&p.record_id);
                if target.action==PlanAction::Overwrite && current.is_none(){return Err(crate::error::NarouError::Platform("plan_stale: destination deleted".into()));}
                if let Some(current)=current&&(target.action==PlanAction::Create||Some(Self::record_revision(current).map_err(|e|crate::error::NarouError::Platform(e.to_string()))?)!=target.record_revision){return Err(crate::error::NarouError::Platform("plan_stale: destination changed".into()));}
                let existing=records.remove(&p.record_id);
                let mut record=match existing{Some(existing)=>self.make_record(spec,p.record_id,Some(existing)).map_err(|e|crate::error::NarouError::Platform(e.to_string()))?,None=>new_record.ok_or_else(||crate::error::NarouError::Platform("plan_stale: destination disappeared".into()))?};
                record.file_title=p.file_title.clone();record.sitename=p.sitename.clone();record.use_subdirectory=p.use_subdirectory;record.is_narou=p.is_narou;
                match target.action{PlanAction::Create=>{if records.contains_key(&record.id)||records.values().any(|r|r.toc_url==record.toc_url){return Err(crate::error::NarouError::Platform("plan_stale: destination identity was claimed".into()));}},PlanAction::Overwrite=>{},_=>return Err(crate::error::NarouError::Platform("invalid publication action".into()))}
                if parent{crate::virtual_novel::validate_members(&member_ids).map_err(crate::error::NarouError::Platform)?;for id in &member_ids{let member=records.get(id).ok_or_else(||crate::error::NarouError::Platform("member disappeared".into()))?;if crate::virtual_novel::is_virtual_record(member)||members.groups.iter().any(|(parent,ids)|*parent!=record.id&&ids.contains(id)){return Err(crate::error::NarouError::Platform("membership_conflict".into()));}}members.groups.insert(record.id,member_ids.clone());if !record.tags.contains(&crate::virtual_novel::tag(record.id)){record.tags.push(crate::virtual_novel::tag(record.id));}}
                records.insert(record.id,record);Ok((records,members,()))
            }));
            if let Err(error)=result&&!self.committed(spec)?{return Err(TransferError::platform(error));}p.committed=true;self.save_publication(spec,&p)?;Ok(PublicationStep{done:true,committed:true,assigned_id:Some(p.record_id),receipt:Some(bytes),..Default::default()})
        })}
        fn publication_rollback<'a>(&'a self,spec:&'a PublicationSpec,cursor:Option<&'a str>)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(self.rollback_publication(spec,cursor))}
        fn publication_finish<'a>(&'a self,spec:&'a PublicationSpec,_cursor:Option<&'a str>)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(async move{
            self.journal.require_authority()?;if !self.committed(spec)?{return Err(TransferError::RecoveryRequired{message:"publication commit marker is absent".into()});}super::recover_metadata(&self.publication_directory(spec)).map_err(TransferError::platform)?;let mut p=self.publication(spec)?;p.committed=true;self.save_publication(spec,&p)?;let receipt=PublicationReceipt{novel_ordinal:spec.publication_id,action:spec.target.action,outcome:if spec.target.action==PlanAction::Create{"created"}else{"overwritten"}.into(),assigned_id:Some(p.record_id),files_installed:spec.file_count,error:None};Ok(PublicationStep{done:true,committed:true,assigned_id:Some(p.record_id),receipt:Some(serde_json::to_vec(&receipt).map_err(TransferError::platform)?),..Default::default()})
        })}
        fn publication_gate_open<'a>(&'a self,spec:&'a PublicationSpec,_cursor:Option<&'a str>)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(async move{
            self.journal.require_authority()?;if self.journal.backend_get::<NativePublication>(&spec.session_id.0,&format!("publication:{}",spec.publication_id))?.is_none(){return Ok(PublicationStep{done:true,..Default::default()});}
            let publication=self.publication(spec)?;
            if publication.epoch!=0{let prefix=ObjectPrefix::new(&publication.prefix).map_err(TransferError::platform)?;let current=self.primary.gate_info(&prefix).await.map_err(TransferError::platform)?;if current.owner.as_deref()==Some(&publication.owner)&&current.epoch==publication.epoch{self.primary.open_gate(&prefix,&publication.owner,publication.epoch).await.map_err(TransferError::platform)?;}else if current.owner.is_some()||current.epoch!=publication.epoch{return Err(TransferError::RecoveryRequired{message:"publication gate no longer belongs to this plan".into()});}}
            let connection=self.journal.control_connection();connection.lock().execute("DELETE FROM native_transfer_target_claims WHERE owner=?1 AND session_id=?2 AND publication_id=?3 AND plan_revision=?4",rusqlite::params![publication.owner,spec.session_id.0,spec.publication_id,spec.plan_revision]).map_err(TransferError::platform)?;
            Ok(PublicationStep{done:true,..Default::default()})
        })}
        fn release_session_resources<'a>(&'a self,session:&'a SessionRow,cursor:Option<&'a str>)->PlatformFuture<'a,TR<PublicationStep>>{Box::pin(self.collect_session_resources(session,cursor))}
    }

    impl NativeTransferBackend {
        fn committed(&self,spec:&PublicationSpec)->TR<bool>{
            let receipt=if let Some(state)=&self.journal.state{let connection=state.conn();let connection=connection.lock();connection.query_row("SELECT value FROM transfer_backend_marks WHERE key=?1",[format!("commit:{}:{}",spec.session_id.0,spec.publication_id)],|row|row.get::<_,Vec<u8>>(0)).optional().map_err(TransferError::platform)?}else{match fs::read(self.publication_directory(spec).join("committed")){Ok(bytes)=>Some(bytes),Err(error)if error.kind()==std::io::ErrorKind::NotFound=>None,Err(error)=>return Err(TransferError::platform(error))}};
            let Some(receipt)=receipt else{return Ok(false)};let publication=self.publication(spec)?;
            let expected=PublicationReceipt{novel_ordinal:spec.publication_id,action:spec.target.action,outcome:if spec.target.action==PlanAction::Create{"created"}else{"overwritten"}.into(),assigned_id:Some(publication.record_id),files_installed:spec.file_count,error:None};
            if receipt!=serde_json::to_vec(&expected).map_err(TransferError::platform)?{return Err(TransferError::RecoveryRequired{message:"immutable publication receipt conflicts with this plan".into()});}Ok(true)
        }
    }
}

/// Private bookkeeping connection, usable before Database initialization.
pub(crate) fn control_connection(root:&Path)->Result<Arc<parking_lot::Mutex<rusqlite::Connection>>>{
    let journal=v2::NativeTransferJournal::open(root).map_err(|error|NarouError::Platform(error.to_string()))?;Ok(journal.control_connection())
}

/// The caller already holds the real common metadata authority. SQL's supplied
/// primary connection must never be re-locked through the journal accessor.
pub(crate) fn with_control_metadata_snapshot<T>(root:&Path,primary:Option<&rusqlite::Connection>,capture:impl FnOnce(&rusqlite::Connection)->Result<T>)->Result<T>{
    if let Some(connection)=primary{return capture(connection);}let connection=control_connection(root)?;let connection=connection.lock();capture(&connection)
}

fn reclaim_metadata_images(root:&Path,session:&str,publication:u32,limit:usize)->Result<bool>{
    let publication_root=root.join(".narou/transfer/publications").join(session).join(publication.to_string());validate_publication_root(root,&publication_root)?;let relative=publication_root.strip_prefix(root).map_err(|_|metadata_error("publication is outside library"))?.to_string_lossy().replace('\\',"/");match fs::read_to_string(root.join(".narou/transfer/metadata-pending")){Ok(owner)if owner==relative=>return Err(metadata_error("pending authoritative metadata still protects its images")),Ok(_)=>{},Err(error)if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(error.into())}
    let directory=publication_root.join("metadata");let entries=match fs::read_dir(&directory){Ok(entries)=>entries,Err(error)if error.kind()==std::io::ErrorKind::NotFound=>return Ok(true),Err(error)=>return Err(error.into())};if metadata_resolution(&publication_root)?.is_none(){return Err(metadata_error("unresolved metadata images cannot be reclaimed"));}let limit=limit.clamp(1,crate::application::transfer::MAX_PAGE_ITEMS);let mut count=0;for entry in entries.take(limit){let entry=entry?;if entry.file_type()?.is_dir(){return Err(metadata_error("unexpected nested metadata image directory"));}fs::remove_file(entry.path())?;count+=1;}if count==limit{return Ok(false);}match fs::remove_dir(directory){Ok(())=>Ok(true),Err(error)if error.kind()==std::io::ErrorKind::NotFound=>Ok(true),Err(error)=>Err(error.into())}
}

fn metadata_resolution(publication_root:&Path)->Result<Option<bool>>{
    match read_metadata_receipt(&publication_root.join("metadata-resolved"))?{None=>Ok(None),Some(value)if value==b"committed"=>Ok(Some(true)),Some(value)if value==b"rolled_back"=>Ok(Some(false)),Some(_)=>Err(metadata_error("metadata resolution marker is corrupt"))}
}

pub fn v2_service()->std::result::Result<Arc<crate::application::transfer::TransferService>,crate::application::transfer::TransferError>{
    let root=crate::db::inventory::find_narou_root().map_err(crate::application::transfer::TransferError::platform)?;let journal=Arc::new(v2::NativeTransferJournal::open(&root)?);let backend=Arc::new(v2::NativeTransferBackend::open(root,journal.clone())?);
    let build=crate::application::transfer::TransferBuildInfo{wasm_build_id:crate::web::frontend::transfer_build_id().map(str::to_owned),abi_version:crate::web::frontend::transfer_abi_version().map(|v|v.to_string())};
    Ok(Arc::new(crate::application::transfer::TransferService::new(backend,journal,Arc::new(crate::platform::clock::SystemClock),build)))
}

pub fn recover_startup()->Result<()>{
    let service=v2_service().map_err(|e|NarouError::Platform(e.to_string()))?;let scope=v2::native_scope().map_err(|e|NarouError::Platform(e.to_string()))?;
    loop{let report=futures::executor::block_on(service.recover(&scope)).map_err(|e|NarouError::Platform(e.to_string()))?;if report.pending_recovery==0{break;}
        if report.stepped_publications==0{return Err(NarouError::Platform("recovery_required: unfinished transfer publication must be recovered before opening the library".into()));}}
    futures::executor::block_on(service.expire_idle(&scope)).map_err(|e|NarouError::Platform(e.to_string()))?;Ok(())
}


#[derive(Debug,Clone,serde::Serialize,serde::Deserialize)]
struct MetadataImage { name:String, old_sha256:Option<String>, new_sha256:String }

fn metadata_error(message:impl Into<String>)->NarouError{NarouError::Platform(format!("recovery_required: {}",message.into()))}

fn metadata_name(path:&Path)->Result<&str>{
    let name=path.file_name().and_then(|n|n.to_str()).ok_or_else(||metadata_error("invalid metadata filename"))?;
    if !matches!(name,"tag_sequence.yaml"|"virtual_novel_members.yaml"|"database.yaml")||path.parent().and_then(|p|p.file_name()).and_then(|n|n.to_str())!=Some(".narou"){return Err(metadata_error("metadata journal target is not a library inventory"));}Ok(name)
}

fn durable_metadata_bytes(path:&Path,bytes:&[u8])->Result<()>{
    let parent=path.parent().ok_or_else(||metadata_error("metadata path has no parent"))?;fs::create_dir_all(parent)?;let mut file=tempfile::NamedTempFile::new_in(parent)?;for chunk in bytes.chunks(crate::transfer::protocol::IO_CHUNK_BYTES){file.write_all(chunk)?;}file.as_file().sync_all()?;file.persist(path).map_err(|e|NarouError::Io(e.error))?;
    #[cfg(unix)] fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn metadata_fingerprint(path:&Path)->Result<Option<String>>{
    use sha2::Digest;
    let mut file=match fs::File::open(path){Ok(file)=>file,Err(error)if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(error)=>return Err(error.into())};let mut hash=sha2::Sha256::new();let mut buffer=[0u8;crate::transfer::protocol::IO_CHUNK_BYTES];loop{let count=file.read(&mut buffer)?;if count==0{break;}hash.update(&buffer[..count]);}Ok(Some(hex::encode(hash.finalize())))
}

fn durable_metadata_image(path:&Path,source:&Path,expected:&str)->Result<()>{
    use sha2::Digest;
    let parent=path.parent().ok_or_else(||metadata_error("metadata path has no parent"))?;fs::create_dir_all(parent)?;let mut input=fs::File::open(source)?;let mut output=tempfile::NamedTempFile::new_in(parent)?;let mut hash=sha2::Sha256::new();let mut buffer=[0u8;crate::transfer::protocol::IO_CHUNK_BYTES];loop{let count=input.read(&mut buffer)?;if count==0{break;}hash.update(&buffer[..count]);output.write_all(&buffer[..count])?;}if hex::encode(hash.finalize())!=expected{return Err(metadata_error("metadata private image is corrupt"));}output.as_file().sync_all()?;output.persist(path).map_err(|error|NarouError::Io(error.error))?;
    #[cfg(unix)] fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_metadata_receipt(path:&Path)->Result<Option<Vec<u8>>>{
    let file=match fs::File::open(path){Ok(file)=>file,Err(error)if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(error)=>return Err(error.into())};let limit=crate::transfer::protocol::CONTROL_BODY_MAX_BYTES;let length=file.metadata()?.len();if length>limit{return Err(metadata_error("metadata receipt exceeds control budget"));}let mut bytes=Vec::with_capacity(length as usize);file.take(limit+1).read_to_end(&mut bytes)?;if bytes.len() as u64>limit{return Err(metadata_error("metadata receipt exceeds control budget"));}Ok(Some(bytes))
}

fn validate_metadata_receipt(publication_root:&Path,bytes:&[u8])->Result<crate::application::transfer::PublicationReceipt>{
    use crate::application::transfer::{PlanAction,PublicationReceipt};
    let publication=publication_root.file_name().and_then(|name|name.to_str()).and_then(|name|name.parse::<u32>().ok()).ok_or_else(||metadata_error("invalid publication ordinal"))?;let receipt:PublicationReceipt=serde_json::from_slice(bytes).map_err(|error|metadata_error(error.to_string()))?;if receipt.novel_ordinal!=publication||receipt.assigned_id.is_none()||receipt.error.is_some()||!matches!((receipt.action,receipt.outcome.as_str()),(PlanAction::Create,"created")|(PlanAction::Overwrite,"overwritten")){return Err(metadata_error("metadata commit receipt identity differs"));}Ok(receipt)
}

fn save_expected_metadata_receipt(root:&Path,marker:&TransferCommitMarker)->Result<()>{
    let publication_root=marker.yaml_commit_path.parent().ok_or_else(||metadata_error("publication root missing"))?;validate_publication_root(root,publication_root)?;let receipt=validate_metadata_receipt(publication_root,&marker.receipt)?;if receipt.novel_ordinal!=marker.publication_id||receipt.assigned_id!=Some(marker.novel_id.0)||publication_root.parent().and_then(Path::file_name).and_then(|name|name.to_str())!=Some(marker.session_id.as_str()){return Err(metadata_error("metadata marker identity differs"));}let path=publication_root.join("expected-receipt");if let Some(existing)=read_metadata_receipt(&path)?{if existing!=marker.receipt{return Err(metadata_error("immutable expected receipt conflicts"));}return Ok(());}durable_metadata_bytes(&path,&marker.receipt)
}

fn validate_publication_root(library_root:&Path,publication_root:&Path)->Result<()>{
    let base=library_root.join(".narou/transfer/publications");let relative=publication_root.strip_prefix(&base).map_err(|_|metadata_error("metadata journal escapes private publication namespace"))?;let components=relative.components().collect::<Vec<_>>();if components.len()!=2||components.iter().any(|p|!matches!(p,std::path::Component::Normal(_))){return Err(metadata_error("metadata journal owner is invalid"));}Ok(())
}

/// Capture metadata bytes while the caller holds the inventory locks. No
/// canonical metadata is changed until every postimage has been journaled.
pub fn stage_metadata(marker:&TransferCommitMarker,path:&Path,original:Option<&str>,replacement:&str)->Result<()>{
    let name=metadata_name(path)?;let library_root=path.parent().and_then(Path::parent).ok_or_else(||metadata_error("inventory root missing"))?;let publication_root=marker.yaml_commit_path.parent().ok_or_else(||metadata_error("publication root missing"))?;validate_publication_root(library_root,publication_root)?;
    save_expected_metadata_receipt(library_root,marker)?;
    if metadata_resolution(publication_root)?.is_some(){return Err(metadata_error("resolved metadata publication cannot be staged again"));}
    let pending=library_root.join(".narou/transfer/metadata-pending");let owner=publication_root.strip_prefix(library_root).map_err(|_|metadata_error("publication is outside library"))?.to_string_lossy().replace('\\',"/");
    if let Ok(existing)=fs::read_to_string(&pending)&&existing!=owner{return Err(metadata_error("another publication owns pending library metadata"));}
    durable_metadata_bytes(&pending,owner.as_bytes())?;let directory=publication_root.join("metadata");let descriptor=directory.join(format!("{name}.json"));let image=MetadataImage{name:name.into(),old_sha256:original.map(|s|crate::application::transfer::sha256_hex(s.as_bytes())),new_sha256:crate::application::transfer::sha256_hex(replacement.as_bytes())};
    if descriptor.exists(){let existing:MetadataImage=serde_json::from_slice(&fs::read(&descriptor)?).map_err(|e|metadata_error(e.to_string()))?;if existing.old_sha256!=image.old_sha256||existing.new_sha256!=image.new_sha256{return Err(metadata_error("metadata replay changed its preimage or postimage"));}return Ok(());}
    if let Some(original)=original{durable_metadata_bytes(&directory.join(format!("{name}.undo")),original.as_bytes())?;}durable_metadata_bytes(&directory.join(format!("{name}.new")),replacement.as_bytes())?;durable_metadata_bytes(&descriptor,&serde_json::to_vec(&image).map_err(|e|metadata_error(e.to_string()))?)
}

/// Install an already-journaled postimage. Unknown current bytes stop rather
/// than overwriting an unrelated edit. The private postimage is retained.
pub fn install_metadata(marker:&TransferCommitMarker,path:&Path)->Result<()>{
    let name=metadata_name(path)?;let publication_root=marker.yaml_commit_path.parent().ok_or_else(||metadata_error("publication root missing"))?;let image:MetadataImage=serde_json::from_slice(&fs::read(publication_root.join("metadata").join(format!("{name}.json")))?).map_err(|e|metadata_error(e.to_string()))?;apply_metadata_image(path,publication_root,&image,true)
}

fn apply_metadata_image(path:&Path,publication_root:&Path,image:&MetadataImage,committed:bool)->Result<()>{
    if metadata_name(path)?!=image.name{return Err(metadata_error("metadata descriptor target differs"));}let current=metadata_fingerprint(path)?;
    let expected=if committed{Some(&image.new_sha256)}else{image.old_sha256.as_ref()};if current.as_ref()==expected{return Ok(());}
    if current.as_ref()!=Some(&image.new_sha256)&&current!=image.old_sha256{return Err(metadata_error(format!("{name} differs from both journal fingerprints; preserving bytes",name=image.name)));}
    if let Some(expected)=expected{let suffix=if committed{"new"}else{"undo"};durable_metadata_image(path,&publication_root.join("metadata").join(format!("{}.{suffix}",image.name)),expected)}else{match fs::remove_file(path){Ok(())=>Ok(()),Err(e)if e.kind()==std::io::ErrorKind::NotFound=>Ok(()),Err(e)=>Err(e.into())}}
}

/// Recover with compat, tag sequence, membership and database locks already
/// held. A commit mark is irrevocable: postmark recovery only finishes redo.
pub fn recover_metadata_locked(library_root:&Path,publication_root:&Path,committed:bool)->Result<()>{
    validate_publication_root(library_root,publication_root)?;let pending=library_root.join(".narou/transfer/metadata-pending");let owner=publication_root.strip_prefix(library_root).map_err(|_|metadata_error("publication is outside library"))?.to_string_lossy().replace('\\',"/");let current_owner=match fs::read_to_string(&pending){Ok(value)=>Some(value),Err(error)if error.kind()==std::io::ErrorKind::NotFound=>None,Err(error)=>return Err(error.into())};
    let resolved=metadata_resolution(publication_root)?;if resolved.is_some_and(|value|value!=committed){return Err(metadata_error("metadata resolution conflicts with durable commit authority"));}
    if resolved.is_none(){
        let directory=publication_root.join("metadata");if directory.exists(){if current_owner.as_deref()!=Some(owner.as_str()){return Err(metadata_error("unresolved metadata journal lost its owner pointer"));}let mut descriptors=fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;descriptors.sort_by_key(|entry|entry.file_name());for entry in descriptors{if entry.path().extension().is_none_or(|extension|extension!="json"){continue;}let image:MetadataImage=serde_json::from_slice(&fs::read(entry.path())?).map_err(|error|metadata_error(error.to_string()))?;let path=library_root.join(".narou").join(&image.name);metadata_name(&path)?;apply_metadata_image(&path,publication_root,&image,committed)?;}}
        // Resolution precedes pointer removal, so a retry cannot redo over a
        // later legitimate mutation of the shared global Inventory files.
        durable_metadata_bytes(&publication_root.join("metadata-resolved"),if committed{b"committed"}else{b"rolled_back"})?;
    }
    if current_owner.as_deref()==Some(owner.as_str()){fs::remove_file(&pending)?;#[cfg(unix)] fs::File::open(pending.parent().ok_or_else(||metadata_error("pending metadata parent missing"))?)?.sync_all()?;}Ok(())
}

/// Library-level pending metadata is always reconciled before ordinary
/// inventory mutation; this is called with the same inventory locks held.
pub fn recover_pending_metadata_locked(library_root:&Path)->Result<()>{
    let pending=library_root.join(".narou/transfer/metadata-pending");let owner=match fs::read_to_string(pending){Ok(owner)=>owner,Err(e)if e.kind()==std::io::ErrorKind::NotFound=>return Ok(()),Err(e)=>return Err(e.into())};let publication_root=library_root.join(owner);validate_publication_root(library_root,&publication_root)?;let committed=metadata_committed(library_root,&publication_root)?;recover_metadata_locked(library_root,&publication_root,committed)
}

pub fn recover_metadata(publication_root:&Path)->Result<()>{
    let root=crate::db::inventory::find_narou_root()?;validate_publication_root(&root,publication_root)?;let narou=root.join(".narou");let operation=||crate::db::inventory::with_exclusive_file_lock(&narou.join("tag_sequence.yaml"),||crate::db::inventory::with_exclusive_file_lock(&narou.join("virtual_novel_members.yaml"),||crate::db::inventory::with_exclusive_file_lock(&narou.join("database.yaml"),||recover_metadata_locked(&root,publication_root,metadata_committed(&root,publication_root)?))));
    if let Some(state)=crate::native::sqlite::state::active_for(&narou){state.with_compat_lock(operation)}else{operation()}
}


pub fn ensure_metadata_writable(library_root:&Path,marker:Option<&TransferCommitMarker>)->Result<()>{
    let path=library_root.join(".narou/transfer/metadata-pending");let pending=match fs::read_to_string(path){Ok(value)=>value,Err(e)if e.kind()==std::io::ErrorKind::NotFound=>return Ok(()),Err(e)=>return Err(e.into())};
    let same=marker.and_then(|m|m.yaml_commit_path.parent()).and_then(|p|p.strip_prefix(library_root).ok()).is_some_and(|p|p.to_string_lossy().replace('\\',"/")==pending);
    if same{Ok(())}else{Err(metadata_error("library metadata is reserved by an unresolved publication; recover before editing"))}
}


fn metadata_committed(root:&Path,publication_root:&Path)->Result<bool>{
    use rusqlite::OptionalExtension;
    validate_publication_root(root,publication_root)?;let publication=publication_root.file_name().and_then(|name|name.to_str()).and_then(|name|name.parse::<u32>().ok()).ok_or_else(||metadata_error("invalid publication ordinal"))?;let session=publication_root.parent().and_then(Path::file_name).and_then(|name|name.to_str()).ok_or_else(||metadata_error("invalid publication session"))?;let key=format!("commit:{session}:{publication}");
    let sql=if let Some(state)=crate::native::sqlite::state::active_for(&root.join(".narou")){let connection=state.conn();let connection=connection.lock();connection.query_row("SELECT value FROM transfer_backend_marks WHERE key=?1",[&key],|row|row.get::<_,Vec<u8>>(0)).optional().map_err(|error|metadata_error(error.to_string()))?}else{crate::native::sqlite::state::transfer_commit_receipt_before_open(&root.join(".narou"),&key)?};
    let yaml=read_metadata_receipt(&publication_root.join("committed"))?;if sql.as_ref().zip(yaml.as_ref()).is_some_and(|(sql,yaml)|sql!=yaml){return Err(metadata_error("SQL and YAML commit receipts conflict"));}let Some(receipt)=sql.or(yaml)else{return Ok(false)};let expected=read_metadata_receipt(&publication_root.join("expected-receipt"))?.ok_or_else(||metadata_error("immutable expected receipt is missing"))?;validate_metadata_receipt(publication_root,&receipt)?;if receipt!=expected{return Err(metadata_error("durable commit receipt differs from expected receipt"));}Ok(true)
}


pub fn recover_pending_metadata_before_open(root:&Path)->Result<()>{
    let narou=root.join(".narou");if !narou.join("transfer/metadata-pending").try_exists()?{return Ok(());}
    let operation=||crate::db::inventory::with_exclusive_file_lock(&narou.join("tag_sequence.yaml"),||crate::db::inventory::with_exclusive_file_lock(&narou.join("virtual_novel_members.yaml"),||crate::db::inventory::with_exclusive_file_lock(&narou.join("database.yaml"),||recover_pending_metadata_locked(root))));
    if let Some(state)=crate::native::sqlite::state::active_for(&narou){state.with_compat_lock(operation)}else{operation()}
}

#[cfg(test)]
#[path="transfer/metadata_tests.rs"]
mod metadata_tests;
