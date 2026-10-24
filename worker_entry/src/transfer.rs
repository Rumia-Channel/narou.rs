//! `/api/transfer/export` / `/api/transfer/import` — portable novel archives.
//!
//! The archive format itself is platform neutral (`narou_rs::transfer`): the
//! manifest, the deterministic ZIP writer and the parser are shared with the
//! CLI and the native Web API. This module is only the Worker glue.
//!
//! `GET  /api/transfer/export?ids=1,2` returns `application/zip`.
//! `POST /api/transfer/import[?overwrite=true]` accepts a raw ZIP body and
//! returns `{success, created, overwritten, skipped, errors}`.
//!
//! # Storage strategy
//!
//! Both directions talk to the object store, never to a filesystem (the Worker
//! has none):
//!
//! * **Export** lists the novel's logical prefix ([`NovelObjectKeys`]) on the
//!   write store and reads every object back with `read_small`. The prefix is
//!   listed twice: once as the novel itself (primary / content backend) and
//!   once as `<prefix>/挿絵`, because [`SplitStore`] routes an illustration
//!   prefix to the illustration backend (S3) while the novel prefix only
//!   reaches the primary side. Keys are stored relative to the prefix, which is
//!   exactly the layout `parse_archive`/`build_archive` expect, and the same
//!   inclusion rules as the native export apply (`backup/`, `*.lock`, `*.tmp`
//!   and paths the archive format cannot represent are skipped). The Worker has
//!   no stored EPUB (it is built on demand by `download.epub`), so an export
//!   contains what the downloader/converter actually stored: `toc.yaml`,
//!   `本文/…`, `setting.ini`, `replace.txt`, `converter.yaml`, `novel.txt`,
//!   `.illustration_cache.yaml` and `挿絵/…`.
//! * **Import** writes through the same store with `write_small`, so D1,
//!   `content_*` (S3 body) and an S3 illustration backend all keep working and
//!   a later read sees the new bytes. Overwrite keeps the existing record id,
//!   folder (`sitename`/`file_title`/`use_subdirectory`) and every object the
//!   archive does not carry. Create allocates a D1 id, writes the files first
//!   and publishes the record last; a failure removes what this run wrote and
//!   releases the reservation.
//!
//! # Limits
//!
//! The route enforces the shared 512 MiB archive cap, but two platform limits
//! sit in front of it: Cloudflare caps a request body well below that, and the
//! small-object path (`ObjectStore::read_small` / `write_small`) is capped at
//! 16 MiB per object on both D1 and S3. An archive is therefore moved one small
//! object at a time; a novel file larger than that is reported (per novel on
//! import, as a request error naming the file on export) instead of being
//! silently truncated.
//!
//! [`SplitStore`]: narou_rs::platform::SplitStore

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, SecondsFormat, Utc};
use narou_rs::db::NovelRecord;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::types::TocFile;
use narou_rs::downloader::util::{load_length_limit, sanitize_filename_with_limit};
use narou_rs::platform::{
    NovelId, NovelMutation, NovelObjectKeys, ObjectKey, ObjectListRequest, ObjectPrefix,
    ObjectStore,
};
use narou_rs::transfer::{
    ArchiveFile, ArchiveManifest, ArchiveNovel, ConflictPolicy, ExistingNovel, FORMAT_VERSION,
    ImportAction, MAX_ARCHIVE_BYTES, ParsedNovel, build_archive, parse_archive, plan_import,
};
use worker::{Env, Method, Request, Response, console_log};

use crate::composition::WorkerRuntime;

/// Maximum request body accepted by `POST /api/transfer/import`.
///
/// The archive core enforces the same cap after decode; checking it here keeps
/// an oversized body out of the parser in the first place.
pub const MAX_IMPORT_BODY_BYTES: u64 = MAX_ARCHIVE_BYTES;

/// Largest object this route can move.
///
/// `ObjectStore::read_small` / `write_small` are capped at 16 MiB on both the
/// D1 and the S3 adapter (`SMALL_CAP`), so anything larger cannot travel
/// through the Worker without a streaming path.
const MAX_OBJECT_BYTES: u64 = 16 * 1024 * 1024;

/// Page size for object listings (the D1/S3 adapters both paginate by cursor).
const LIST_PAGE_SIZE: usize = 100;

