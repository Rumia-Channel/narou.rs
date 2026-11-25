//! 起動時の一括バックアップ提案と、ライブラリ全体バックアップの実装。
//!
//! 0.4.0 未満から 0.4.0 以上へアップデートした後の初回起動時に、
//! 小説データ (`小説データ/`) と管理データ (`.narou/`) をまとめた zip を
//! narou_rs 実行ファイルと同じフォルダの `backup/` へ作成するか確認する。
//! Web UI では `GET /api/library_backup` が同じ判定を返し、モーダルで提案する。
//!
//! 前回起動バージョンは `.narou/last-run-version` に記録する。
//! プロンプトを出した・出さなかったに関わらず、判定を行った起動では
//! 常に現在バージョンへ更新する。
//! 実際の zip 作成は `narou_rs_backup` 実行ファイルとしても独立しており、
//! Web UI からはサブプロセスとして起動される。
//!
//! ログイン資格情報の鍵 `.narou/login.key` と Web UI のローカル認証トークン
//! `.narou/ui_token` はバックアップに含めない。在庫 (`.narou/login_cookie.yaml`
//! / SQLite) には鍵で暗号化した Cookie が入るため、鍵と暗号文を同じ zip に
//! 載せるとバックアップが平文の資格情報と同じ強さになってしまう。UI トークン
//! も、別環境でそのまま Web UI に入れてしまわないよう持ち出さない。
//!
//! Canonical YAML/SQLite metadata and retained namespace provenance are captured
//! under the publication authority. Novel bytes use original reader admissions;
//! remote immutable references are verified and rehosted as local canonical
//! files. Restore needs neither S3 staging journals nor live lease/OS handles.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;

use crate::error::{NarouError, Result};
use crate::version;

/// このバージョンを境にバックアップ提案を出す。
/// prev < BOUNDARY <= current のときだけ提案する。
const BACKUP_PROMPT_BOUNDARY: &str = "0.4.0";

/// 前回起動バージョンを記録する `.narou` 内のファイル名。
const LAST_RUN_VERSION_FILE: &str = "last-run-version";

/// バックアップ zip の出力先 (exe と同じフォルダの下)。
const BACKUP_DIR_NAME: &str = "backup";

/// バックアップ専用実行ファイル名 (拡張子なし)。
pub const BACKUP_EXE_STEM: &str = "narou_rs_backup";

/// Machine-readable completion line emitted only after the ZIP is closed.
pub const BACKUP_RESULT_PREFIX: &str = "NAROU_BACKUP_RESULT ";

/// プロンプトを出さないコマンド。
/// `web` はサーバー起動をブロックしないため除外 (Web UI 側で提案する)。
/// `help`/`version` は情報表示のみなので除外。
const SKIP_COMMANDS: &[&str] = &["web", "help", "version"];

/// バックアップ提案に必要な容量情報。
#[derive(Debug, Clone, Copy)]
pub struct PendingOffer {
    /// バックアップ対象の非圧縮合計サイズ。
    pub total_bytes: u64,
    /// 保存先ドライブの空き容量。取得不能なら None。
    pub free_bytes: Option<u64>,
    /// 空き容量が合計サイズ以上 (または不明) なら true。
    pub enough_space: bool,
}

/// コマンド実行前に呼ばれる入口。失敗しても本体の動作を妨げない。
pub fn maybe_offer(command: &str) {
    if let Err(err) = offer(command) {
        eprintln!("[WARN] 起動時バックアップ処理でエラー: {}", err);
    }
}

fn offer(command: &str) -> Result<()> {
    if SKIP_COMMANDS.contains(&command) {
        return Ok(());
    }
    if std::env::var_os("NAROU_ENV").is_some_and(|v| v == "test") {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Ok(());
    }
    let Some(root) = crate::logger::find_narou_root() else {
        return Ok(());
    };

    if let Some(pending) = pending_offer(&root)? {
        prompt_and_backup(&root, &pending);
    }

    // 判定を行った起動では常に現在バージョンを記録する。
    // バックアップ失敗時も記録して再プロンプトの嵐を防ぐ。
    record_run(&root);
    Ok(())
}

/// バックアップ提案が必要なら容量情報を返す。
/// CLI と Web API で共通の判定。提案不要・データなしなら None。
/// Capture errors are not evidence that a backup was already completed.
pub fn pending_offer(root: &Path) -> Result<Option<PendingOffer>> {
    if !backup_offer_needed(root)? { return Ok(None); }
    let total_bytes = backup_size(root)?;
    let free_bytes = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .and_then(|dir| fs2::available_space(dir).ok());
    Ok(Some(PendingOffer {
        total_bytes,
        free_bytes,
        enough_space: free_bytes.is_none_or(|bytes| bytes >= total_bytes),
    }))
}

/// Metadata-only eligibility check; creation performs its own single capture.
pub fn backup_offer_needed(root: &Path) -> Result<bool> {
    if !should_offer(read_last_run_version(root).as_deref(), version::VERSION) { return Ok(false); }
    has_library_data(root)
}

/// 前回起動バージョンを `.narou/last-run-version` へ記録する。
/// 提案への回答 (作成/スキップ) があった時点で呼ぶ。
pub fn record_run(root: &Path) {
    let marker_path = root.join(".narou").join(LAST_RUN_VERSION_FILE);
    let _ = fs::write(marker_path, format!("{}\n", version::VERSION));
}

fn read_last_run_version(root: &Path) -> Option<String> {
    fs::read_to_string(root.join(".narou").join(LAST_RUN_VERSION_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
/// prev < BOUNDARY <= current のとき true。
/// prev が読めない/パース不能なら旧バージョン扱いで提案する。
fn should_offer(previous: Option<&str>, current: &str) -> bool {
    let current_ok = matches!(
        version::version_compare(&version::version_core(current), BACKUP_PROMPT_BOUNDARY),
        Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
    );
    if !current_ok {
        return false;
    }
    match previous {
        Some(prev) => matches!(
            version::version_compare(&version::version_core(prev), BACKUP_PROMPT_BOUNDARY),
            Some(std::cmp::Ordering::Less) | None
        ),
        None => true,
    }
}

/// Include database-only novels when `sqlite.mirror-files=false` creates no
/// novel directories, while still suppressing offers for an empty library.
fn has_library_data(root: &Path) -> Result<bool> {
    match fs::read_dir(root.join(crate::downloader::ARCHIVE_ROOT_DIR)) {
        Ok(mut entries) => if let Some(entry) = entries.next() { entry?; return Ok(true); },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
    }
    if crate::db::DATABASE.lock().is_none() { crate::db::init_database()?; }
    let active = crate::db::with_database(|database| Ok(database.inventory().root_dir().canonicalize()?))?;
    if active != root.canonicalize()? { return Err(NarouError::Platform("backup root differs from the active library".into())); }
    Ok(!read_backup_metadata(None)?.records.is_empty())
}


fn prompt_and_backup(root: &Path, pending: &PendingOffer) {
    let default_dir = default_backup_dir();

    println!();
    println!("0.4.0 では管理データの保存形式に破壊的な変更が加わっています。データ消失に備え、小説データのバックアップを推奨します。");
    println!(
        "対象: ライブラリフォルダ全体 ({}、.narou、webnovel など、計 {})",
        crate::downloader::ARCHIVE_ROOT_DIR,
        format_size(pending.total_bytes)
    );
    match pending.free_bytes {
        Some(bytes) => println!("保存先ドライブの空き容量: {}", format_size(bytes)),
        None => println!("保存先ドライブの空き容量: 不明"),
    }
    if !pending.enough_space {
        println!("[WARN] 空き容量が不足している可能性があります。");
    }

    if !crate::compat::confirm("バックアップを作成しますか", pending.enough_space, false) {
        return;
    }

    // 保存先の指定を受け付ける。未入力なら exe 横の backup/。
    let dest_dir = match default_dir.as_ref() {
        Some(dir) => match ask_backup_destination(dir) {
            Ok(dir) => dir,
            Err(err) => {
                eprintln!("[WARN] 保存先の確認に失敗しました: {}", err);
                return;
            }
        },
        None => {
            eprintln!("[WARN] 実行ファイルの場所を特定できないためバックアップを作成できません");
            return;
        }
    };

    if let Err(err) = check_free_space(&dest_dir, pending.total_bytes) {
        eprintln!("[WARN] {}", err);
        return;
    }

    match create_library_backup(root, &dest_dir) {
        Ok(path) => println!("バックアップを作成しました: {}", path.display()),
        Err(err) => eprintln!("[WARN] バックアップの作成に失敗しました: {}", err),
    }
}

/// 保存先フォルダを対話で尋ねる。未入力は default_dir。
fn ask_backup_destination(default_dir: &Path) -> Result<PathBuf> {
    println!("保存先フォルダ (未入力で {}):", default_dir.display());
    print!(">");
    io::stdout().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Ok(default_dir.to_path_buf());
    }
    let input = input.trim();
    if input.is_empty() {
        return Ok(default_dir.to_path_buf());
    }
    Ok(PathBuf::from(input))
}

/// 既定のバックアップ出力先: narou_rs と同じフォルダの `backup/`。
pub fn default_backup_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .map(|dir| dir.join(BACKUP_DIR_NAME))
}

/// 保存先ドライブの空き容量が合計サイズに足りるか確認する。
/// 空き容量を取得できない場合は Ok (不明扱い)。
pub fn check_free_space(dest_dir: &Path, total_bytes: u64) -> Result<()> {
    // 存在しないパスでも最も近い既存の親で statvfs 相当を取るため、
    // 親を辿って既存ディレクトリを探す。
    let mut probe = dest_dir;
    let mut free = None;
    loop {
        if probe.is_dir() {
            free = fs2::available_space(probe).ok();
            break;
        }
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    match free {
        Some(bytes) if bytes < total_bytes => Err(NarouError::Conversion(format!(
            "保存先ドライブの空き容量が不足しています (必要: 約{}, 空き: {})",
            format_size(total_bytes),
            format_size(bytes)
        ))),
        _ => Ok(()),
    }
}

/// `narou_rs_backup` 実行ファイルのパス (本体と同じフォルダ)。
pub fn backup_exe_path() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))?;
    let name = if cfg!(windows) {
        format!("{}.exe", BACKUP_EXE_STEM)
    } else {
        BACKUP_EXE_STEM.to_string()
    };
    let path = exe_dir.join(name);
    path.is_file().then_some(path)
}

