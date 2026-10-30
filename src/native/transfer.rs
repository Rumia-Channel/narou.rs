//! Native end-to-end glue for the portable transfer archive
//! ([`crate::transfer`]).
//!
//! The archive format itself is platform neutral and lives in `crate::transfer`.
//! This module connects it to a real library: it reads a novel directory from
//! disk (materialising DB-only content first), builds a manifest from the
//! `NovelRecord`, and on import allocates IDs, writes files and publishes the
//! records.
//!
//! Both the CLI (`narou export` / `narou import`) and the native Web API
//! (`/api/transfer/*`) call these functions so the two front ends cannot drift.
//!
//! # DB strategy / limitations
//!
//! * **Export** works in both storage modes. A novel that only lives in SQLite
//!   (`sqlite.mirror-files=false`) is materialised into its canonical directory
//!   for the duration of the export and cleaned up afterwards, exactly like
//!   `narou backup`.
//! * **Import** writes through the native object store, not straight to the
//!   filesystem. That keeps YAML mode, SQLite mode (rows + mirror) and an S3
//!   illustration backend consistent — in SQLite a stored row wins over the
//!   mirror, so a filesystem-only overwrite would otherwise be invisible.
//!   A library that stores nothing on disk (`sqlite.mirror-files=false`) is
//!   refused up front: the mirror is also the narou.rb-facing copy.
//! * A created record is a *new* library entry, not a byte-for-byte copy of the
//!   exported one: IDs are re-allocated, `file_title` is re-derived from the
//!   ncode/title and the site definition (the archive format does not carry the
//!   original folder name), and `original_url` / `raw_title` / login state are
//!   not transferred. Everything the reader observes (toc + 本文 + setting.ini +
//!   挿絵) is copied.
//! * **Overwrite** keeps the existing record id, folder and any extra local
//!   files. Files present in the archive are replaced atomically; library files
//!   that the archive does not carry (conversion history, newer sections, user
//!   edits) are left alone so an old archive cannot silently truncate a novel.
//!   Metadata is only filled in where the live record has no value.
//! * Only regular files are exported. Symlinks and reparse points are skipped so
//!   an export can never escape the novel directory. `backup/` and runtime
//!   artifacts (`*.lock`, `*.tmp`) are not part of a transfer.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::db::{self, NovelRecord};
use crate::downloader::settings::{DownloaderSettings, NativeDownloaderSettings};
use crate::downloader::site_setting;
use crate::downloader::types::TocFile;
use crate::downloader::util::{load_length_limit, sanitize_filename_with_limit};
use crate::error::{NarouError, Result};
use crate::native::novel_repository::NativeNovelRepository;
use crate::native::object_store::NativeStore;
use crate::platform::{AssetStream, NovelFilter, NovelId, NovelMutation, ObjectKey};
use crate::transfer::{
    ArchiveFile, ArchiveManifest, ArchiveNovel, ConflictPolicy, ExistingNovel, FORMAT_VERSION,
    ImportAction, ParsedNovel, build_archive, parse_archive, plan_import,
};
use crate::version;

/// Top-level novel-directory entries that are never exported.
///
/// `backup/` is what `narou backup` itself excludes (`compat::create_backup`);
/// lock and temporary files are runtime artifacts of the inventory writers and
/// would make the archive non-deterministic.
const EXCLUDED_TOP_LEVEL_DIRS: &[&str] = &["backup"];

/// The single file every novel directory must contain for a transfer.
const TOC_FILE: &str = "toc.yaml";

/// Result of a successful export.
#[derive(Debug, Clone)]
pub struct ExportResult {
    /// ZIP bytes produced by [`build_archive`].
    pub bytes: Vec<u8>,
    /// One entry per novel actually included, in argument order.
    pub novels: Vec<ExportedNovelInfo>,
}

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