/// Upper bound on listing pages per prefix, so a backend that keeps returning
/// a cursor cannot spin forever (100 pages = 10,000 objects per prefix).
const MAX_LIST_PAGES: usize = 100;

/// The single file every novel must contain for a transfer.
const TOC_FILE: &str = "toc.yaml";

/// Top-level novel-directory entries the native export never archives.
const EXCLUDED_TOP_LEVEL_DIRS: &[&str] = &["backup"];

/// Same limits `narou_rs::transfer` enforces when it validates a path.
const MAX_PATH_BYTES: usize = 4096;
const MAX_PATH_COMPONENT_CHARS: usize = 255;

/// Folder-name length limit used by the downloader (`folder-length-limit`).
///
/// Matches the native `derive_file_title` call: the Worker has no local setting
/// file, so `load_length_limit` returns the caller-provided default.
const FOLDER_LENGTH_LIMIT: Option<usize> = Some(50);

/// Entry point; `lib.rs` routes both transfer paths here.
pub async fn handle(req: Request, env: Env) -> worker::Result<Response> {
    match (req.method(), req.path().as_str()) {
        (Method::Get, "/api/transfer/export") => export(req, env).await,
        (Method::Post, "/api/transfer/import") => import(req, env).await,
        (
            _,
            "/api/transfer/export" | "/api/transfer/import",
        ) => Response::error("Method Not Allowed", 405),
        _ => Response::error("Not Found", 404),
    }
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// `GET /api/transfer/export?ids=1,2` — download a ZIP of the selected novels.
async fn export(req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let url = req.url().map_err(|error| worker::Error::RustError(error.to_string()))?;
    let ids = match parse_ids(crate::webui::query_param(&url, "ids").as_deref()) {
        Ok(ids) => ids,
        Err(message) => return export_error(400, message),
    };

    let runtime = match WorkerRuntime::build_ui(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("transfer export: service composition failed: {error}");
            return export_error(503, "Service unavailable".to_string());
        }
    };
    let objects = runtime.write_objects();

    let mut novels = Vec::with_capacity(ids.len());
    for id in ids {
        let record = match runtime.novels.get(NovelId(id)).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                return export_error(404, format!("ID: {id} が見つかりません"));
            }
            Err(error) => {
                return export_error(
                    500,
                    format!("ID: {id} のレコードを取得できません: {error}"),
                );
            }
        };
        let keys = match NovelObjectKeys::new(
            &record.sitename,
            &record.file_title,
            record.use_subdirectory,
        ) {
            Ok(keys) => keys,
            Err(error) => {
                return export_error(
                    400,
                    format!("ID: {id} の保存先キーを作れません: {error}"),
                );
            }
        };
        let files = match collect_novel_files(objects.as_ref(), &keys).await {
            Ok(files) => files,
            Err(message) => return export_error(400, format!("ID: {id}: {message}")),
        };
        if !files.iter().any(|file| file.path == TOC_FILE) {
            return export_error(404, format!("ID: {id} に {TOC_FILE} がありません"));
        }
        let subtitle = files
            .iter()
            .find(|file| file.path == TOC_FILE)
            .and_then(|file| parse_toc_story(&file.bytes));
        novels.push(archive_novel_from_record(&record, files, subtitle));
    }

    let manifest = ArchiveManifest {
        format_version: FORMAT_VERSION,
        app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        exported_at: Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
        novels,
    };
    let bytes = match build_archive(&manifest) {
        Ok(bytes) => bytes,
        Err(error) => return export_error(400, error.to_string()),
    };

    // UTC, not local time: wasm has no reliable local time zone in the Worker
    // runtime, and the name is only a default download name.
    let filename = format!("narou-export-{}.zip", Utc::now().format("%Y%m%d%H%M%S"));
    let disposition = narou_rs::application::webui::sanitize_content_disposition(&filename);
    // `from_bytes` sets `application/octet-stream`, so the type is set after
    // the body; `with_header` before `from_bytes` would be overwritten.
    let mut response = worker::ResponseBuilder::new()
        .with_header("Content-Disposition", &disposition)?
        .with_header("Cache-Control", "no-store")?
        .from_bytes(bytes)?;
    response.headers_mut().set("Content-Type", "application/zip")?;
    Ok(response)
}