/// ライブラリルート全体 (`小説データ/`、`.narou/`、`webnovel/`、その他の
/// ユーザー作成ファイルを含む) を `dest_dir` に zip 化する。
/// `dest_dir` は zip を置くフォルダ (なければ作成する)。
/// `dest_dir` がライブラリ内にある場合はバックアップ対象から除外する。
/// ルート・管理データ内・作品全体を除外する保存先は、欠落した backup を作らず拒否する。
/// 失敗時は中途半端な zip を残さない。
pub fn create_library_backup(root: &Path, dest_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dest_dir)?;
    let exclude_dir = dest_dir.canonicalize()?;
    let snapshot = capture_library_backup(root, Some(&exclude_dir))?;
    check_free_space(dest_dir, snapshot_size(snapshot.path())?)?;
    let backup_path = dest_dir.join(format!(
        "narou-backup-{}-{}.zip",
        chrono::Local::now().format("%Y%m%d%H%M%S"),
        uuid::Uuid::new_v4().simple(),
    ));
    let mut temporary = tempfile::NamedTempFile::new_in(dest_dir)?;
    {
        let mut zip = zip::ZipWriter::new(temporary.as_file_mut());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated).large_file(true);
        add_directory_to_zip(&mut zip, snapshot.path(), snapshot.path(), None, options)?;
        zip.finish().map_err(|error| NarouError::Conversion(error.to_string()))?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(&backup_path).map_err(|error| NarouError::Io(error.error))?;
    Ok(backup_path)
}

/// バックアップ対象ファイルの合計サイズ (非圧縮)。
/// `exclude_dir` (保存先) がライブラリ内にある場合は除外して数える。
pub fn backup_size(root: &Path) -> Result<u64> {
    backup_size_excluding(root, None)
}

/// `backup_size` の除外指定あり版。
pub fn backup_size_excluding(root: &Path, exclude_dir: Option<&Path>) -> Result<u64> {
    let snapshot = capture_library_backup(root, exclude_dir)?;
    snapshot_size(snapshot.path())
}

fn snapshot_size(root: &Path) -> Result<u64> {
    let mut files = Vec::new();
    collect_backup_files(root, root, None, &mut files)?;
    files.into_iter().try_fold(0u64, |total, path| total.checked_add(fs::metadata(path)?.len())
        .ok_or_else(|| NarouError::Platform("backup size overflow".into())))
}

#[derive(Debug, PartialEq)]
struct BackupMetadata {
    records: std::collections::BTreeMap<i64, crate::db::NovelRecord>,
    /// Registered identities and retained, unregistered namespace provenance.
    namespaces: std::collections::BTreeMap<String, String>,
    references: Vec<BackupObjectReference>,
    yaml_metadata: bool,
    digest: [u8; 32],
}

#[derive(Debug, PartialEq)]
struct BackupObjectReference {
    key: crate::platform::ObjectKey,
    size: u64,
    sha256: String,
}

#[derive(serde::Deserialize)]
struct BackupRemoteReference {
    physical: String,
    size: u64,
    sha256: String,
    logical: String,
    prefix: String,
    #[serde(default)]
    etag: String,
}

/// All metadata is captured under the existing common authority. Original
/// admissions cover registered AND retained canonical namespaces until every
/// body and reference has been verified; a concurrent publication fails closed.
fn capture_library_backup(root: &Path, exclude_dir: Option<&Path>) -> Result<tempfile::TempDir> {
    if crate::db::DATABASE.lock().is_none() { crate::db::init_database()?; }
    crate::native::transfer::recover_startup()?;
    crate::native::application::recover_removals()?;
    let root = root.canonicalize()?;
    let active = crate::db::with_database(|database| Ok(database.inventory().root_dir().canonicalize()?))?;
    if active != root { return Err(NarouError::Platform("backup root differs from the active library".into())); }
    let stores = crate::native::object_store::NativeStores::for_narou_root(&root)?;
    capture_library_backup_with_stores(&root, exclude_dir, stores, || Ok(()))
}

fn capture_library_backup_with_stores(
    root: &Path,
    exclude_dir: Option<&Path>,
    stores: crate::native::object_store::NativeStores,
    after_metadata: impl FnOnce() -> Result<()> + Send,
) -> Result<tempfile::TempDir> {
    use crate::platform::{GateState, ObjectStore, ObjectPrefix};
    let stage = tempfile::Builder::new().prefix("narou-library-backup-").tempdir()?;
    let objects = crate::native::materialization::read_store_with(&stores);
    // Store backends resolve every canonical path through
    // `ensure_within_archive_root` (canonical spelling without the `\\?\`
    // verbatim marker). Normalize the library root and the exclusion to the
    // same spelling or `strip_prefix`/`starts_with` always fail on Windows.
    let root = &crate::db::paths::ensure_within_archive_root(root, root)?;
    let exclude_dir = exclude_dir
        .map(|path| crate::db::paths::ensure_within_archive_root(path, path))
        .transpose()?
        .filter(|path| path.starts_with(root));
    if exclude_dir.as_ref().is_some_and(|path| path == root || path.starts_with(root.join(".narou")) || path.starts_with(root.join(".narousetting"))) {
        return Err(NarouError::Platform("backup destination cannot exclude the library root or its authoritative metadata".into()));
    }
    let remote_objects = stores.s3.as_ref().map(|remote|
        crate::native::object_store::NativeStores::remote_objects(stores.primary.clone(), remote.clone()));
    let initial = read_backup_metadata(None)?;
    crate::native::materialization::block_on(async {
        let mut scope = crate::application::novel_access::NovelReadScope::new(&objects, "library-backup");
        let result = async {
            let fence = objects.access_fence().ok_or_else(|| NarouError::Platform("backup reader fence unavailable".into()))?;
            let mut sources = Vec::new();
            for prefix in initial.namespaces.keys() {
                let prefix = ObjectPrefix::new(prefix)?;
                let mut lease = scope.lease_for(&prefix).await?;
                lease.validate().await?;
                let info = fence.gate_info(&prefix).await?;
                if info.state != GateState::Open || info.epoch != lease.epoch() {
                    return Err(NarouError::Platform("novel_recovering: backup source changed".into()));
                }
                sources.push((stores.primary.novel_directory(&prefix)?, prefix, info.content_revision));
            }
            let mut directories = sources.iter().map(|(path, _, _)| if path.exists() { crate::db::paths::ensure_within_archive_root(path, path) } else { Ok(path.clone()) }).collect::<Result<Vec<_>>>()?;
            directories.sort();
            if directories.windows(2).any(|pair| pair[1].starts_with(&pair[0])) {
                return Err(NarouError::Platform("ambiguous overlapping physical backup directories".into()));
            }
            if exclude_dir.as_ref().is_some_and(|excluded| directories.iter().any(|path| path.starts_with(excluded))) {
                return Err(NarouError::Platform("backup destination cannot exclude a canonical novel directory".into()));
            }
            let metadata = read_backup_metadata(Some(stage.path()))?;
            if metadata.records != initial.records || metadata.namespaces != initial.namespaces {
                return Err(NarouError::Platform("novel_busy: backup identities changed before capture".into()));
            }
            after_metadata()?;
            let mut excluded = sources.iter().map(|(path, _, _)| path.clone()).collect::<Vec<_>>();
            excluded.push(root.join(".narou"));
            excluded.push(root.join(".narousetting/global_setting.yaml"));
            excluded.push(root.join(crate::downloader::ARCHIVE_ROOT_DIR).join(".objects/.narou-transfer"));
            if let Some(path) = &exclude_dir { excluded.push(path.clone()); }
            copy_backup_tree(root, root, stage.path(), &excluded)?;
            for (directory, prefix, _) in &sources {
                scope.refresh_all().await?;
                let mut lease = scope.lease_for(prefix).await?;
                let excluded_relative = exclude_dir.as_ref().and_then(|path| path.strip_prefix(directory).ok());
                let snapshot = crate::native::materialization::snapshot_leased_excluding(&objects, &mut lease, root, excluded_relative).await?;
                let relative = directory.strip_prefix(root).map_err(|error| NarouError::Platform(error.to_string()))?;
                let target = stage.path().join(relative);
                fs::create_dir_all(&target)?;
                copy_backup_tree(snapshot.path(), snapshot.path(), &target, &[])?;
            }
            // A root-level remote LIST includes legacy user objects that are not
            // image-shaped. Never guess an unregistered prefix from path depth.
            if let Some(remote) = &remote_objects {
                let mut request = backup_list_request("novels")?;
                loop {
                    scope.refresh_all().await?;
                    let page = remote.list_page(&request).await?;
                    for object in page.objects {
                        let prefix = backup_namespace_for_key(&metadata, &object.key)?;
                        let destination = backup_key_destination(root, stage.path(), stores.primary.as_ref(), &object.key)?;
                        if exclude_dir.as_ref().is_some_and(|excluded| stores.primary.path_for_canonical_key(&object.key).is_ok_and(|path| path.starts_with(excluded))) { continue; }
                        if !destination.is_file() {
                            let mut lease = scope.lease_for(&prefix).await?;
                            if !crate::native::materialization::copy_asset(remote, &mut lease, &object.key, &destination, None).await? {
                                return Err(NarouError::NotFound(object.key.to_string()));
                            }
                            if fs::metadata(&destination)?.len() != object.size {
                                return Err(NarouError::Platform("remote backup object length changed".into()));
                            }
                        }
                    }
                    match page.next_cursor {
                        Some(cursor) => { require_backup_cursor(&request, &cursor)?; request = request.after(cursor); }
                        None => break,
                    }
                }
            }
            let mut remote_refs = Vec::new();
            let mut request = backup_list_request(".narou-transfer/.refs")?;
            loop {
                let page = stores.primary.list_page(&request).await?;
                for object in page.objects {
                    if object.size > 64 * 1024 { return Err(NarouError::Platform("remote backup reference exceeds metadata admission".into())); }
                    let bytes = stores.primary.read_small(&object.key).await?.ok_or_else(|| NarouError::NotFound(object.key.to_string()))?;
                    let reference: BackupRemoteReference = serde_json::from_slice(&bytes).map_err(|error| NarouError::Platform(format!("invalid remote backup reference: {error}")))?;
                    let key = crate::platform::ObjectKey::try_new(&reference.logical)?;
                    let prefix = backup_namespace_for_key(&metadata, &key)?;
                    if object.key.as_ref().strip_prefix(".narou-transfer/.refs/") != Some(key.as_ref())
                        || reference.prefix.trim_end_matches('/') != prefix.as_ref().trim_end_matches('/') {
                        return Err(NarouError::Platform("remote backup reference has conflicting provenance".into()));
                    }
                    let destination = backup_key_destination(root, stage.path(), stores.primary.as_ref(), &key)?;
                    if exclude_dir.as_ref().is_some_and(|excluded| stores.primary.path_for_canonical_key(&key).is_ok_and(|path| path.starts_with(excluded))) {
                        return Err(NarouError::Platform("backup destination overlaps a canonical remote reference".into()));
                    }
                    if reference.physical.is_empty() {
                        if destination.is_file() { fs::remove_file(&destination)?; }
                    } else {
                        if reference.etag.is_empty() || !valid_backup_sha(&reference.sha256) {
                            return Err(NarouError::Platform("remote backup reference has no immutable fingerprint".into()));
                        }
                        crate::platform::ObjectKey::try_new(&reference.physical)?;
                        let remote = remote_objects.as_ref().ok_or_else(|| NarouError::Platform("canonical remote references require configured S3 storage for backup".into()))?;
                        let mut lease = scope.lease_for(&prefix).await?;
                        // Explicitly open the immutable alias even if a local
                        // mirror exists; stale mirrors are not rehosting proof.
                        if !crate::native::materialization::copy_asset(remote, &mut lease, &key, &destination, None).await? {
                            return Err(NarouError::NotFound(key.to_string()));
                        }
                        verify_backup_reference(&destination, reference.size, &reference.sha256)?;
                    }
                    remote_refs.push(key);
                }
                match page.next_cursor {
                    Some(cursor) => { require_backup_cursor(&request, &cursor)?; request = request.after(cursor); }
                    None => break,
                }
            }
            for reference in &metadata.references {
                if !reference.key.as_ref().starts_with("novels/") { continue; }
                let destination = backup_key_destination(root, stage.path(), stores.primary.as_ref(), &reference.key)?;
                verify_backup_reference(&destination, reference.size, &reference.sha256)?;
            }
            for (_, prefix, revision) in &sources {
                let mut lease = scope.lease_for(prefix).await?;
                lease.validate().await?;
                let info = fence.gate_info(prefix).await?;
                if info.state != GateState::Open || info.epoch != lease.epoch() || info.content_revision != *revision {
                    return Err(NarouError::Platform("novel_busy: backup source content changed".into()));
                }
            }
            if read_backup_metadata(None)? != metadata {
                return Err(NarouError::Platform("novel_busy: backup metadata changed during capture".into()));
            }
            sanitize_backup_image(stage.path(), &remote_refs, metadata.yaml_metadata)?;
            Ok(())
        }.await;
        let released = scope.release_all().await;
        result.and(released)
    })?;
    Ok(stage)
}