/// Builds one ZIP archive for `ids`.
///
/// Duplicate ids are exported once. Unknown ids abort the whole export before
/// any bytes are returned; nothing in the library is modified.
pub fn export_archive_bytes(ids: &[i64]) -> Result<ExportResult> {
    if ids.is_empty() {
        return Err(NarouError::InvalidTarget(
            "書き出す小説が指定されていません".to_string(),
        ));
    }

    let repository = NativeNovelRepository::new();
    let archive_root = db::with_database(|database| Ok(database.archive_root().to_path_buf()))?;

    let mut seen = HashSet::new();
    let mut novels = Vec::new();
    let mut info = Vec::new();

    for &id in ids {
        if !seen.insert(id) {
            continue;
        }
        let record = repository
            .get_sync(NovelId(id))?
            .ok_or_else(|| NarouError::NotFound(format!("ID: {id}")))?;
        let novel_dir = db::existing_novel_dir_for_record(&archive_root, &record);
        ensure_exportable(&archive_root, &novel_dir)?;

        // DB-only storage has no files on disk: materialise the stored objects
        // for the duration of the read. The guards remove only what they wrote.
        let store = NativeStore::for_current_root()?;
        let materialized_files = store.materialize_novel_files(&novel_dir)?;
        let materialized_illustrations =
            crate::native::illustrations::materialize_blocking(&novel_dir)?;

        let files = collect_novel_files(&novel_dir)?;
        drop(materialized_illustrations);
        drop(materialized_files);

        if !files.iter().any(|file| file.path == TOC_FILE) {
            return Err(NarouError::NotFound(format!(
                "{} に {TOC_FILE} がありません",
                novel_dir.display()
            )));
        }

        let subtitle = files
            .iter()
            .find(|file| file.path == TOC_FILE)
            .and_then(|file| parse_toc_story(&file.bytes));

        info.push(ExportedNovelInfo {
            id,
            title: record.title.clone(),
            file_count: files.len(),
        });
        novels.push(archive_novel_from_record(&record, files, subtitle));
    }

    let manifest = ArchiveManifest {
        format_version: FORMAT_VERSION,
        app_version: Some(version::VERSION.to_string()),
        exported_at: Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
        novels,
    };
    let bytes = build_archive(&manifest)?;

    Ok(ExportResult {
        bytes,
        novels: info,
    })
}

/// Imports `bytes` into the current library.
///
/// `policy` decides what happens to a novel whose `toc_url` already exists.
/// Malformed archives abort before anything is written; per-novel problems are
/// reported in [`ImportOutcome::errors`].
pub fn import_archive_bytes(bytes: &[u8], policy: ConflictPolicy) -> Result<ImportOutcome> {
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

    let parsed = parse_archive(bytes)?;
    let repository = NativeNovelRepository::new();
    let (existing, records_by_url) = load_library_index(&repository)?;
    let plan = plan_import(&parsed, &existing, policy);
    let archive_root = db::with_database(|database| Ok(database.archive_root().to_path_buf()))?;

    let mut outcome = ImportOutcome::default();
    // A single archive may repeat a toc_url; the plan is computed against the
    // library as it was before the run, so track what this run already handled.
    let mut handled_urls: HashSet<String> = HashSet::new();

    for entry in &plan.entries {
        let novel = &parsed.novels[entry.index];
        let metadata = &parsed.manifest.novels[entry.index];

        if !handled_urls.insert(novel.toc_url.clone()) {
            outcome.errors.push(format!(
                "{}: 同じ toc_url がアーカイブ内で重複しています",
                describe(novel)
            ));
            continue;
        }

        let result = match entry.action {
            ImportAction::Skip => {
                outcome.skipped += 1;
                continue;
            }
            ImportAction::Create => create_novel(&repository, &archive_root, metadata, novel)
                .map(|_| outcome.created += 1),
            ImportAction::Overwrite => {
                let record = records_by_url.get(&novel.toc_url).ok_or_else(|| {
                    NarouError::NotFound(format!("{} はライブラリにありません", novel.toc_url))
                });
                record
                    .and_then(|record| {
                        overwrite_novel(&repository, &archive_root, record, metadata, novel)
                    })
                    .map(|_| outcome.overwritten += 1)
            }
        };

        if let Err(error) = result {
            outcome.errors.push(format!("{}: {error}", describe(novel)));
        }
    }

    Ok(outcome)
}