/// Collects one novel's stored objects as archive entries (relative paths).
///
/// Mirrors the native `collect_novel_files`: the whole logical prefix is
/// archived except runtime artifacts and names the archive format cannot
/// represent, which are skipped rather than failing the export.
async fn collect_novel_files(
    store: &dyn ObjectStore,
    keys: &NovelObjectKeys,
) -> Result<Vec<ArchiveFile>, String> {
    let mut listed: BTreeMap<String, u64> = BTreeMap::new();
    let prefix = keys.prefix();
    list_prefix(store, prefix.as_ref(), &mut listed).await?;
    // Illustrations can live in a different backend (S3). `SplitStore` routes
    // only an illustration *prefix* to it, so the novel prefix above never
    // covers them there.
    let illustrations = ObjectPrefix::new(format!("{}/挿絵", prefix.as_ref()))
        .map_err(|error| format!("挿絵のプレフィックスを作れません: {error}"))?;
    list_prefix(store, illustrations.as_ref(), &mut listed).await?;

    let root = format!("{}/", prefix.as_ref());
    let mut files = Vec::new();
    for (key, size) in listed {
        let Some(relative) = key.strip_prefix(root.as_str()) else {
            continue;
        };
        let relative = relative.to_string();
        if is_excluded_top_level(&relative)
            || is_excluded_file_name(&relative)
            || !is_archivable_relative_path(&relative)
        {
            continue;
        }
        if size > MAX_OBJECT_BYTES {
            return Err(format!(
                "{relative} は {size} bytes で、Worker が 1 オブジェクトとして扱える上限 ({MAX_OBJECT_BYTES} bytes) を超えています"
            ));
        }
        let object_key =
            ObjectKey::try_new(key.clone()).map_err(|error| format!("{key}: {error}"))?;
        match store.read_small(&object_key).await {
            Ok(Some(bytes)) => files.push(ArchiveFile::new(relative, bytes)),
            // The object disappeared between LIST and GET; export what is
            // still there instead of failing the whole download.
            Ok(None) => {
                console_log!("transfer export: {key} は一覧後に消えました (スキップ)");
            }
            Err(error) => {
                return Err(format!("{relative} を読み出せません: {error}"));
            }
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

/// Lists every key below `prefix` and records its size.
async fn list_prefix(
    store: &dyn ObjectStore,
    prefix: &str,
    into: &mut BTreeMap<String, u64>,
) -> Result<(), String> {
    let prefix = ObjectPrefix::new(prefix)
        .map_err(|error| format!("オブジェクトのプレフィックスが不正です: {error}"))?;
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LIST_PAGES {
        let mut request = ObjectListRequest::new(
            prefix.clone(),
            std::num::NonZeroUsize::new(LIST_PAGE_SIZE).expect("page size is non-zero"),
        );
        request.cursor = cursor.take();
        let page = store
            .list_page(&request)
            .await
            .map_err(|error| format!("オブジェクト一覧を取得できません: {error}"))?;
        for metadata in page.objects {
            into.entry(metadata.key.as_ref().to_string())
                .or_insert(metadata.size);
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(()),
        }
    }
    Err(format!(
        "オブジェクト一覧が {MAX_LIST_PAGES} ページを超えました ({} 件)",
        into.len()
    ))
}

/// Builds the manifest metadata for one record (native
/// `archive_novel_from_record`).
fn archive_novel_from_record(
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
        novel_type: record.novel_type,
        files,
        ncode: record.ncode.clone(),
        subtitle,
        general_lastup: record.general_lastup.map(rfc3339),
        last_update: Some(rfc3339(record.last_update)),
    }
}

fn parse_toc_story(bytes: &[u8]) -> Option<String> {
    let toc: TocFile = serde_yaml::from_slice(bytes).ok()?;
    toc.story.filter(|story| !story.trim().is_empty())
}

fn rfc3339(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse_rfc3339(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// `POST /api/transfer/import[?overwrite=true]` — import a raw ZIP body.
async fn import(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if !is_zip_content_type(&req) {
        return import_error(
            415,
            "Content-Type は application/zip を指定してください".to_string(),
        );
    }
    // Refuse an oversized upload before buffering it. The platform enforces its
    // own request-body limit first; this is the archive contract's cap.
    let declared = req
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|value| value.trim().parse::<u64>().ok());
    if let Some(declared) = declared
        && declared > MAX_IMPORT_BODY_BYTES
    {
        return import_error(
            413,
            format!(
                "アーカイブが大きすぎます ({declared} bytes、上限 {MAX_IMPORT_BODY_BYTES} bytes)"
            ),
        );
    }

    let url = req.url().map_err(|error| worker::Error::RustError(error.to_string()))?;
    let policy = if flag_value(crate::webui::query_param(&url, "overwrite").as_deref()) {
        ConflictPolicy::Overwrite
    } else {
        ConflictPolicy::Skip
    };
    let body = match req.bytes().await {
        Ok(body) => body,
        Err(error) => {
            return import_error(400, format!("本文を読み取れません: {error}"));
        }
    };
    if body.is_empty() {
        return import_error(400, "空のリクエストです".to_string());
    }
    if body.len() as u64 > MAX_IMPORT_BODY_BYTES {
        return import_error(
            413,
            format!(
                "アーカイブが大きすぎます ({} bytes、上限 {MAX_IMPORT_BODY_BYTES} bytes)",
                body.len()
            ),
        );
    }

    let parsed = match parse_archive(&body) {
        Ok(parsed) => parsed,
        Err(error) => return import_error(400, error.to_string()),
    };

    let runtime = match WorkerRuntime::build_ui(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("transfer import: service composition failed: {error}");
            return import_error(503, "Service unavailable".to_string());
        }
    };
    let objects = runtime.write_objects();

    // Conflicts are resolved by `toc_url`. Resolve only the archives' URLs
    // through the indexed membership query, then fetch the matching records —
    // an import must not read the whole library.
    let urls: Vec<String> = parsed
        .novels
        .iter()
        .map(|novel| novel.toc_url.clone())
        .collect();
    let known = match runtime.novels.contains_toc_urls(&urls).await {
        Ok(known) if known.len() == urls.len() => known,
        Ok(_) => {
            return import_error(500, "URL の照合結果が不正です".to_string());
        }
        Err(error) => {
            return import_error(
                500,
                format!("ライブラリの照合に失敗しました: {error}"),
            );
        }
    };

    let mut existing = Vec::with_capacity(urls.len());
    let mut records_by_url: HashMap<String, NovelRecord> = HashMap::new();
    for (index, url) in urls.iter().enumerate() {
        if !known[index] {
            continue;
        }
        match runtime.novels.find_by_toc_url(url).await {
            Ok(Some(record)) => {
                records_by_url
                    .entry(record.toc_url.clone())
                    .or_insert_with(|| record.clone());
                existing.push(ExistingNovel {
                    toc_url: record.toc_url,
                    title: record.title,
                });
            }
            Ok(None) => {}
            Err(error) => {
                return import_error(
                    500,
                    format!("{url} のレコードを取得できません: {error}"),
                );
            }
        }
    }

    let plan = plan_import(&parsed, &existing, policy);
    let local = runtime
        .settings_store()
        .load(narou_rs::setting_core::SettingScope::Local)
        .await
        .unwrap_or_default();
    let now = Utc::now();

    let mut created = 0usize;
    let mut overwritten = 0usize;
    let mut skipped = 0usize;
    let mut errors = Vec::new();
    // The plan was computed against the library before the run; a single
    // archive may repeat a toc_url, so track what this run already handled.
    let mut handled_urls: HashSet<String> = HashSet::new();

    for entry in &plan.entries {
        let novel = &parsed.novels[entry.index];
        let metadata = &parsed.manifest.novels[entry.index];
        if !handled_urls.insert(novel.toc_url.clone()) {
            errors.push(format!(
                "{}: 同じ toc_url がアーカイブ内で重複しています",
                describe(novel)
            ));
            continue;
        }
        let result = match entry.action {
            ImportAction::Skip => {
                skipped += 1;
                continue;
            }
            ImportAction::Create => create_novel(
                &runtime,
                objects.as_ref(),
                metadata,
                novel,
                &local,
                now,
            )
            .await
            .map(|_| created += 1),
            ImportAction::Overwrite => {
                let Some(record) = records_by_url.get(&novel.toc_url) else {
                    errors.push(format!(
                        "{}: ライブラリにありません",
                        describe(novel)
                    ));
                    continue;
                };
                overwrite_novel(objects.as_ref(), &runtime, record, metadata, novel)
                    .await
                    .map(|_| overwritten += 1)
            }
        };
        if let Err(message) = result {
            errors.push(format!("{}: {message}", describe(novel)));
        }
    }

    let success = errors.is_empty();
    Response::from_json(&import_payload(
        success,
        created,
        overwritten,
        skipped,
        errors,
    ))
}

fn describe(novel: &ParsedNovel) -> String {
    if novel.title.is_empty() {
        novel.toc_url.clone()
    } else {
        format!("{} ({})", novel.title, novel.toc_url)
    }
}

/// Creates a new library entry from one archive novel.
///
/// Files are written first; the record is published only after every object is
/// stored. A failure removes what this run wrote and releases the reserved id,
/// so an interrupted import never leaves a half novel behind.
async fn create_novel(
    runtime: &WorkerRuntime,
    objects: &dyn ObjectStore,
    metadata: &ArchiveNovel,
    novel: &ParsedNovel,
    local: &HashMap<String, serde_yaml::Value>,
    now: DateTime<Utc>,
) -> Result<i64, String> {
    // Authoritative duplicate check: the plan ran against an earlier snapshot.
    match runtime.novels.find_by_toc_url(&novel.toc_url).await {
        Ok(Some(existing)) => {
            return Err(format!("既に ID:{} として登録されています", existing.id));
        }
        Ok(None) => {}
        Err(error) => return Err(format!("重複を確認できません: {error}")),
    }
    if !novel.files.iter().any(|(path, _)| path == TOC_FILE) {
        return Err(format!("アーカイブに {TOC_FILE} がありません"));
    }

    let toc = parse_archive_toc(&novel.files);
    let title = toc
        .as_ref()
        .map(|toc| toc.title.clone())
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| metadata.title.clone());
    if title.trim().is_empty() {
        return Err("アーカイブに作品名がありません".to_string());
    }
    let author = toc
        .as_ref()
        .map(|toc| toc.author.clone())
        .filter(|author| !author.trim().is_empty())
        .unwrap_or_else(|| metadata.author.clone());

    let setting = find_site_setting(runtime, &novel.toc_url);
    // Manifest sitename first, then the site definition, then the URL host —
    // the record needs a non-empty sitename because it names the key prefix.
    let mut sitename = metadata.sitename.trim().to_string();
    if sitename.is_empty()
        && let Some(setting) = setting.as_ref()
    {
        sitename = setting.sitename.clone();
    }
    if sitename.is_empty()
        && let Some(host) = host_of(&novel.toc_url)
    {
        sitename = host;
    }
    if sitename.is_empty() {
        return Err("アーカイブにサイト名がありません".to_string());
    }

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
    let use_subdirectory = download_use_subdirectory(local);
    let novel_type = if metadata.novel_type != 0 {
        metadata.novel_type
    } else {
        toc.as_ref().and_then(|toc| toc.novel_type).unwrap_or(1)
    };
    let general_all_no = toc
        .as_ref()
        .map(|toc| toc.subtitles.len() as i64)
        .filter(|count| *count > 0);

    let keys = NovelObjectKeys::new(&sitename, &file_title, use_subdirectory)
        .map_err(|error| format!("保存先キーを作れません: {error}"))?;

    // The native import refuses an existing directory: it would overwrite a
    // folder that is not the record it is about to create. The Worker
    // equivalent is "the prefix already stores objects".
    if prefix_has_objects(objects, &keys).await? {
        return Err(format!(
            "保存先 {} は既に使われています。管理外の保存先は上書きしません",
            keys.prefix()
        ));
    }

    let mut record = NovelRecord {
        id: 0,
        author,
        title,
        file_title,
        toc_url: novel.toc_url.clone(),
        sitename,
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

    let reservation = runtime
        .novels
        .allocate_id()
        .await
        .map_err(|error| format!("ID を確保できません: {error}"))?;
    record.id = reservation.id().0;
    let id = record.id;

    let written = match write_novel_files(objects, &keys, &novel.files, true).await {
        Ok(written) => written,
        Err(message) => {
            // `write_novel_files` already rolled back what it wrote; only the
            // reservation is left to release.
            let _ = runtime.novels.release_id(reservation).await;
            return Err(message);
        }
    };

    if let Err(error) = runtime
        .novels
        .apply_batch(vec![NovelMutation::Upsert(record)])
        .await
    {
        cleanup_written(objects, written).await;
        let _ = runtime.novels.release_id(reservation).await;
        return Err(format!("レコードを保存できません: {error}"));
    }
    Ok(id)
}

/// Replaces the files of an existing novel, keeping its record id and prefix.
async fn overwrite_novel(
    objects: &dyn ObjectStore,
    runtime: &WorkerRuntime,
    record: &NovelRecord,
    metadata: &ArchiveNovel,
    novel: &ParsedNovel,
) -> Result<(), String> {
    let keys = NovelObjectKeys::new(
        &record.sitename,
        &record.file_title,
        record.use_subdirectory,
    )
    .map_err(|error| format!("保存先キーを作れません: {error}"))?;

    // A record whose stored toc is gone is not an overwrite target: writing
    // the prefix back would resurrect a copy while the rest of the novel is
    // missing.
    match objects.stat(&keys.toc()).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(format!(
                "保存先 {} に {TOC_FILE} がありません。上書きは既存の保存先にだけ行います",
                keys.prefix()
            ));
        }
        Err(error) => return Err(format!("保存先を確認できません: {error}")),
    }

    write_novel_files(objects, &keys, &novel.files, false).await?;

    // Only fill fields the live record does not have. Changing title/author of
    // a managed novel is the downloader's job, and changing sitename/file_title
    // would move the prefix out from under the files just written.
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

    if changed {
        runtime
            .novels
            .apply_batch(vec![NovelMutation::Upsert(updated)])
            .await
            .map_err(|error| format!("レコードを保存できません: {error}"))?;
    }
    Ok(())
}