fn backup_list_request(prefix: &str) -> Result<crate::platform::ObjectListRequest> {
    Ok(crate::platform::ObjectListRequest::new(crate::platform::ObjectPrefix::new(prefix)?, std::num::NonZeroUsize::new(100).expect("nonzero backup page")))
}

fn require_backup_cursor(request: &crate::platform::ObjectListRequest, cursor: &str) -> Result<()> {
    if request.cursor.as_deref() == Some(cursor) {
        return Err(NarouError::Platform("backup listing cursor did not advance".into()));
    }
    Ok(())
}

fn backup_namespace_for_key(metadata: &BackupMetadata, key: &crate::platform::ObjectKey) -> Result<crate::platform::ObjectPrefix> {
    let prefix = metadata.namespaces.keys().find(|prefix| key.as_ref() == prefix.as_str()
        || key.as_ref().strip_prefix(prefix.as_str()).is_some_and(|tail| tail.starts_with('/')))
        .ok_or_else(|| NarouError::Platform(format!("canonical backup object has no registered or retained origin: {key}")))?;
    crate::platform::ObjectPrefix::new(prefix)
}

fn backup_key_destination(root: &Path, target: &Path, store: &crate::native::object_store::NativeStore, key: &crate::platform::ObjectKey) -> Result<PathBuf> {
    let path = store.path_for_canonical_key(key)?;
    Ok(target.join(path.strip_prefix(root).map_err(|error| NarouError::Platform(error.to_string()))?))
}

fn valid_backup_sha(sha: &str) -> bool { sha.len() == 64 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()) }

fn verify_backup_reference(path: &Path, size: u64, sha: &str) -> Result<()> {
    if !valid_backup_sha(sha) || fs::metadata(path)?.len() != size || !hex::encode(backup_file_digest(path)?).eq_ignore_ascii_case(sha) {
        return Err(NarouError::Platform(format!("backup canonical reference is missing or corrupt: {}", path.display())));
    }
    Ok(())
}

fn read_backup_metadata(target: Option<&Path>) -> Result<BackupMetadata> {
    crate::db::with_database(|database| database.with_library_metadata_snapshot(|inventory, primary| {
        let yaml = database.metadata_is_yaml_in_snapshot(primary);
        crate::native::transfer::with_control_metadata_snapshot(inventory.root_dir(), primary, |control| {
            let metadata = backup_metadata_snapshot(inventory, if yaml { None } else { primary }, control)?;
            if let Some(target) = target { capture_backup_metadata(inventory, target, primary, control)?; }
            Ok(metadata)
        })
    }))
}

fn backup_metadata_snapshot(inventory: &crate::db::inventory::Inventory, records_connection: Option<&rusqlite::Connection>, control: &rusqlite::Connection) -> Result<BackupMetadata> {
    let records = match records_connection {
        Some(connection) => crate::native::sqlite::bulk::load_all_records(connection)?,
        None => {
            let raw = crate::db::inventory::read_optional_yaml_file(&inventory.inventory_path("database", crate::db::inventory::InventoryScope::Local))?;
            let mut records: std::collections::BTreeMap<i64, crate::db::NovelRecord> = if raw.trim().is_empty() { Default::default() } else { serde_yaml::from_str(&raw)? };
            for (id, record) in &mut records { record.id = *id; }
            records
        }
    };
    crate::native::transfer::ensure_no_unpublished_creations_conn(control, |id| records.contains_key(&id))?;
    let mut namespaces = std::collections::BTreeMap::new();
    for record in records.values() {
        let prefix = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?.prefix().to_string();
        if namespaces.insert(prefix, crate::db::novel_codec::fold(&record.toc_url)).is_some() {
            return Err(NarouError::Platform("ambiguous backup source namespace".into()));
        }
    }
    crate::native::object_store::visit_namespace_gates(inventory.root_dir(), control, |prefix, closed, owner, _, origin| {
        if closed || owner.is_some() { return Err(NarouError::Platform("novel_recovering: finish publications before library backup".into())); }
        if origin.is_empty() { return Ok(()); }
        let prefix = crate::platform::ObjectPrefix::new(prefix)?.as_ref().trim_end_matches('/').to_owned();
        if !prefix.starts_with("novels/") { return Err(NarouError::Platform("retained backup namespace is not a novel".into())); }
        if namespaces.get(&prefix).is_some_and(|current| current != origin) {
            return Err(NarouError::Platform("backup namespace origin conflicts with current identity".into()));
        }
        namespaces.insert(prefix, origin.to_owned());
        Ok(())
    })?;
    let mut prefixes = namespaces.keys().collect::<Vec<_>>();
    prefixes.sort_unstable();
    for pair in prefixes.windows(2) {
        if crate::platform::access_fence::namespace_prefixes_overlap(pair[0], pair[1]) {
            return Err(NarouError::Platform("ambiguous overlapping backup source namespaces".into()));
        }
    }
    let mut metadata = BackupMetadata { records, namespaces, references: Vec::new(), yaml_metadata: records_connection.is_none(), digest: backup_metadata_digest(inventory, control)? };
    if backup_table_exists(control, "transfer_object_refs")? {
        let mut statement = control.prepare("SELECT r.object_key,r.size,r.sha256,r.backend,r.blob_generation,o.generation FROM transfer_object_refs r LEFT JOIN objects o ON o.object_key=r.blob_key ORDER BY r.object_key")
            .map_err(crate::native::sqlite::sqlite_error)?;
        let mut rows = statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;
        while let Some(row) = rows.next().map_err(crate::native::sqlite::sqlite_error)? {
            let key = crate::platform::ObjectKey::try_new(row.get::<_, String>(0).map_err(crate::native::sqlite::sqlite_error)?)?;
            let size = u64::try_from(row.get::<_, i64>(1).map_err(crate::native::sqlite::sqlite_error)?)
                .map_err(|_| NarouError::Platform("backup canonical reference size is negative".into()))?;
            let sha256: String = row.get(2).map_err(crate::native::sqlite::sqlite_error)?;
            let backend: String = row.get(3).map_err(crate::native::sqlite::sqlite_error)?;
            let expected: String = row.get(4).map_err(crate::native::sqlite::sqlite_error)?;
            let actual: Option<String> = row.get(5).map_err(crate::native::sqlite::sqlite_error)?;
            if backend != "d1" || expected.is_empty() || actual.as_deref() != Some(expected.as_str()) || !valid_backup_sha(&sha256) {
                return Err(NarouError::Platform(format!("backup canonical reference proof missing or changed: {key}")));
            }
            if key.as_ref().starts_with("novels/") { backup_namespace_for_key(&metadata, &key)?; }
            metadata.references.push(BackupObjectReference { key, size, sha256 });
        }
        let mut statement = control.prepare("SELECT object_key FROM native_logical_objects WHERE object_key>='novels/' AND object_key<'novels0' ORDER BY object_key")
            .map_err(crate::native::sqlite::sqlite_error)?;
        let mut rows = statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;
        while let Some(row) = rows.next().map_err(crate::native::sqlite::sqlite_error)? {
            backup_namespace_for_key(&metadata, &crate::platform::ObjectKey::try_new(row.get::<_, String>(0).map_err(crate::native::sqlite::sqlite_error)?)?)?;
        }
    }
    Ok(metadata)
}