fn describe(novel: &ParsedNovel) -> String {
    if novel.title.is_empty() {
        novel.toc_url.clone()
    } else {
        format!("{} ({})", novel.title, novel.toc_url)
    }
}

/// Loads the conflict index (all novels) plus a `toc_url -> record` map.
fn load_library_index(
    repository: &NativeNovelRepository,
) -> Result<(Vec<ExistingNovel>, HashMap<String, NovelRecord>)> {
    let ids = repository.scan_ids_sync(&NovelFilter::default(), None, usize::MAX)?;
    let mut existing = Vec::with_capacity(ids.len());
    let mut by_url = HashMap::with_capacity(ids.len());
    for id in ids {
        let Some(record) = repository.get_sync(id)? else {
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

/// Collects every regular file below `novel_dir` as archive entries.
///
/// Paths are relative to `novel_dir` and always use `/`. Files that the archive
/// format cannot represent (Windows-reserved names, control characters, …) are
/// skipped rather than failing the export of the whole library.
pub fn collect_novel_files(novel_dir: &Path) -> Result<Vec<ArchiveFile>> {
    let mut files = Vec::new();
    collect_files_into(novel_dir, novel_dir, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn collect_files_into(base: &Path, current: &Path, files: &mut Vec<ArchiveFile>) -> Result<()> {
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
            collect_files_into(base, &path, files)?;
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

        files.push(ArchiveFile::new(relative, fs::read(&path)?));
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

fn parse_toc_story(bytes: &[u8]) -> Option<String> {
    let toc: TocFile = serde_yaml::from_slice(bytes).ok()?;
    toc.story.filter(|story| !story.trim().is_empty())
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
    repository: &NativeNovelRepository,
    archive_root: &Path,
    metadata: &ArchiveNovel,
    novel: &ParsedNovel,
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

    let toc = parse_archive_toc(&novel.files);
    let title = toc
        .as_ref()
        .map(|toc| toc.title.clone())
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| metadata.title.clone());
    if title.trim().is_empty() {
        return Err(NarouError::Platform(
            "アーカイブに作品名がありません".to_string(),
        ));
    }
    let author = toc
        .as_ref()
        .map(|toc| toc.author.clone())
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
        .map(|toc| toc.subtitles.len() as i64)
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

    if let Err(error) = write_novel_files(archive_root, &novel_dir, &novel.files, false) {
        remove_written_files(archive_root, &novel_dir, &novel.files);
        let _ = db::with_database_mut(|database| {
            database.release_id(reservation);
            Ok::<(), NarouError>(())
        });
        return Err(error);
    }

    if let Err(error) = repository.apply_batch_sync(vec![NovelMutation::Upsert(record)]) {
        remove_written_files(archive_root, &novel_dir, &novel.files);
        let _ = db::with_database_mut(|database| {
            database.release_id(reservation);
            Ok::<(), NarouError>(())
        });
        return Err(error);
    }

    Ok(id)
}

/// Replaces the files of an existing novel, keeping its record id and folder.
fn overwrite_novel(
    repository: &NativeNovelRepository,
    archive_root: &Path,
    record: &NovelRecord,
    metadata: &ArchiveNovel,
    novel: &ParsedNovel,
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

    write_novel_files(archive_root, &novel_dir, &novel.files, true)?;

    // Only fill fields the live record does not have. Changing title/author of
    // a managed novel is the downloader's job, and changing sitename/file_title
    // would move the folder out from under the files just written.
    let toc = parse_archive_toc(&novel.files);
    let toc_title = toc
        .as_ref()
        .map(|toc| toc.title.clone())
        .filter(|title| !title.trim().is_empty());
    let toc_author = toc
        .as_ref()
        .map(|toc| toc.author.clone())
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
            .map(|toc| toc.subtitles.len() as i64)
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
        repository.apply_batch_sync(vec![NovelMutation::Upsert(updated)])?;
    }
    Ok(())
}

fn parse_archive_toc(files: &[(String, Vec<u8>)]) -> Option<TocFile> {
    files
        .iter()
        .find(|(path, _)| path == TOC_FILE)
        .and_then(|(_, bytes)| serde_yaml::from_slice::<TocFile>(bytes).ok())
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
    archive_root: &Path,
    novel_dir: &Path,
    files: &[(String, Vec<u8>)],
    overwrite: bool,
) -> Result<()> {
    let stores = crate::native::object_store::NativeStores::for_current_root()?;
    let mut writes = Vec::with_capacity(files.len());
    for (relative, bytes) in files {
        let target = safe_join(novel_dir, relative)?;
        if !overwrite && target.exists() {
            return Err(NarouError::Platform(format!(
                "{} は既に存在します",
                target.display()
            )));
        }
        let key = crate::native::object_store::object_key_for_file(archive_root, &target)
            .ok_or_else(|| {
                NarouError::Platform(format!("保存先キーを作れません: {}", target.display()))
            })?;
        writes.push((key, bytes.clone()));
    }

    block_on_transfer(async move {
        for (key, bytes) in writes {
            let stream: AssetStream = Box::pin(futures::stream::once(async move { Ok(bytes) }));
            stores.assets.write_stream(&key, stream).await?;
        }
        Ok(())
    })
}

fn safe_join(base: &Path, relative: &str) -> Result<PathBuf> {
    if !is_archivable_relative_path(relative) {
        return Err(NarouError::Platform(format!(
            "安全でないファイルパスです: {relative:?}"
        )));
    }
    Ok(base.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

/// Drives an object-store write from synchronous CLI/Web code.
///
/// The store futures use `tokio::fs`/`spawn_blocking`, which need a runtime.
/// Inside one (the Web API runs the import on `spawn_blocking`) a fresh runtime
/// is built on a scoped thread, mirroring
/// [`crate::native::illustrations::materialize_blocking`]; the CLI has no
/// runtime and builds one directly.
fn block_on_transfer<F>(future: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>> + Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .spawn_scoped(scope, move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| NarouError::Platform(error.to_string()))?;
                    runtime.block_on(future)
                })
                .map_err(|error| NarouError::Platform(error.to_string()))?
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    } else {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| NarouError::Platform(error.to_string()))?;
        runtime.block_on(future)
    }
}

/// Best-effort cleanup of a partially written new novel directory.
///
/// Deletes the stored rows/objects first (so readers cannot see a row whose
/// body is about to disappear), then prunes the empty directories the import
/// created. A concurrent writer's files are never touched.
fn remove_written_files(archive_root: &Path, novel_dir: &Path, files: &[(String, Vec<u8>)]) {
    let keys: Vec<ObjectKey> = files
        .iter()
        .filter_map(|(relative, _)| {
            let target = safe_join(novel_dir, relative).ok()?;
            crate::native::object_store::object_key_for_file(archive_root, &target)
        })
        .collect();
    let _ = block_on_transfer(async move {
        let stores = crate::native::object_store::NativeStores::for_current_root()?;
        for key in keys {
            stores.objects.delete(&key).await?;
        }
        Ok(())
    });
    // Filesystem backends that never stored a row still have the mirror file.
    for (relative, _) in files {
        if let Ok(target) = safe_join(novel_dir, relative) {
            let _ = fs::remove_file(&target);
        }
    }
    // Prune directories that our write created, bottom-up. Only empty
    // directories are removed, so a concurrent writer keeps its data.
    let mut directories: Vec<PathBuf> = fs::read_dir(novel_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(fs::FileType::is_dir)
                .map(|_| entry.path())
        })
        .collect();
    directories.sort();
    directories.reverse();
    for directory in directories {
        prune_empty_directories(&directory);
    }
    let _ = fs::remove_dir(novel_dir);
}

fn prune_empty_directories(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            prune_empty_directories(&entry.path());
        }
    }
    let _ = fs::remove_dir(directory);
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
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
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
                .find(|file| file.path == "toc.yaml")
                .map(|file| file.bytes.clone()),
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