/// Writes archive files below the novel prefix and returns the keys written.
///
/// `parse_archive` already rejected unsafe paths; the key constructor validates
/// them again because this routine is also reachable with in-memory data.
///
/// `rollback` removes the objects written so far when a later write fails. A
/// *create* rolls back (the novel never existed); an *overwrite* does not,
/// because the objects already written replaced live files — deleting them
/// would destroy data the archive did not remove.
async fn write_novel_files(
    objects: &dyn ObjectStore,
    keys: &NovelObjectKeys,
    files: &[(String, Vec<u8>)],
    rollback: bool,
) -> Result<Vec<ObjectKey>, String> {
    let mut written = Vec::with_capacity(files.len());
    for (relative, bytes) in files {
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(format!(
                "{relative} は {} bytes で、Worker が 1 オブジェクトとして保存できる上限 ({MAX_OBJECT_BYTES} bytes) を超えています",
                bytes.len()
            ));
        }
        let key = object_key_for(keys, relative)?;
        if let Err(error) = objects.write_small(&key, bytes.clone()).await {
            if rollback {
                cleanup_written(objects, written).await;
            }
            return Err(format!("{relative} を保存できません: {error}"));
        }
        written.push(key);
    }
    Ok(written)
}

/// Best-effort removal of the objects this run wrote (create rollback).
async fn cleanup_written(objects: &dyn ObjectStore, keys: Vec<ObjectKey>) {
    for key in keys {
        if let Err(error) = objects.delete(&key).await {
            console_log!("transfer import: 巻き戻しに失敗しました: {key}: {error}");
        }
    }
}