fn capture_backup_metadata(inventory: &crate::db::inventory::Inventory, target: &Path, primary: Option<&rusqlite::Connection>, control: &rusqlite::Connection) -> Result<()> {
    let root = inventory.root_dir();
    let metadata = target.join(".narou");
    fs::create_dir_all(&metadata)?;
    copy_backup_tree(root, &root.join(".narou"), target, &[root.join(".narou/db.sqlite"), root.join(".narou/transfer/control.sqlite")])?;
    let image = if primary.is_some() { metadata.join("db.sqlite") } else { metadata.join("transfer/control.sqlite") };
    fs::create_dir_all(image.parent().ok_or_else(|| NarouError::Platform("backup database image has no parent".into()))?)?;
    control.execute("VACUUM INTO ?1", [image.to_string_lossy().as_ref()]).map_err(crate::native::sqlite::sqlite_error)?;
    // File-only global settings may live outside the library. Rehome them to
    // the supported library-local directory, never consult an Inventory cache.
    let global = inventory.inventory_path("global_setting", crate::db::inventory::InventoryScope::Global);
    if global.is_file() {
        let directory = target.join(".narousetting");
        fs::create_dir_all(&directory)?;
        copy_backup_file(&global, &directory.join("global_setting.yaml"))?;
    }
    let gates = root.join(crate::downloader::ARCHIVE_ROOT_DIR).join(".objects/.narou-transfer/.gate");
    if gates.is_dir() {
        for entry in fs::read_dir(&gates)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() { return Err(NarouError::Platform("invalid durable gate backup entry".into())); }
            let gate = crate::native::object_store::sanitize_gate_snapshot(&fs::read(entry.path())?)?;
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|error| NarouError::Platform(error.to_string()))?;
            let destination = target.join(relative);
            fs::create_dir_all(destination.parent().ok_or_else(|| NarouError::Platform("gate destination has no parent".into()))?)?;
            fs::write(destination, gate)?;
        }
    }
    Ok(())
}

fn backup_table_exists(connection: &rusqlite::Connection, name: &str) -> Result<bool> {
    connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |row| row.get(0)).map_err(crate::native::sqlite::sqlite_error)
}

fn backup_metadata_digest(inventory: &crate::db::inventory::Inventory, control: &rusqlite::Connection) -> Result<[u8; 32]> {
    use sha2::Digest;
    let root = inventory.root_dir();
    let mut hash = sha2::Sha256::new();
    let mut files = Vec::new();
    collect_backup_files(root, &root.join(".narou"), None, &mut files)?;
    files.sort();
    for path in files {
        if path == root.join(".narou/db.sqlite") || path == root.join(".narou/transfer/control.sqlite") || path == root.join(".narou/database.yaml") { continue; }
        hash.update(path.strip_prefix(root).map_err(|error| NarouError::Platform(error.to_string()))?.to_string_lossy().as_bytes());
        hash.update(backup_file_digest(&path)?);
    }
    let global = inventory.inventory_path("global_setting", crate::db::inventory::InventoryScope::Global);
    hash.update([u8::from(global.is_file())]);
    if global.is_file() { hash.update(backup_file_digest(&global)?); }
    // No payload columns, leases, session journals or renewal timestamps.
    for table in ["frozen_novels", "app_state", "novel_id_sequence", "novel_id_reservations", "native_yaml_id_sequence", "native_yaml_id_claims", "native_id_liveness", "native_transfer_target_claims", "native_transfer_id_owners", "novel_revisions", "novel_gate_bindings", "transfer_object_refs"] {
        if backup_table_exists(control, table)? {
            let order = if matches!(table, "app_state" | "native_id_liveness" | "native_transfer_id_owners") { "1,2" } else { "1" };
            hash_backup_query(&mut hash, control, table, &format!("SELECT * FROM {table} ORDER BY {order}"))?;
        }
    }
    if backup_table_exists(control, "novel_access_gates")? {
        hash_backup_query(&mut hash, control, "novel_access_gates", "SELECT prefix,epoch,content_revision,origin_url_fold,closed,owner FROM novel_access_gates ORDER BY prefix")?;
    }
    if backup_table_exists(control, "objects")? {
        hash_backup_query(&mut hash, control, "objects", "SELECT object_key,size,encoding,crc32,generation FROM objects WHERE (object_key NOT LIKE '.narou-transfer/%' AND object_key NOT LIKE '.narou-staging/%') OR (object_key>='.narou-transfer/.refs/' AND object_key<'.narou-transfer/.refs0') OR object_key IN (SELECT blob_key FROM transfer_object_refs) ORDER BY object_key")?;
    }
    let gates = root.join(crate::downloader::ARCHIVE_ROOT_DIR).join(".objects/.narou-transfer/.gate");
    if gates.is_dir() {
        let mut paths = fs::read_dir(gates)?.map(|entry| entry.map(|entry| entry.path())).collect::<io::Result<Vec<_>>>()?;
        paths.sort();
        for path in paths {
            let bytes = crate::native::object_store::sanitize_gate_snapshot(&fs::read(&path)?)?;
            let gate: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| NarouError::Platform(error.to_string()))?;
            hash.update(path.file_name().ok_or_else(|| NarouError::Platform("gate has no name".into()))?.to_string_lossy().as_bytes());
            for name in ["prefix", "origin_url_fold"] {
                let value = gate.get(name).and_then(serde_json::Value::as_str).unwrap_or("");
                hash.update((value.len() as u64).to_le_bytes()); hash.update(value.as_bytes());
            }
            for name in ["epoch", "content_revision"] { hash.update(gate.get(name).and_then(serde_json::Value::as_u64).unwrap_or(0).to_le_bytes()); }
        }
    }
    let refs = root.join(crate::downloader::ARCHIVE_ROOT_DIR).join(".objects/.narou-transfer/.refs");
    if refs.is_dir() {
        let mut files = Vec::new();
        collect_backup_files(root, &refs, None, &mut files)?;
        files.sort();
        for path in files { hash.update(path.strip_prefix(root).map_err(|error| NarouError::Platform(error.to_string()))?.to_string_lossy().as_bytes()); hash.update(backup_file_digest(&path)?); }
    }
    Ok(hash.finalize().into())
}

fn hash_backup_query(hash: &mut sha2::Sha256, connection: &rusqlite::Connection, name: &str, query: &str) -> Result<()> {
    use sha2::Digest;
    hash.update(name.as_bytes());
    let mut statement = connection.prepare(query).map_err(crate::native::sqlite::sqlite_error)?;
    let columns = statement.column_count();
    let mut rows = statement.query([]).map_err(crate::native::sqlite::sqlite_error)?;
    while let Some(row) = rows.next().map_err(crate::native::sqlite::sqlite_error)? {
        hash.update([255]);
        for column in 0..columns {
            use rusqlite::types::ValueRef;
            match row.get_ref(column).map_err(crate::native::sqlite::sqlite_error)? {
                ValueRef::Null => hash.update([0]),
                ValueRef::Integer(value) => { hash.update([1]); hash.update(value.to_le_bytes()); }
                ValueRef::Real(value) => { hash.update([2]); hash.update(value.to_bits().to_le_bytes()); }
                ValueRef::Text(bytes) => { hash.update([3]); hash.update((bytes.len() as u64).to_le_bytes()); hash.update(bytes); }
                ValueRef::Blob(bytes) => { hash.update([4]); hash.update((bytes.len() as u64).to_le_bytes()); hash.update(bytes); }
            }
        }
    }
    Ok(())
}

fn sanitize_backup_image(root: &Path, remote_refs: &[crate::platform::ObjectKey], yaml_metadata: bool) -> Result<()> {
    let image = root.join(".narou/db.sqlite");
    let control = if image.is_file() { image } else { root.join(".narou/transfer/control.sqlite") };
    let mut connection = rusqlite::Connection::open(control).map_err(crate::native::sqlite::sqlite_error)?;
    connection.pragma_update(None, "foreign_keys", true).map_err(crate::native::sqlite::sqlite_error)?;
    let local_path = root.join(".narou/local_setting.yaml");
    let mut local = match fs::read_to_string(&local_path) { Ok(raw) => raw, Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(), Err(error) => return Err(error.into()) };
    if backup_table_exists(&connection, "objects")? {
        // Rehosted aliases use verified local canonical files. Delete stale
        // shadow rows too, including tombstones, before removing remote refs.
        connection.execute_batch("CREATE TEMP TABLE backup_gate_revisions AS SELECT prefix,content_revision FROM novel_access_gates; CREATE TEMP TABLE backup_record_revisions AS SELECT novel_id,content_revision FROM novel_revisions;")
            .map_err(crate::native::sqlite::sqlite_error)?;
        for key in remote_refs {
            let alias = format!(".narou-transfer/.refs/{key}");
            for logical in [key.as_ref(), alias.as_str()] {
                connection.execute("DELETE FROM transfer_object_refs WHERE object_key=?1", [logical]).map_err(crate::native::sqlite::sqlite_error)?;
                connection.execute("DELETE FROM object_chunks WHERE object_key=?1", [logical]).map_err(crate::native::sqlite::sqlite_error)?;
                connection.execute("DELETE FROM objects WHERE object_key=?1", [logical]).map_err(crate::native::sqlite::sqlite_error)?;
            }
        }
        crate::native::sqlite::object_store::sanitize_transfer_snapshot(&mut connection)?;
        connection.execute_batch("UPDATE novel_access_gates SET content_revision=(SELECT content_revision FROM backup_gate_revisions b WHERE b.prefix=novel_access_gates.prefix); UPDATE novel_revisions SET content_revision=(SELECT content_revision FROM backup_record_revisions b WHERE b.novel_id=novel_revisions.novel_id); DROP TABLE backup_gate_revisions; DROP TABLE backup_record_revisions;")
            .map_err(crate::native::sqlite::sqlite_error)?;
        if !yaml_metadata {
            local = connection.query_row("SELECT value_yaml FROM app_state WHERE scope='inv' AND key='local_setting'", [], |row| row.get::<_, String>(0))
                .optional().map_err(crate::native::sqlite::sqlite_error)?.unwrap_or_default();
        }
        let settings = backup_local_settings(&local)?;
        connection.execute("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES('inv','local_setting','{}',?1) ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml", [&settings]).map_err(crate::native::sqlite::sqlite_error)?;
    }
    // Only the isolated copies are sanitized. Allocator high water, durable
    // generations, committed identity receipts and retained origins survive.
    crate::native::transfer::sanitize_control_snapshot(&mut connection)?;
    if !yaml_metadata {
        // SQLite is the authority. Preserve pending/manual legacy files as
        // history, not as startup import instructions that could overwrite the
        // captured primary metadata when this backup is opened elsewhere.
        let files = ["database.yaml", "database_index.yaml"].into_iter()
            .chain(crate::db::state_inventory::import_export_states().map(|(_, file)| file));
        for file in files {
            let path = root.join(".narou").join(file);
            if path.is_file() {
                fs::rename(&path, root.join(".narou").join(format!("{file}.imported-backup-{}", uuid::Uuid::new_v4().simple())))?;
            }
        }
    }
    drop(connection);
    fs::write(local_path, backup_local_settings(&local)?)?;
    // Imported settings are retained user history, but credential keys must
    // not survive in those old metadata copies either.
    for entry in fs::read_dir(root.join(".narou"))? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.file_name().to_str().is_some_and(|name| name.starts_with("local_setting.yaml.imported-")) {
            fs::write(entry.path(), backup_local_settings(&fs::read_to_string(entry.path())?)?)?;
        }
    }
    Ok(())
}