fn object_key_for(keys: &NovelObjectKeys, relative: &str) -> Result<ObjectKey, String> {
    ObjectKey::try_new(format!("{}/{}", keys.prefix().as_ref(), relative))
        .map_err(|error| format!("安全でないファイルパスです {relative:?}: {error}"))
}

/// Whether anything is stored below the novel prefix.
async fn prefix_has_objects(
    objects: &dyn ObjectStore,
    keys: &NovelObjectKeys,
) -> Result<bool, String> {
    let prefix = ObjectPrefix::from(keys.prefix().clone());
    let request = ObjectListRequest::new(
        prefix,
        std::num::NonZeroUsize::new(1).expect("page size is non-zero"),
    );
    let page = objects
        .list_page(&request)
        .await
        .map_err(|error| format!("保存先を確認できません: {error}"))?;
    Ok(!page.objects.is_empty())
}

fn parse_archive_toc(files: &[(String, Vec<u8>)]) -> Option<TocFile> {
    files
        .iter()
        .find(|(path, _)| path == TOC_FILE)
        .and_then(|(_, bytes)| serde_yaml::from_slice::<TocFile>(bytes).ok())
}

fn find_site_setting(runtime: &WorkerRuntime, url: &str) -> Option<SiteSetting> {
    runtime
        .site_settings()
        .iter()
        .find(|setting| setting.matches_url(url))
        .cloned()
}

fn host_of(url: &str) -> Option<String> {
    worker::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

/// Extracts an `n1234ab`-style work id from a table-of-contents URL.
///
/// Same rule as the native import (`(?i)[/?](n\d+[a-z]+)`), without pulling the
/// `regex` crate into the Worker: split on non-alphanumerics and take the first
/// segment that is `n` + digits + letters.
fn extract_ncode(url: &str) -> Option<String> {
    url.split(|ch: char| !ch.is_ascii_alphanumeric())
        .find_map(|segment| {
            let bytes = segment.as_bytes();
            if bytes.len() < 3 || !bytes[0].eq_ignore_ascii_case(&b'n') {
                return None;
            }
            let digits = bytes[1..].iter().take_while(|byte| byte.is_ascii_digit()).count();
            if digits == 0 {
                return None;
            }
            let letters = bytes[1 + digits..]
                .iter()
                .take_while(|byte| byte.is_ascii_alphabetic())
                .count();
            if letters == 0 || 1 + digits + letters != bytes.len() {
                return None;
            }
            Some(segment.to_ascii_lowercase())
        })
}

/// Same rule as the downloader's `compute_file_title` for a new novel.
fn derive_file_title(ncode: Option<&str>, title: &str, append_title: bool) -> String {
    let limit = load_length_limit("folder-length-limit", FOLDER_LENGTH_LIMIT);
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

/// `download.use-subdirectory` (local setting, default `false`).
fn download_use_subdirectory(local: &HashMap<String, serde_yaml::Value>) -> bool {
    local
        .get("download.use-subdirectory")
        .and_then(narou_rs::setting_core::setting_bool_value)
        .unwrap_or(false)
}

/// Whether `path` (relative, `/` separated) can be stored in an archive.
///
/// Mirrors `narou_rs::transfer`'s path rules so one odd object name does not
/// abort an export, and rejects traversal before a key is built.
fn is_archivable_relative_path(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.starts_with('/') {
        return false;
    }
    if path.contains('\\') || path.contains('\0') {
        return false;
    }
    for (index, component) in path.split('/').enumerate() {
        if component.is_empty() || component == "." || component == ".." {
            return false;
        }
        if component.chars().count() > MAX_PATH_COMPONENT_CHARS {
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

fn is_excluded_top_level(relative: &str) -> bool {
    relative
        .split('/')
        .next()
        .is_some_and(|first| EXCLUDED_TOP_LEVEL_DIRS.contains(&first))
}

/// Runtime artifacts that must not travel: `*.lock` files from the inventory
/// writers and `*.tmp` partial writes.
fn is_excluded_file_name(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.ends_with(".lock") || name.ends_with(".tmp")
}

// ---------------------------------------------------------------------------
// request parsing / responses
// ---------------------------------------------------------------------------

/// Parses `ids=1,2,3`. An omitted or empty value is rejected: exporting the
/// whole library by accident is not a useful default for a download endpoint.
fn parse_ids(value: Option<&str>) -> Result<Vec<i64>, String> {
    let Some(value) = value else {
        return Err("ids を指定してください (例: /api/transfer/export?ids=1,2)".to_string());
    };
    let mut ids = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let id = part
            .parse::<i64>()
            .map_err(|_| format!("不正な ID です: {part:?}"))?;
        if id < 0 {
            return Err(format!("不正な ID です: {part:?}"));
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err("ids を指定してください (例: /api/transfer/export?ids=1,2)".to_string());
    }
    Ok(ids)
}

fn flag_value(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim),
        Some("1" | "true" | "TRUE" | "True" | "yes" | "on")
    )
}

/// Accepted upload media types (native parity).
fn is_zip_content_type(req: &Request) -> bool {
    let Ok(Some(value)) = req.headers().get("content-type") else {
        return false;
    };
    let value = value.split(';').next().unwrap_or("").trim();
    value.eq_ignore_ascii_case("application/zip")
        || value.eq_ignore_ascii_case("application/x-zip-compressed")
        || value.eq_ignore_ascii_case("application/octet-stream")
}

fn import_payload(
    success: bool,
    created: usize,
    overwritten: usize,
    skipped: usize,
    errors: Vec<String>,
) -> serde_json::Value {
    serde_json::json!({
        "success": success,
        "created": created,
        "overwritten": overwritten,
        "skipped": skipped,
        "errors": errors,
    })
}

fn import_error(status: u16, message: String) -> worker::Result<Response> {
    let payload = import_payload(false, 0, 0, 0, vec![message]);
    Ok(Response::from_json(&payload)?.with_status(status))
}

fn export_error(status: u16, message: String) -> worker::Result<Response> {
    let payload = serde_json::json!({
        "success": false,
        "message": message,
    });
    Ok(Response::from_json(&payload)?.with_status(status))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

// The crate is `#![cfg(target_arch = "wasm32")]`, so a host `cargo test
// -p narou_worker` builds an empty crate and a wasm test binary needs a wasm
// runner. The pure helpers are exercised by `worker_entry/tests/transfer.mjs`
// against a real `workerd` instead (see that file's header).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ncode_follows_the_native_rule() {
        assert_eq!(
            extract_ncode("https://ncode.syosetu.com/n1234ab/").as_deref(),
            Some("n1234ab")
        );
        assert_eq!(
            extract_ncode("https://novel18.syosetu.com/N1234AB/").as_deref(),
            Some("n1234ab")
        );
        assert_eq!(
            extract_ncode("https://example.com/?ncode=n9999zz").as_deref(),
            Some("n9999zz")
        );
        assert_eq!(extract_ncode("https://example.com/1234ab/"), None);
        assert_eq!(extract_ncode("https://example.com/n1234/"), None);
        assert_eq!(extract_ncode("https://example.com/n1234abx1/"), None);
    }

    #[test]
    fn parse_ids_rejects_empty_and_negative_values() {
        assert_eq!(parse_ids(Some("1,2, 3")).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_ids(Some("2,2")).unwrap(), vec![2]);
        assert!(parse_ids(None).is_err());
        assert!(parse_ids(Some("")).is_err());
        assert!(parse_ids(Some("1,x")).is_err());
        assert!(parse_ids(Some("-1")).is_err());
    }

    #[test]
    fn runtime_artifacts_are_not_archivable() {
        assert!(is_excluded_file_name("toc.yaml.lock"));
        assert!(is_excluded_file_name("本文/.1 一話.yaml.tmp"));
        assert!(!is_excluded_file_name("toc.yaml"));
        assert!(is_excluded_top_level("backup/novel.zip"));
        assert!(!is_excluded_top_level("本文/1 一話.yaml"));
    }

    #[test]
    fn unsafe_object_names_are_skipped() {
        for path in [
            "../escape.txt",
            "/absolute.txt",
            "a\\b.txt",
            "bad\0name.txt",
            "CON",
            "COM1.log",
            "dir//empty.txt",
            "trailing.",
        ] {
            assert!(!is_archivable_relative_path(path), "{path:?}");
        }
        for path in ["toc.yaml", "本文/1 一話.yaml", "挿絵/001.png"] {
            assert!(is_archivable_relative_path(path), "{path:?}");
        }
    }

    #[test]
    fn file_title_follows_the_downloader_rule() {
        assert_eq!(derive_file_title(Some("n1234ab"), "作品名", false), "n1234ab");
        assert_eq!(
            derive_file_title(Some("n1234ab"), "作品名", true),
            "n1234ab 作品名"
        );
        assert_eq!(derive_file_title(None, "作品/名", false), "作品_名");
    }
}