fn backup_local_settings(raw: &str) -> Result<String> {
    let mut settings: std::collections::BTreeMap<String, serde_yaml::Value> = if raw.trim().is_empty() { Default::default() } else { serde_yaml::from_str(raw)? };
    settings.remove("s3.access-key-id");
    settings.remove("s3.secret-access-key");
    // Explicitly overrides environment configuration when this backup is
    // restored. Every remote canonical byte has already been rehosted locally.
    settings.insert(crate::native::s3::ASSET_BACKEND_SETTING.into(), serde_yaml::Value::String("local".into()));
    Ok(serde_yaml::to_string(&settings)?)
}

fn backup_file_digest(path: &Path) -> Result<[u8; 32]> {
    use sha2::Digest;
    use std::io::Read;
    let mut input = fs::File::open(path)?;
    let mut hash = sha2::Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 { break; }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().into())
}

fn copy_backup_tree(base: &Path, current: &Path, target: &Path, excluded: &[PathBuf]) -> Result<()> {
    if !current.exists() { return Ok(()); }
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() || excluded.iter().any(|exclude| path.starts_with(exclude)) || is_backup_excluded(base, &path) { continue; }
        if kind.is_dir() { copy_backup_tree(base, &path, target, excluded)?; }
        else if kind.is_file() {
            let relative = path.strip_prefix(base).map_err(|error| NarouError::Platform(error.to_string()))?;
            copy_backup_file(&path, &target.join(relative))?;
        }
    }
    Ok(())
}

fn copy_backup_file(source: &Path, destination: &Path) -> Result<()> {
    use std::io::Read;
    use sha2::Digest;
    let before = backup_file_digest(source)?;
    if let Some(parent) = destination.parent() { fs::create_dir_all(parent)?; }
    let mut input = fs::File::open(source)?;
    let mut output = fs::File::create(destination)?;
    let mut hash = sha2::Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 { break; }
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    if <[u8; 32]>::from(hash.finalize()) != before || backup_file_digest(source)? != before {
        return Err(NarouError::Platform(format!("backup source changed while copying: {}", source.display())));
    }
    Ok(())
}

fn add_directory_to_zip<W: io::Write + io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    base_dir: &Path,
    current_dir: &Path,
    exclude_dir: Option<&Path>,
    options: zip::write::SimpleFileOptions,
) -> Result<()> {
    let mut files = Vec::new();
    collect_backup_files(base_dir, current_dir, exclude_dir, &mut files)?;

    let mut entries: Vec<(String, PathBuf)> = files
        .into_iter()
        .map(|path| {
            let rel = path
                .strip_prefix(base_dir)
                .map_err(|e| NarouError::Conversion(e.to_string()))?;
            Ok((rel.to_string_lossy().replace('\\', "/"), path))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (rel_name, path) in entries {
        let mut file = fs::File::open(&path)?;
        zip.start_file(rel_name, options)
            .map_err(|e| NarouError::Conversion(e.to_string()))?;
        io::copy(&mut file, zip)?;
    }
    Ok(())
}

/// バックアップ対象を再帰列挙する。シンボリックリンクは辿らない。
/// 除外ルール:
/// - `exclude_dir` (保存先フォルダ) 以下
/// - live journals/stages/locks and credential keys, not canonical references
/// - live SQLite WAL/SHM (the database itself is captured with VACUUM INTO)
fn collect_backup_files(
    base_dir: &Path,
    current_dir: &Path,
    exclude_dir: Option<&Path>,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    if !current_dir.exists() { return Ok(()); }
    for entry in fs::read_dir(current_dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
            continue;
        }
        if path.is_dir() {
            // 保存先フォルダ以下は対象外 (zip の自己取り込み防止)。
            if let Some(exclude) = exclude_dir
                && let Ok(canonical) = path.canonicalize()
                && canonical.starts_with(exclude)
            {
                continue;
            }
            if is_backup_excluded(base_dir, &path) {
                continue;
            }
            collect_backup_files(base_dir, &path, exclude_dir, files)?;
        } else if path.is_file() && !is_backup_excluded(base_dir, &path) {
            files.push(path);
        }
    }
    Ok(())
}

fn is_backup_excluded(base_dir: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(base_dir) else {
        return false;
    };
    let text = rel.to_string_lossy().replace('\\', "/");
    let first = text.split('/').next().unwrap_or("");
    if first.starts_with(".narou-snapshot-") { return true; }
    if path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.ends_with(".lock") || (name.starts_with('.') && name.ends_with(".tmp"))) { return true; }
    if rel.starts_with(".narou") {
        let local = rel.strip_prefix(".narou").expect("checked prefix");
        let name = local.components().next().map(|component| component.as_os_str().to_string_lossy());
        if name.as_deref() == Some("transfer") {
            return local != Path::new("transfer") && local != Path::new("transfer/control.sqlite");
        }
        if name.as_deref().is_some_and(|name| matches!(name,
            "id_claims" | "materialization-locks" | "server.pid" | "login.key" | "ui_token" | "db.sqlite-wal" | "db.sqlite-shm"
        )) { return true; }
        return local.components().count() == 1 && name.is_some_and(|name| name.ends_with(".lock"));
    }
    let storage = rel.strip_prefix(crate::downloader::ARCHIVE_ROOT_DIR).unwrap_or(rel);
    if storage.starts_with(".objects/.narou-staging") { return true; }
    if let Ok(private) = storage.strip_prefix(".objects/.narou-transfer") {
        // Canonical references and durable epochs survive; only transient
        // owners/readers are stripped from the private backup image.
        return !private.as_os_str().is_empty() && !private.starts_with(".refs") && !private.starts_with(".gate");
    }
    false
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_offer_only_when_crossing_boundary() {
        // prev < 0.4.0 <= current → 提案
        assert!(should_offer(Some("0.3.6"), "0.4.0"));
        assert!(should_offer(Some("0.3.6"), "0.4.1"));
        // 初回起動 (marker なし) → 提案
        assert!(should_offer(None, "0.4.0"));
        // marker が壊れていても旧バージョン扱い → 提案
        assert!(should_offer(Some("garbage"), "0.4.0"));
        // 既に 0.4.0 以上で起動済み → 出さない
        assert!(!should_offer(Some("0.4.0"), "0.4.1"));
        assert!(!should_offer(Some("0.4.0"), "0.4.0"));
        // 現在が 0.4.0 未満 → 出さない
        assert!(!should_offer(Some("0.3.6"), "0.3.9"));
        assert!(!should_offer(None, "0.3.9"));
        // サフィックス付き表記も core で比較
        assert!(should_offer(Some("0.3.6 (develop)"), "0.4.0 (local-build)"));
    }

    #[test]
    fn backup_exclusion_rules() {
        let base = Path::new("/lib");
        // User history and regenerable but canonical files are preserved.
        assert!(!is_backup_excluded(
            base,
            Path::new("/lib/小説データ/site/1 title/backup/x.zip")
        ));
        // Runtime locks and PID files are not portable.
        assert!(!is_backup_excluded(
            base,
            Path::new("/lib/.narou/section_convert_cache/a.bin")
        ));
        assert!(is_backup_excluded(
            base,
            Path::new("/lib/.narou/database.yaml.lock")
        ));
        assert!(is_backup_excluded(
            base,
            Path::new("/lib/.narou/server.pid")
        ));
        // 資格情報の鍵と UI トークンは除外し、鍵以外の .narou/*.yaml は
        // 従来どおり含める。
        assert!(is_backup_excluded(
            base,
            &base.join(".narou").join("login.key")
        ));
        assert!(is_backup_excluded(
            base,
            &base.join(".narou").join("ui_token")
        ));
        assert!(!is_backup_excluded(
            base,
            &base.join(".narou").join("login_cookie.yaml")
        ));
        assert!(!is_backup_excluded(
            base,
            &base.join(".narou").join("author.yaml")
        ));
        // .narou の通常ファイルは残す
        assert!(!is_backup_excluded(
            base,
            Path::new("/lib/.narou/database.yaml")
        ));
        assert!(!is_backup_excluded(
            base,
            Path::new("/lib/.narou/last-run-version")
        ));
        // 小説データ本体は残す
        assert!(!is_backup_excluded(
            base,
            Path::new("/lib/小説データ/site/1 title/toc.yaml")
        ));
        // タイトルに backup を含むだけの小説は残す (完全一致のみ除外)
        assert!(!is_backup_excluded(
            base,
            Path::new("/lib/小説データ/site/1 backup-story/toc.yaml")
        ));
        assert!(is_backup_excluded(base, Path::new("/lib/.narou/id_claims/12-71.lock")));
        assert!(is_backup_excluded(base, Path::new("/lib/.narou/transfer/control.sqlite-wal")));
        assert!(is_backup_excluded(base, Path::new("/lib/.narou/transfer/operations/private.json")));
        assert!(!is_backup_excluded(base, Path::new("/lib/.narou/transfer/control.sqlite")));
    }

    #[test]
    fn backup_preserves_gate_epoch_and_user_files_without_live_transfer_state() {
        struct DatabaseRestore(Option<crate::db::Database>);
        impl Drop for DatabaseRestore { fn drop(&mut self) { *crate::db::DATABASE.lock() = self.0.take(); } }
        let root = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(root.path());
        let _database = DatabaseRestore(crate::db::DATABASE.lock().take());
        let _legacy = crate::test_support::legacy_yaml_guard();
        fs::create_dir(root.path().join(".narou")).unwrap();
        fs::write(root.path().join(".narou/login.key"), b"never export").unwrap();
        fs::create_dir_all(root.path().join("unregistered/backup")).unwrap();
        fs::write(root.path().join("unregistered/backup/user.bin"), b"user bytes").unwrap();
        crate::db::init_database().unwrap();
        use crate::platform::{NovelAccessFence, ObjectPrefix};
        let stores = crate::native::object_store::NativeStores::for_narou_root(root.path()).unwrap();
        let prefix = ObjectPrefix::new("novels/site/old-deleted-book").unwrap();
        let epoch = crate::native::materialization::block_on(async {
            let epoch = stores.primary.close_gate(&prefix, "test-overwrite").await?;
            stores.primary.open_gate(&prefix, "test-overwrite", epoch).await?;
            Ok(epoch)
        }).unwrap();
        let output = tempfile::tempdir().unwrap();
        let path = create_library_backup(root.path(), output.path()).unwrap();
        let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
        assert!(archive.by_name(".narou/login.key").is_err());
        let mut user = Vec::new();
        std::io::Read::read_to_end(&mut archive.by_name("unregistered/backup/user.bin").unwrap(), &mut user).unwrap();
        assert_eq!(user, b"user bytes");
        let gate_name = format!("{}/.objects/.narou-transfer/.gate/{}", crate::downloader::ARCHIVE_ROOT_DIR,
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(prefix.as_ref().as_bytes())));
        let gate: serde_json::Value = serde_json::from_reader(archive.by_name(&gate_name).unwrap()).unwrap();
        assert_eq!(gate["epoch"].as_u64(), Some(epoch));
        assert_eq!(gate["owner"], serde_json::Value::Null);
        assert!(gate["readers"].as_object().unwrap().is_empty());
    }

    use crate::platform::{NovelAccessFence, NovelRepository, ObjectKey, ObjectPrefix, ObjectStore, TransferStageStore};
    use crate::native::materialization::block_on;

    struct SavedDatabase(Option<crate::db::Database>);
    impl Drop for SavedDatabase {
        fn drop(&mut self) { *crate::db::DATABASE.lock() = self.0.take(); }
    }

    fn backup_record() -> crate::db::NovelRecord {
        use chrono::TimeZone;
        crate::db::NovelRecord {
            id: 12, author: "author".into(), title: "Backup source".into(), file_title: "registered".into(),
            toc_url: "https://example.test/registered/".into(), sitename: "site".into(), novel_type: 1,
            end: false, last_update: chrono::Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap(),
            new_arrivals_date: None, use_subdirectory: false, general_firstup: None, novelupdated_at: None,
            general_lastup: None, last_mail_date: None, tags: vec!["first".into(), "second".into()], ncode: None,
            domain: None, general_all_no: None, length: None, suspend: false, is_narou: false,
            last_check_date: None, convert_failure: false, requires_login: false, login_session: None,
            extra_fields: Default::default(),
        }
    }

    fn with_backup_library(sqlite: bool, test: impl FnOnce(&Path)) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(&path);
        let _database = SavedDatabase(crate::db::DATABASE.lock().take());
        let _legacy = (!sqlite).then(crate::test_support::legacy_yaml_guard);
        fs::create_dir(path.join(".narou")).unwrap();
        fs::create_dir(path.join(".narousetting")).unwrap();
        fs::write(path.join(".narousetting/global_setting.yaml"), "line-height: 1.75\n").unwrap();
        fs::write(path.join(".narou/local_setting.yaml"), "narou-compat: false\nsqlite.mirror-files: false\ns3.asset-backend: local\ncustom-user-setting: preserved\n").unwrap();
        let record = backup_record();
        fs::write(path.join(".narou/database.yaml"), serde_yaml::to_string(&std::collections::BTreeMap::from([(record.id, record)])).unwrap()).unwrap();
        if sqlite { crate::native::sqlite::state::write_mode(&path.join(".narou"), crate::native::sqlite::state::StorageMode::Sqlite).unwrap(); }
        crate::db::init_database().unwrap();
        test(&path);
    }

    fn local_backup_stores(root: &Path) -> crate::native::object_store::NativeStores {
        let primary = std::sync::Arc::new(crate::native::object_store::NativeStore::for_narou_root(root).unwrap());
        crate::native::object_store::NativeStores { primary: primary.clone(), objects: primary.clone(), assets: primary, s3: None }
    }

    fn write_registered_backup_content(stores: &crate::native::object_store::NativeStores) -> crate::application::novel_access::NovelWriteGuard {
        block_on(async {
            let keys = crate::platform::NovelObjectKeys::new("site", "registered", false)?;
            let write = crate::application::novel_access::NovelWriteGuard::capture_for(&stores.objects, Some(&stores.assets), &keys).await?;
            write.write_small(&keys.toc(), b"title: Backup source\nsubtitles: []\n".to_vec()).await?;
            write.write_small(&keys.section("1", "current"), b"current body bytes\n".to_vec()).await?;
            write.write_small(&keys.prefix().join("user-notes.bin")?, vec![0x5a; 2 * 64 * 1024 + 17]).await?;
            Ok(write)
        }).unwrap()
    }

    fn retain_backup_namespace(root: &Path, stores: &crate::native::object_store::NativeStores, prefix: &ObjectPrefix) -> crate::application::novel_access::NovelWriteGuard {
        let write = block_on(crate::application::novel_access::NovelWriteGuard::capture(&stores.objects, Some(&stores.assets), prefix)).unwrap();
        stores.primary.with_open_write(write.fence(), || crate::db::with_database(|database| database.with_library_metadata_snapshot(|inventory, primary|
            crate::native::transfer::with_control_metadata_snapshot(inventory.root_dir(), primary, |control|
                crate::native::object_store::bind_namespace_origin(root, control, prefix, "https://example.test/retained/"))
        ))).unwrap();
        write
    }

    fn write_retained_backup_reference(root: &Path, stores: &crate::native::object_store::NativeStores) -> (ObjectPrefix, ObjectKey, crate::application::novel_access::NovelWriteGuard) {
        use sha2::Digest;
        let prefix = ObjectPrefix::new("novels/site/retained").unwrap();
        let old = retain_backup_namespace(root, stores, &prefix);
        let key = ObjectKey::try_new(format!("{}/retained-user.bin", prefix.as_ref())).unwrap();
        let bytes = b"retained unregistered canonical bytes";
        block_on(async {
            let owner = "backup-retained-publication";
            stores.primary.stage_open("backup-retained-stage", owner, 1, 3600).await?;
            stores.primary.stage_append("backup-retained-stage", owner, 1, 0, bytes, true).await?;
            let sealed = stores.primary.stage_seal("backup-retained-stage", owner, 1, bytes.len() as u64, &hex::encode(sha2::Sha256::digest(bytes))).await?;
            let epoch = stores.primary.close_gate(&prefix, owner).await?;
            stores.primary.commit_staged(&prefix, owner, epoch, &sealed.key, &key).await?;
            stores.primary.open_gate(&prefix, owner, epoch).await?;
            Ok(())
        }).unwrap();
        (prefix, key, old)
    }

    #[test]
    fn library_backup_offer_detects_database_only_records_without_novel_directories() {
        with_backup_library(true, |root| {
            let archive = root.join(crate::downloader::ARCHIVE_ROOT_DIR);
            if archive.exists() { fs::remove_dir_all(&archive).unwrap(); }
            assert!(backup_offer_needed(root).unwrap());
            assert!(!archive.join("site/registered").exists(), "eligibility must not create content mirrors");
        });
    }

    #[test]
    fn coherent_backup_rejects_publication_after_metadata_without_readmitting() {
        for sqlite in [false, true] {
            with_backup_library(sqlite, |root| {
                let stores = local_backup_stores(root);
                let write = write_registered_backup_content(&stores);
                let primary = stores.primary.clone();
                let prefix = write.prefix().clone();
                let result = capture_library_backup_with_stores(root, None, stores, move || block_on(async {
                    let epoch = primary.close_gate(&prefix, "concurrent-publication").await?;
                    primary.open_gate(&prefix, "concurrent-publication", epoch).await
                }));
                assert!(result.is_err(), "backup must not recapture the newly opened epoch");
                assert!(block_on(write.write_small(&ObjectKey::try_new(format!("{}/late.bin", write.prefix().as_ref())).unwrap(), b"stale".to_vec())).is_err());
            });
        }
    }

    #[test]
    fn coherent_backup_rejects_new_bytes_under_the_same_admitted_epoch() {
        for sqlite in [false, true] {
            with_backup_library(sqlite, |root| {
                let stores = local_backup_stores(root);
                let write = write_registered_backup_content(&stores);
                let primary = stores.primary.clone();
                let body = crate::platform::NovelObjectKeys::new("site", "registered", false).unwrap().section("1", "current");
                let mutation = body.clone();
                let result = capture_library_backup_with_stores(root, None, stores, move ||
                    block_on(write.write_small(&mutation, b"concurrently published bytes\n".to_vec())));
                assert!(result.is_err(), "a new content revision cannot be paired with the old metadata image");
                assert_eq!(block_on(primary.read_small(&body)).unwrap().unwrap(), b"concurrently published bytes\n");
            });
        }
    }

    #[test]
    fn coherent_backup_rejects_global_metadata_changes_after_capture() {
        for sqlite in [false, true] {
            with_backup_library(sqlite, |root| {
                let stores = local_backup_stores(root);
                write_registered_backup_content(&stores);
                let result = capture_library_backup_with_stores(root, None, stores, || {
                    crate::db::settings::update(crate::setting_core::SettingScope::Local, |settings| {
                        settings.insert("concurrent-user-setting".into(), serde_yaml::Value::String("new".into()));
                        Ok(())
                    })
                });
                assert!(result.is_err(), "global metadata and canonical bytes cannot be mixed");
            });
        }
    }

    #[test]
    fn library_backup_sqlite_without_mirrors_restores_authoritative_metadata_and_retained_refs() {
        with_backup_library(true, |root| {
            let stores = local_backup_stores(root);
            write_registered_backup_content(&stores);
            let (prefix, retained, old) = write_retained_backup_reference(root, &stores);
            block_on(async {
                stores.primary.stage_open("unpublished-private-payload", "unrelated-session", 1, 3600).await?;
                stores.primary.stage_append("unpublished-private-payload", "unrelated-session", 1, 0, &vec![0x7d; 64 * 1024], true).await?;
                Ok(())
            }).unwrap();
            let state = crate::native::sqlite::state::active_for(&root.join(".narou")).unwrap();
            state.conn_ref().lock().execute("UPDATE novels SET title='fresh primary metadata' WHERE id=12", []).unwrap();
            state.set_raw("inv", "alias", "primary-alias: 12\n").unwrap();
            fs::write(root.join(".narou/alias.yaml"), "stale-pending-import: 99\n").unwrap();
            assert!(!root.join("小説データ/site/registered/toc.yaml").exists());
            let output = tempfile::tempdir().unwrap();
            let path = create_library_backup(root, output.path()).unwrap();
            let restored = tempfile::tempdir().unwrap();
            let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
            assert!((0..archive.len()).all(|index| !archive.by_index(index).unwrap().name().unwrap().starts_with(".narou/id_claims/")));
            archive.extract(restored.path()).unwrap();
            let connection = rusqlite::Connection::open(restored.path().join(".narou/db.sqlite")).unwrap();
            assert_eq!(connection.query_row("SELECT title FROM novels WHERE id=12", [], |row| row.get::<_, String>(0)).unwrap(), "fresh primary metadata");
            assert_eq!(connection.query_row("SELECT origin_url_fold FROM novel_access_gates WHERE prefix=?1", [prefix.as_ref()], |row| row.get::<_, String>(0)).unwrap(), "https://example.test/retained/");
            assert_eq!(connection.query_row("SELECT COUNT(*) FROM transfer_stage_ledger", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
            assert_eq!(connection.query_row("SELECT COUNT(*) FROM novel_reader_leases", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
            assert_eq!(connection.query_row("SELECT COUNT(*) FROM object_chunks c LEFT JOIN objects o ON o.object_key=c.object_key WHERE o.object_key IS NULL", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
            let private = crate::platform::transfer_stage::stage_data_key("unpublished-private-payload").unwrap();
            assert_eq!(connection.query_row("SELECT COUNT(*) FROM objects WHERE object_key=?1", [private.as_ref()], |row| row.get::<_, i64>(0)).unwrap(), 0);
            assert_eq!(block_on(stores.primary.stage_cursor("unpublished-private-payload")).unwrap().unwrap().confirmed_bytes, 64 * 1024);
            let copied = crate::native::sqlite::object_store::SqliteObjectStore::with_mirror_files(
                std::sync::Arc::new(parking_lot::Mutex::new(connection)),
                crate::native::object_store::NativeObjectStore::from_root(restored.path().join("小説データ")).unwrap(), false);
            assert_eq!(block_on(ObjectStore::read_small(&copied, &retained)).unwrap().unwrap(), b"retained unregistered canonical bytes");
            assert!(block_on(ObjectStore::write_small_guarded(&copied, old.fence(), &retained, b"old generation".to_vec())).is_err());
            let keys = crate::platform::NovelObjectKeys::new("site", "registered", false).unwrap();
            assert_eq!(block_on(ObjectStore::read_small(&copied, &keys.section("1", "current"))).unwrap().unwrap(), b"current body bytes\n");
            assert_eq!(block_on(ObjectStore::read_small(&copied, &keys.prefix().join("user-notes.bin").unwrap())).unwrap().unwrap(), vec![0x5a; 2 * 64 * 1024 + 17]);
            let settings: std::collections::BTreeMap<String, serde_yaml::Value> = serde_yaml::from_str(&fs::read_to_string(restored.path().join(".narou/local_setting.yaml")).unwrap()).unwrap();
            assert_eq!(settings["custom-user-setting"].as_str(), Some("preserved"));
            assert_eq!(settings["sqlite.mirror-files"].as_bool(), Some(false));
            assert_eq!(fs::read_to_string(restored.path().join(".narousetting/global_setting.yaml")).unwrap(), "line-height: 1.75\n");
            assert!(!restored.path().join(".narou/alias.yaml").exists());
            let reopened = crate::db::Database::with_root(restored.path().to_path_buf()).unwrap();
            assert_eq!(reopened.all_records()[&12].title, "fresh primary metadata");
            assert_eq!(reopened.inventory().load_raw("alias", crate::db::inventory::InventoryScope::Local).unwrap(), "primary-alias: 12\n");
            assert!(fs::read_dir(restored.path().join(".narou")).unwrap().any(|entry| entry.unwrap().file_name().to_str().is_some_and(|name| name.starts_with("alias.yaml.imported-backup-"))));
        });
    }

    #[test]
    fn library_backup_preserves_yaml_allocator_highwater_and_committed_identity_receipts() {
        with_backup_library(false, |root| {
            let stores = local_backup_stores(root);
            let repository = crate::native::novel_repository::NativeNovelRepository::new();
            let reservation = block_on(repository.allocate_id()).unwrap();
            let id = reservation.id();
            let generation = reservation.generation();
            let mut record = backup_record(); record.id = id.0; record.file_title = "newly-committed".into(); record.toc_url = "https://example.test/newly-committed/".into();
            block_on(async {
                let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, false)?;
                let mut write = crate::application::novel_access::NovelWriteGuard::capture_for(&stores.objects, Some(&stores.assets), &keys).await?;
                write.retain_reservation(&reservation)?;
                repository.reserve_create(write.fence(), &reservation, &record.toc_url).await?;
                write.write_small(&keys.toc(), b"title: newly committed\n".to_vec()).await?;
                repository.apply_create_guarded(write.fence(), &reservation, record).await?;
                repository.release_id(reservation).await
            }).unwrap();
            let control = crate::native::transfer::control_connection(root).unwrap();
            let before: (i64, i64) = control.lock().query_row("SELECT next_id,next_generation FROM native_yaml_id_sequence WHERE id=1", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
            let secret = "DO_NOT_BACKUP_PRIVATE_CONTROL_TOKEN";
            control.lock().execute("INSERT INTO transfer_native_rows(kind,owner,key,value,sort_key) VALUES('backend','unrelated-private','token',?1,'')", [serde_json::to_vec(secret).unwrap()]).unwrap();
            let snapshot = capture_library_backup_with_stores(root, None, stores, || Ok(())).unwrap();
            let copied = rusqlite::Connection::open(snapshot.path().join(".narou/transfer/control.sqlite")).unwrap();
            assert_eq!(copied.query_row("SELECT next_id,next_generation FROM native_yaml_id_sequence WHERE id=1", [], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))).unwrap(), before);
            assert_eq!(copied.query_row("SELECT generation,committed FROM native_id_liveness WHERE allocator='yaml' AND novel_id=?1", [id.0], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?))).unwrap(), (generation, true));
            assert_eq!(copied.query_row("SELECT COUNT(*) FROM native_transfer_target_claims", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
            assert_eq!(copied.query_row("SELECT COUNT(*) FROM transfer_native_rows", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
            assert!(!fs::read(snapshot.path().join(".narou/transfer/control.sqlite")).unwrap().windows(secret.len()).any(|bytes| bytes == secret.as_bytes()));
            assert_eq!(control.lock().query_row("SELECT COUNT(*) FROM transfer_native_rows WHERE owner='unrelated-private'", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
            assert!(!snapshot.path().join(".narou/id_claims").exists());
        });
    }

    #[test]
    fn library_backup_rejects_live_unpublished_creation_without_dropping_retained_user_data() {
        for sqlite in [false, true] {
            with_backup_library(sqlite, |root| {
                let stores = local_backup_stores(root);
                let prefix = ObjectPrefix::new("novels/site/retained").unwrap();
                let mut write = retain_backup_namespace(root, &stores, &prefix);
                let key = ObjectKey::try_new(format!("{}/user.bin", prefix.as_ref())).unwrap();
                block_on(write.write_small(&key, b"preexisting retained data".to_vec())).unwrap();
                let repository = crate::native::novel_repository::NativeNovelRepository::new();
                let reservation = block_on(repository.allocate_id()).unwrap();
                write.retain_reservation(&reservation).unwrap();
                block_on(repository.reserve_create(write.fence(), &reservation, "https://example.test/retained/")).unwrap();
                let output = tempfile::tempdir().unwrap();
                let old = output.path().join("narou-backup-preexisting.zip"); fs::write(&old, b"old backup remains").unwrap();
                assert!(create_library_backup(root, output.path()).is_err());
                assert_eq!(fs::read(&old).unwrap(), b"old backup remains");
                assert_eq!(block_on(stores.objects.read_small(&key)).unwrap().unwrap(), b"preexisting retained data");
                block_on(repository.release_id(reservation)).unwrap();
                let snapshot = capture_library_backup_with_stores(root, None, stores, || Ok(())).unwrap();
                assert_eq!(fs::read(snapshot.path().join("小説データ/site/retained/user.bin")).unwrap(), b"preexisting retained data");
            });
        }
    }

    #[test]
    fn library_backup_fails_closed_for_missing_or_corrupt_canonical_sqlite_references() {
        for corruption in ["missing", "generation", "sha"] {
            with_backup_library(true, |root| {
                let stores = local_backup_stores(root);
                let (_, key, _) = write_retained_backup_reference(root, &stores);
                let state = crate::native::sqlite::state::active_for(&root.join(".narou")).unwrap();
                {
                    let connection = state.conn_ref().lock();
                    match corruption {
                        "missing" => { connection.execute("DELETE FROM objects WHERE object_key=(SELECT blob_key FROM transfer_object_refs WHERE object_key=?1)", [key.as_ref()]).unwrap(); }
                        "generation" => { connection.execute("UPDATE objects SET generation='different-generation' WHERE object_key=(SELECT blob_key FROM transfer_object_refs WHERE object_key=?1)", [key.as_ref()]).unwrap(); }
                        _ => { connection.execute("UPDATE transfer_object_refs SET sha256=?1 WHERE object_key=?2", ["0".repeat(64), key.to_string()]).unwrap(); }
                    }
                }
                let output = tempfile::tempdir().unwrap();
                assert!(create_library_backup(root, output.path()).is_err(), "{corruption} must not produce an apparently successful ZIP");
                assert!(pending_offer(root).is_err(), "capture errors must not become already-done offers");
                assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
            });
        }
    }

    #[test]
    fn library_backup_excludes_output_inside_a_registered_novel_without_excluding_user_history() {
        with_backup_library(false, |root| {
            let stores = local_backup_stores(root);
            write_registered_backup_content(&stores);
            let output = root.join("小説データ/site/registered/backup-output");
            fs::create_dir_all(&output).unwrap(); fs::write(output.join("previous.zip"), b"do not recursively back up output").unwrap();
            fs::create_dir_all(root.join("小説データ/site/registered/backup")).unwrap();
            fs::write(root.join("小説データ/site/registered/backup/user-history.zip"), b"preserve history").unwrap();
            let path = create_library_backup(root, &output).unwrap();
            let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
            assert!(archive.by_name("小説データ/site/registered/backup-output/previous.zip").is_err());
            assert!(archive.by_name("小説データ/site/registered/backup/user-history.zip").is_ok());
            assert!(create_library_backup(root, root).is_err());
            assert!(create_library_backup(root, &root.join(".narou")).is_err());
            assert!(create_library_backup(root, &root.join("小説データ/site")).is_err());
        });
    }

    struct BackupS3Transport {
        objects: std::collections::BTreeMap<String, (Vec<u8>, String)>,
        listings: Vec<(String, u64)>,
        requests: parking_lot::Mutex<Vec<String>>,
    }

    impl crate::platform::HttpClient for BackupS3Transport {
        fn send<'a>(&'a self, request: crate::platform::HttpRequest) -> crate::platform::PlatformFuture<'a, Result<crate::platform::http::HttpResponse>> {
            Box::pin(async move {
                assert!(request.url.contains("list-type=2"), "backup must not require a staging journal: {}", request.url);
                let url = url::Url::parse(&request.url).unwrap();
                let query = url.query_pairs().collect::<std::collections::BTreeMap<_, _>>();
                let prefix = query.get("prefix").map(|value| value.as_ref()).unwrap_or("");
                let after = query.get("start-after").map(|value| value.as_ref()).unwrap_or("");
                let offset = query.get("continuation-token").map(|value| value.strip_prefix("opaque-backup:").unwrap().parse::<usize>().unwrap()).unwrap_or(0);
                let cap = query.get("max-keys").map(|value| value.parse::<usize>().unwrap()).unwrap_or(100);
                let rows = self.listings.iter().filter(|(key, _)| key.starts_with(prefix) && key.as_str() > after).collect::<Vec<_>>();
                let end = (offset + cap).min(rows.len());
                let more = end < rows.len();
                let mut xml = format!("<ListBucketResult><IsTruncated>{more}</IsTruncated>");
                for (key, size) in rows.into_iter().skip(offset).take(cap) { xml.push_str(&format!("<Contents><Key>{key}</Key><Size>{size}</Size><ETag>legacy-etag</ETag></Contents>")); }
                if more { xml.push_str(&format!("<NextContinuationToken>opaque-backup:{end}</NextContinuationToken>")); }
                xml.push_str("</ListBucketResult>");
                Ok(crate::platform::http::HttpResponse { status: 200, headers: Vec::new(), body: xml.into_bytes() })
            })
        }

        fn send_stream<'a>(&'a self, request: crate::platform::HttpRequest) -> crate::platform::PlatformFuture<'a, Result<crate::platform::http::HttpStreamResponse>> {
            Box::pin(async move {
                self.requests.lock().push(request.url.clone());
                let Some((bytes, etag)) = self.objects.get(&request.url) else {
                    return Ok(crate::platform::http::HttpStreamResponse { status: 404, url: request.url, headers: Vec::new(), body: Box::pin(futures::stream::empty()) });
                };
                if request.header("if-match").is_some_and(|expected| expected != etag) {
                    return Ok(crate::platform::http::HttpStreamResponse { status: 412, url: request.url, headers: Vec::new(), body: Box::pin(futures::stream::empty()) });
                }
                let range = request.header("range").map(|value| {
                    let (start, end) = value.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
                    let start: usize = start.parse().unwrap();
                    let end = if end.is_empty() { bytes.len() } else { end.parse::<usize>().unwrap() + 1 };
                    start..end.min(bytes.len())
                });
                let mut headers = vec![("etag".into(), etag.clone())];
                let (status, body) = if let Some(range) = range {
                    headers.push(("content-range".into(), format!("bytes {}-{}/{}", range.start, range.end - 1, bytes.len())));
                    (206, bytes[range].to_vec())
                } else { (200, bytes.clone()) };
                headers.push(("content-length".into(), body.len().to_string()));
                Ok(crate::platform::http::HttpStreamResponse { status, url: request.url, headers, body: crate::platform::staging::owned_bytes_stream(body) })
            })
        }
    }

    fn remote_backup_stores(root: &Path, payload: Option<Vec<u8>>, legacy: bool) -> (crate::native::object_store::NativeStores, std::sync::Arc<BackupS3Transport>) {
        let mut stores = local_backup_stores(root);
        let physical = ObjectKey::try_new(".narou-transfer/.data/original-remote-stage").unwrap();
        let legacy_key = "novels/site/retained/remote-user-note.bin";
        let mut objects = std::collections::BTreeMap::new();
        if let Some(bytes) = payload { objects.insert(format!("https://backup-s3.test/bucket/{}", physical.as_ref()), (bytes, "\"remote-generation\"".into())); }
        if legacy { objects.insert(format!("https://backup-s3.test/bucket/{legacy_key}"), (b"legacy retained S3 user bytes".to_vec(), "\"legacy-etag\"".into())); }
        let transport = std::sync::Arc::new(BackupS3Transport { objects, listings: if legacy { vec![(legacy_key.into(), 29)] } else { Vec::new() }, requests: Default::default() });
        let remote = std::sync::Arc::new(crate::platform::S3Store::new(crate::platform::S3StoreConfig {
            endpoint: "https://backup-s3.test".into(), bucket: "bucket".into(), region: "test".into(), prefix: String::new(),
            illustration_dedup: false, access_key_id: "test".into(), secret_access_key: "test".into(),
        }, transport.clone(), std::sync::Arc::new(crate::platform::SystemClock)).unwrap());
        stores.s3 = Some(remote);
        (stores, transport)
    }

    fn install_backup_remote_reference(root: &Path, stores: &crate::native::object_store::NativeStores) -> ObjectKey {
        use sha2::Digest;
        let prefix = ObjectPrefix::new("novels/site/retained").unwrap();
        retain_backup_namespace(root, stores, &prefix);
        let key = ObjectKey::try_new(format!("{}/remote.bin", prefix.as_ref())).unwrap();
        let reference = serde_json::json!({"physical":".narou-transfer/.data/original-remote-stage","size":18,"sha256":hex::encode(sha2::Sha256::digest(b"immutable S3 bytes")),"logical":key.to_string(),"prefix":prefix.as_ref(),"etag":"\"remote-generation\""});
        block_on(stores.primary.write_small(&ObjectKey::try_new(format!(".narou-transfer/.refs/{key}")).unwrap(), serde_json::to_vec(&reference).unwrap())).unwrap();
        key
    }

    #[test]
    fn library_backup_rehosts_unregistered_s3_refs_and_legacy_user_objects_without_remote_journals() {
        for sqlite in [false, true] {
            with_backup_library(sqlite, |root| {
                let (stores, transport) = remote_backup_stores(root, Some(b"immutable S3 bytes".to_vec()), true);
                let key = install_backup_remote_reference(root, &stores);
                let primary = stores.primary.clone();
                let source_ref = ObjectKey::try_new(format!(".narou-transfer/.refs/{key}")).unwrap();
                let snapshot = capture_library_backup_with_stores(root, None, stores, || Ok(())).unwrap();
                assert_eq!(fs::read(snapshot.path().join("小説データ/site/retained/remote.bin")).unwrap(), b"immutable S3 bytes");
                assert_eq!(fs::read(snapshot.path().join("小説データ/site/retained/remote-user-note.bin")).unwrap(), b"legacy retained S3 user bytes");
                assert!(!snapshot.path().join("小説データ/.objects/.narou-transfer/.refs").exists());
                assert!(block_on(primary.read_small(&source_ref)).unwrap().is_some(), "source ref must not be rewritten");
                let restored = if sqlite {
                    let connection = rusqlite::Connection::open(snapshot.path().join(".narou/db.sqlite")).unwrap();
                    let count: i64 = connection.query_row("SELECT COUNT(*) FROM objects WHERE object_key LIKE '.narou-transfer/.refs/%'", [], |row| row.get(0)).unwrap();
                    assert_eq!(count, 0);
                    crate::native::object_store::NativeStore::Sqlite(crate::native::sqlite::object_store::SqliteObjectStore::with_mirror_files(std::sync::Arc::new(parking_lot::Mutex::new(connection)), crate::native::object_store::NativeObjectStore::from_root(snapshot.path().join("小説データ")).unwrap(), false))
                } else { crate::native::object_store::NativeStore::Fs(crate::native::object_store::NativeObjectStore::from_root(snapshot.path().join("小説データ")).unwrap()) };
                assert_eq!(block_on(restored.read_small(&key)).unwrap().unwrap(), b"immutable S3 bytes");
                assert!(fs::read_to_string(snapshot.path().join(".narou/local_setting.yaml")).unwrap().contains("s3.asset-backend: local"));
                assert!(transport.requests.lock().iter().any(|url| url.ends_with("original-remote-stage")));
            });
        }
    }

    #[test]
    fn library_backup_captures_every_retained_s3_page_with_opaque_cursors() {
        with_backup_library(true, |root| {
            let stores = local_backup_stores(root);
            retain_backup_namespace(root, &stores, &ObjectPrefix::new("novels/site/retained").unwrap());
            let listings = (0..132).map(|index| (format!("novels/site/retained/user-{index:03}.bin"), 9)).collect::<Vec<_>>();
            let transport = std::sync::Arc::new(BackupS3Transport {
                objects: listings.iter().map(|(key, _)| (format!("https://backup-s3.test/bucket/{key}"), (b"page data".to_vec(), "\"legacy-etag\"".into()))).collect(),
                listings, requests: Default::default(),
            });
            let remote = std::sync::Arc::new(crate::platform::S3Store::new(crate::platform::S3StoreConfig {
                endpoint: "https://backup-s3.test".into(), bucket: "bucket".into(), region: "test".into(), prefix: String::new(),
                illustration_dedup: false, access_key_id: "test".into(), secret_access_key: "test".into(),
            }, transport, std::sync::Arc::new(crate::platform::SystemClock)).unwrap());
            let stores = crate::native::object_store::NativeStores { s3: Some(remote), ..stores };
            let snapshot = capture_library_backup_with_stores(root, None, stores, || Ok(())).unwrap();
            for index in 0..132 {
                assert_eq!(fs::read(snapshot.path().join(format!("小説データ/site/retained/user-{index:03}.bin"))).unwrap(), b"page data");
            }
        });
    }

    #[test]
    fn library_backup_missing_or_corrupt_s3_refs_fail_closed_and_keep_source_ownership() {
        for payload in [None, Some(b"CORRUPTED S3 bytes".to_vec())] {
            with_backup_library(true, |root| {
                let (stores, _) = remote_backup_stores(root, payload, false);
                let key = install_backup_remote_reference(root, &stores);
                let primary = stores.primary.clone();
                let result = capture_library_backup_with_stores(root, None, stores, || Ok(()));
                assert!(result.is_err());
                assert!(block_on(primary.read_small(&ObjectKey::try_new(format!(".narou-transfer/.refs/{key}")).unwrap())).unwrap().is_some());
            });
        }
    }

    #[test]
    fn format_size_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }
}
