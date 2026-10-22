use std::path::{Path, PathBuf};
use std::sync::Arc;

use narou_rs::db;
use narou_rs::db::paths::novel_dir_for_record;
use narou_rs::illustration_store;
use narou_rs::platform::{AssetStore, ObjectStore, StoreMigrationState};

use super::download;
use super::log;

/// Subcommand for the `narou illust` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IllustSubcommand {
    Orphan,
    Migrate,
    FixExt,
    Rebuild,
    /// 挿絵をローカルから S3 互換ストレージへ写す (既定 dry-run)。
    S3Push,
    /// ローカルと S3 の挿絵を突き合わせる。
    S3Verify,
    /// 旧 `挿絵/` 配置の S3 オブジェクトを dedup プールへ移し、残ったものを消す。
    S3Dedup,
}

/// Run the `narou illust <sub>` command.
///
/// `force` switches from dry-run to actually mutating the filesystem.
/// `all` iterates over every novel; otherwise `targets` is resolved.
pub fn cmd_illust(sub: IllustSubcommand, targets: &[String], force: bool, all: bool) -> i32 {
    match cmd_illust_inner(sub, targets, force, all) {
        Ok(()) => 0,
        Err(err) => {
            log::report_error(&err);
            1
        }
    }
}

fn cmd_illust_inner(
    sub: IllustSubcommand,
    targets: &[String],
    force: bool,
    all: bool,
) -> Result<(), String> {
    db::init_database().map_err(|e| e.to_string())?;

    // S3 への移行・突き合わせ・dedup は小説単位ではなくライブラリ全体の操作。
    match sub {
        IllustSubcommand::S3Push => return run_s3_push(force),
        IllustSubcommand::S3Verify => return run_s3_verify(),
        IllustSubcommand::S3Dedup => return run_s3_dedup(force),
        _ => {}
    }

    let dirs = resolve_target_dirs(targets, all)?;

    if dirs.is_empty() {
        println!("対象の小説が見つかりませんでした。");
        return Ok(());
    }

    let total = dirs.len();
    for (idx, dir) in dirs.into_iter().enumerate() {
        if total > 1 {
            println!("---");
        }
        println!("[{} / {}] {}", idx + 1, total, dir.display());
        match sub {
            IllustSubcommand::Orphan => run_orphan(&dir, force)?,
            IllustSubcommand::Migrate => run_migrate(&dir, force)?,
            IllustSubcommand::FixExt => run_fix_ext(&dir, force)?,
            IllustSubcommand::Rebuild => run_rebuild(&dir)?,
            // ライブラリ全体の操作なので、この小説単位のループには来ない
            // (手前で return する)。
            IllustSubcommand::S3Push | IllustSubcommand::S3Verify | IllustSubcommand::S3Dedup => {
                unreachable!("S3 subcommands run before the per-novel loop")
            }
        }
    }
    Ok(())
}


/// 移行用のストア (移行元 = ローカル、移行先 = S3)。
///
/// 挿絵の保存先が S3 になっていなければ失敗させる。ここでローカルへ
/// フォールバックすると、利用者が「移行した」と誤解する。
#[allow(clippy::type_complexity)]
fn s3_migration_stores() -> Result<
    (
        Arc<dyn ObjectStore>,
        Arc<dyn AssetStore>,
        Arc<dyn ObjectStore>,
        Arc<dyn AssetStore>,
    ),
    String,
> {
    let root = narou_rs::db::inventory::Inventory::with_default_root()
        .map_err(|error| error.to_string())?
        .root_dir()
        .to_path_buf();
    let local = Arc::new(
        narou_rs::native::object_store::NativeStore::for_narou_root(&root)
            .map_err(|error| error.to_string())?,
    );
    let s3 = narou_rs::native::s3::illustration_store()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            "挿絵の保存先が S3 になっていません (narou setting s3.asset-backend=s3 と S3 の接続情報が必要です)"
                .to_string()
        })?;
    let from_objects: Arc<dyn ObjectStore> = local.clone();
    let from_assets: Arc<dyn AssetStore> = local;
    let to_objects: Arc<dyn ObjectStore> = s3.clone();
    let to_assets: Arc<dyn AssetStore> = s3;
    Ok((from_objects, from_assets, to_objects, to_assets))
}

fn migration_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
}

fn report_migration_failures(state: &StoreMigrationState) {
    if state.failed.is_empty() {
        return;
    }
    println!("失敗 {} 件:", state.failed.len());
    for entry in state.failed.iter().take(20) {
        println!("  {}", entry);
    }
    if state.failed.len() > 20 {
        println!("  ... 他 {} 件", state.failed.len() - 20);
    }
}

/// 挿絵をローカルから S3 へ写す。既定は数えるだけで、`--force` で実行する。
fn run_s3_push(force: bool) -> Result<(), String> {
    let (from_objects, from_assets, to_objects, to_assets) = s3_migration_stores()?;
    let action = if force { "copy" } else { "plan" };
    let runtime = migration_runtime()?;
    let mut state = StoreMigrationState::default();
    while !state.done {
        runtime
            .block_on(narou_rs::platform::migrate_page(
                &from_objects,
                &from_assets,
                &to_assets,
                &to_objects,
                action,
                narou_rs::platform::store_migration::DEFAULT_LIMIT,
                &mut state,
            ))
            .map_err(|error| error.to_string())?;
    }
    if force {
        println!("S3 へ写しました: {} 件", state.copied);
    } else {
        println!(
            "移行対象: {} 件 / {} バイト (実行するには --force)",
            state.planned, state.planned_bytes
        );
    }
    report_migration_failures(&state);
    Ok(())
}

/// ローカルと S3 の挿絵をバイト単位で突き合わせる (書き込みはしない)。
fn run_s3_verify() -> Result<(), String> {
    let (from_objects, from_assets, to_objects, to_assets) = s3_migration_stores()?;
    let runtime = migration_runtime()?;
    let mut state = StoreMigrationState::default();
    while !state.done {
        runtime
            .block_on(narou_rs::platform::migrate_page(
                &from_objects,
                &from_assets,
                &to_assets,
                &to_objects,
                "verify",
                narou_rs::platform::store_migration::DEFAULT_LIMIT,
                &mut state,
            ))
            .map_err(|error| error.to_string())?;
    }
    println!("一致: {} 件", state.verified);
    report_migration_failures(&state);
    Ok(())
}
/// 旧 `挿絵/` 配置の S3 オブジェクトを dedup プールへ寄せる。
///
/// 手順: 全小説の `挿絵/` を LIST → hex 名のものを pool へ `copy` →
/// `pool_exists` で確認後、legacy の実オブジェクトを `delete`。
/// 既定は dry-run (pool へ写さず件数だけ数える)。
fn run_s3_dedup(force: bool) -> Result<(), String> {
    let s3 = narou_rs::native::s3::illustration_store()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            "挿絵の保存先が S3 になっていません (narou setting s3.asset-backend=s3 と S3 の接続情報が必要です)"
                .to_string()
        })?;
    let runtime = migration_runtime()?;
    let mut state = narou_rs::platform::IllustrationDedupState::default();
    let action = if force { "copy" } else { "plan" };
    while !state.done {
        runtime
            .block_on(narou_rs::platform::deduplicate_illustrations_page(
                s3.as_ref(),
                action,
                narou_rs::platform::store_migration::MAX_LIMIT,
                &mut state,
            ))
            .map_err(|error| error.to_string())?;
    }

    if !force {
        println!(
            "dedup 予定: {} 件 / 非hexスキップ {} 件 (実行するには --force)",
            state.planned, state.skipped_non_hex
        );
    } else {
        println!(
            "dedup 完了: 移行 {} 件 / 既に pool {} 件 / 非hexスキップ {} 件 / 失敗 {} 件",
            state.migrated,
            state.already_pooled,
            state.skipped_non_hex,
            state.failed.len()
        );
    }
    for entry in state.failed.iter().take(20) {
        eprintln!("  {entry}");
    }
    if state.failed.len() > 20 {
        eprintln!("  ... 他 {} 件", state.failed.len() - 20);
    }
    Ok(())
}

fn resolve_target_dirs(targets: &[String], all: bool) -> Result<Vec<PathBuf>, String> {
    if all {
        return collect_all_novel_dirs();
    }
    if targets.is_empty() {
        let Some(target) = super::latest_convert_target() else {
            return Ok(Vec::new());
        };
        let Some(dir) = resolve_novel_dir(&target) else {
            log::report_error(&format!("{} は存在しません", target));
            return Ok(Vec::new());
        };
        return Ok(vec![dir]);
    }

    let expanded = download::tagname_to_ids(targets);
    let mut dirs = Vec::new();
    for target in expanded {
        let Some(dir) = resolve_novel_dir(&target) else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };
        dirs.push(dir);
    }
    Ok(dirs)
}

fn collect_all_novel_dirs() -> Result<Vec<PathBuf>, String> {
    let frozen_ids = narou_rs::compat::load_frozen_ids().map_err(|e| e.to_string())?;
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let ids = novels
        .scan_ids_sync(&narou_rs::platform::NovelFilter::all(), None, usize::MAX)
        .map_err(|e| e.to_string())?;
    let archive_root = narou_rs::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
        .map_err(|e| e.to_string())?;
    let mut dirs = Vec::new();
    for id in ids {
        let Ok(Some(record)) = novels.get_sync(id) else {
            continue;
        };
        if narou_rs::compat::record_is_frozen(&record, &frozen_ids) {
            continue;
        }
        dirs.push(novel_dir_for_record(&archive_root, &record));
    }
    Ok(dirs)
}

fn resolve_novel_dir(target: &str) -> Option<PathBuf> {
    let id = super::resolve_target_to_id(target)?;
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let record = novels.get_sync(id.into()).ok().flatten()?;
    let archive_root = narou_rs::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
        .unwrap_or_else(|_| PathBuf::from(narou_rs::downloader::ARCHIVE_ROOT_DIR));
    Some(novel_dir_for_record(&archive_root, &record))
}

fn run_orphan(archive_path: &Path, force: bool) -> Result<(), String> {
    if !archive_path.join("挿絵").is_dir() {
        // 実体がローカルに無い構成では、ストア上のオブジェクトを直接調べる。
        let scan =
            narou_rs::native::illustrations::scan_stored_illustrations_blocking(archive_path)
                .map_err(|e| e.to_string())?;
        return run_orphan_scan(archive_path, scan, force);
    }

    let orphans =
        illustration_store::find_orphan_illustrations(archive_path).map_err(|e| e.to_string())?;
    if orphans.is_empty() {
        println!("  孤児挿絵はありません。");
        return Ok(());
    }
    println!("  孤児挿絵: {} 件", orphans.len());
    for path in &orphans {
        if force {
            match remove_illustration_or_local(path) {
                Ok(()) => println!("  [削除] {}", path.display()),
                Err(err) => {
                    log::report_error(&format!("{} の削除に失敗しました: {}", path.display(), err))
                }
            }
        } else {
            println!("  {}", path.display());
        }
    }
    if !force {
        println!("  -f を指定すると実際に削除します。");
    }
    Ok(())
}

/// ストア上のオブジェクトを対象にした孤児の表示・削除。
fn run_orphan_scan(
    archive_path: &Path,
    scan: narou_rs::native::illustrations::StoredIllustrationScan,
    force: bool,
) -> Result<(), String> {
    if scan.total == 0 || scan.orphans.is_empty() {
        println!("  孤児挿絵はありません。");
        return Ok(());
    }
    println!("  孤児挿絵: {} 件", scan.orphans.len());
    let stores = native_stores()?;
    for (filename, key) in &scan.orphans {
        let display = archive_path.join("挿絵").join(filename);
        if force {
            match futures::executor::block_on(narou_rs::platform::ObjectStore::delete(
                stores.objects.as_ref(),
                key,
            )) {
                Ok(()) => println!("  [削除] {}", display.display()),
                Err(err) => log::report_error(&format!(
                    "{} の削除に失敗しました: {}",
                    display.display(),
                    err
                )),
            }
        } else {
            println!("  {}", display.display());
        }
    }
    if !force {
        println!("  -f を指定すると実際に削除します。");
    }
    Ok(())
}

/// 挿絵 1 件をストアとローカルの両方から消す。
///
/// mirror-files=false ではストアの delete が実ファイルに触れないため、
/// 残っていればファイルも消す (どちらか一方だけが残ると不整合になる)。
fn remove_illustration_or_local(path: &Path) -> Result<(), String> {
    let archive_root = archive_root()?;
    let stores = native_stores()?;
    if let Some(key) = narou_rs::native::object_store::object_key_for_file(&archive_root, path) {
        futures::executor::block_on(narou_rs::platform::ObjectStore::delete(
            stores.objects.as_ref(),
            &key,
        ))
        .map_err(|e| e.to_string())?;
    }
    if path.is_file() {
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn archive_root() -> Result<PathBuf, String> {
    let inventory = narou_rs::db::inventory::Inventory::with_default_root()
        .map_err(|e| e.to_string())?;
    Ok(inventory
        .root_dir()
        .join(narou_rs::downloader::ARCHIVE_ROOT_DIR))
}

fn native_stores() -> Result<narou_rs::native::object_store::NativeStores, String> {
    let inventory = narou_rs::db::inventory::Inventory::with_default_root()
        .map_err(|e| e.to_string())?;
    narou_rs::native::object_store::NativeStores::for_narou_root(inventory.root_dir())
        .map_err(|e| e.to_string())
}

fn run_migrate(archive_path: &Path, force: bool) -> Result<(), String> {
    if store_backed_illustrations_only(archive_path)? {
        report_fs_only_operation(
            "legacy ファイル名の移行",
            Some("S3 上の legacy 配置は `narou illust s3-dedup` で dedup プールへ移せます"),
        );
        return Ok(());
    }
    let plans = illustration_store::plan_legacy_illustration_migrations(archive_path)
        .map_err(|e| e.to_string())?;
    if plans.is_empty() {
        println!("  移行対象の legacy ファイルはありません。");
        return Ok(());
    }
    println!("  移行対象: {} 件", plans.len());
    for plan in &plans {
        if force {
            println!(
                "  [予定] {} -> {}",
                plan.old_path.display(),
                plan.new_path.display()
            );
        } else {
            println!(
                "  [dry-run] {} -> {}",
                plan.old_path.display(),
                plan.new_path.display()
            );
        }
    }
    if !force {
        println!("  -f を指定すると実際に移行します。");
        return Ok(());
    }
    let renamed = illustration_store::apply_legacy_illustration_migrations(archive_path)
        .map_err(|e| e.to_string())?;
    let renames: Vec<(PathBuf, PathBuf)> = plans
        .iter()
        .map(|plan| (plan.old_path.clone(), plan.new_path.clone()))
        .collect();
    sync_illustration_renames_in_store(&renames)?;
    push_illustration_index_to_store(archive_path)?;
    println!("  移行完了: {} 件", renamed);
    Ok(())
}

fn run_fix_ext(archive_path: &Path, force: bool) -> Result<(), String> {
    if store_backed_illustrations_only(archive_path)? {
        report_fs_only_operation("拡張子の修正", None);
        return Ok(());
    }
    let illust_dir = archive_path.join("挿絵");
    let plans = illustration_store::plan_extension_fixes(&illust_dir).map_err(|e| e.to_string())?;
    if plans.is_empty() {
        println!("  拡張子の修正対象はありません。");
        return Ok(());
    }
    println!("  拡張子の修正対象: {} 件", plans.len());
    for (old_path, new_path) in &plans {
        if force {
            println!("  [予定] {} -> {}", old_path.display(), new_path.display());
        } else {
            println!(
                "  [dry-run] {} -> {}",
                old_path.display(),
                new_path.display()
            );
        }
    }
    if !force {
        println!("  -f を指定すると実際に改名します。");
        return Ok(());
    }
    let renamed = illustration_store::apply_extension_fixes(&illust_dir, &plans)
        .map_err(|e| e.to_string())?;
    sync_illustration_renames_in_store(&plans)?;
    println!("  改名完了: {} 件", renamed);
    Ok(())
}

/// ローカル `挿絵/` に実体が無く、挿絵がストアにだけある構成か。
fn store_backed_illustrations_only(archive_path: &Path) -> Result<bool, String> {
    if archive_path.join("挿絵").is_dir() {
        return Ok(false);
    }
    let count = narou_rs::native::illustrations::stored_illustration_count_blocking(archive_path)
        .map_err(|e| e.to_string())?;
    Ok(count > 0)
}

fn report_fs_only_operation(operation: &str, hint: Option<&str>) {
    println!("  挿絵はローカルに無く、ストア (SQLite/S3) に保存されています。");
    println!(
        "  「{operation}」はローカルの実体を対象にする操作のため、この構成では実行できません。"
    );
    if let Some(hint) = hint {
        println!("  {hint}");
    }
}

/// ローカルで改名した挿絵を、ストア側のキーにも反映する。
///
/// mirror-files=true / S3 構成では DB 行や S3 オブジェクトのキーは改名に
/// 追随しないため、旧キーが保管されている場合だけ copy → delete する。
/// (純 YAML 構成では改名済みで旧キーが無いので何もしない。)
fn sync_illustration_renames_in_store(renames: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    if renames.is_empty() {
        return Ok(());
    }
    let archive_root = archive_root()?;
    let stores = native_stores()?;
    for (old_path, new_path) in renames {
        let Some(old_key) = narou_rs::native::object_store::object_key_for_file(&archive_root, old_path)
        else {
            continue;
        };
        let Some(new_key) = narou_rs::native::object_store::object_key_for_file(&archive_root, new_path)
        else {
            continue;
        };
        let stored =
            futures::executor::block_on(narou_rs::platform::AssetStore::stat(stores.assets.as_ref(), &old_key))
                .map_err(|e| e.to_string())?;
        if stored.is_none() {
            continue;
        }
        futures::executor::block_on(narou_rs::platform::AssetStore::copy(
            stores.assets.as_ref(),
            &old_key,
            &new_key,
        ))
        .map_err(|e| e.to_string())?;
        futures::executor::block_on(narou_rs::platform::AssetStore::delete(
            stores.assets.as_ref(),
            &old_key,
        ))
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// ローカルの索引ファイルをストアの `.illustration_cache.yaml` へ書き戻す。
fn push_illustration_index_to_store(archive_path: &Path) -> Result<(), String> {
    let cache = illustration_store::cache_path(archive_path);
    let Ok(body) = std::fs::read(&cache) else {
        return Ok(());
    };
    let archive_root = archive_root()?;
    let Some(prefix) =
        narou_rs::native::object_store::novel_object_prefix(&archive_root, archive_path)
    else {
        return Ok(());
    };
    let key = prefix.join(".illustration_cache.yaml").map_err(|e| e.to_string())?;
    let stores = native_stores()?;
    futures::executor::block_on(narou_rs::platform::ObjectStore::write_small(
        stores.objects.as_ref(),
        &key,
        body,
    ))
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn run_rebuild(archive_path: &Path) -> Result<(), String> {
    if !archive_path.join("挿絵").is_dir() {
        return run_rebuild_via_store(archive_path);
    }
    let count =
        illustration_store::rebuild_illustration_cache(archive_path).map_err(|e| e.to_string())?;
    println!("  cache 再構築完了: {} エントリ", count);
    Ok(())
}

/// 実体がストアにだけある構成の索引再構築。
///
/// ストアから本文 (`raw/*.html` を含む) と挿絵を取り出し、既存の FS 再構築
/// ロジックで索引を組み直してストアへ書き戻す。取り出したファイルは
/// ガードが片付けるので、ローカルには残さない。
fn run_rebuild_via_store(archive_path: &Path) -> Result<(), String> {
    let inventory = narou_rs::db::inventory::Inventory::with_default_root()
        .map_err(|e| e.to_string())?;
    let archive_root = inventory
        .root_dir()
        .join(narou_rs::downloader::ARCHIVE_ROOT_DIR);
    let store = narou_rs::native::object_store::NativeStore::for_narou_root(inventory.root_dir())
        .map_err(|e| e.to_string())?;
    let _files = store
        .materialize_novel_files(archive_path)
        .map_err(|e| e.to_string())?;
    let _illustrations = narou_rs::native::illustrations::materialize_blocking(archive_path)
        .map_err(|e| e.to_string())?;
    let count =
        illustration_store::rebuild_illustration_cache(archive_path).map_err(|e| e.to_string())?;
    let cache = illustration_store::cache_path(archive_path);
    let body = match std::fs::read(&cache) {
        Ok(body) => body,
        // エントリ 0 件では flush がファイルを書かない (空の索引を表す)。
        Err(_) => illustration_index_empty_yaml(),
    };
    let Some(prefix) =
        narou_rs::native::object_store::novel_object_prefix(&archive_root, archive_path)
    else {
        return Err("索引の保存先を解決できません".to_string());
    };
    let key = prefix.join(".illustration_cache.yaml").map_err(|e| e.to_string())?;
    let stores = native_stores()?;
    futures::executor::block_on(narou_rs::platform::ObjectStore::write_small(
        stores.objects.as_ref(),
        &key,
        body,
    ))
    .map_err(|e| e.to_string())?;
    let narou_dir = inventory.root_dir().join(".narou");
    if narou_rs::native::sqlite::state::active_for(&narou_dir).is_some()
        && !narou_rs::native::sqlite::state::mirror_files_enabled()
    {
        // mirror-files=false では索引はストアにだけ置く。
        let _ = std::fs::remove_file(&cache);
    }
    println!("  cache 再構築完了: {} エントリ (ストア保存)", count);
    Ok(())
}

/// 空の索引 YAML (`IllustrationIndex::to_cache_yaml` と同じ形式)。
fn illustration_index_empty_yaml() -> Vec<u8> {
    narou_rs::illustration_store::IllustrationIndex::default()
        .to_cache_yaml()
        .unwrap_or_else(|_| b"version: 2\n".to_vec())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn unique_tempdir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "narou-rs-illust-{}-{}-{}",
            prefix,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn detect_image_extension_recognises_common_formats() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00];
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let gif = [0x47, 0x49, 0x46, 0x38, 0x39, 0x61];
        let webp = [
            0x52, 0x49, 0x46, 0x46, 0x00, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50,
        ];
        let bmp = [0x42, 0x4D, 0x00, 0x00];
        assert_eq!(
            illustration_store::detect_image_extension(&jpeg),
            Some("jpg")
        );
        assert_eq!(
            illustration_store::detect_image_extension(&png),
            Some("png")
        );
        assert_eq!(
            illustration_store::detect_image_extension(&gif),
            Some("gif")
        );
        assert_eq!(
            illustration_store::detect_image_extension(&webp),
            Some("webp")
        );
        assert_eq!(
            illustration_store::detect_image_extension(&bmp),
            Some("bmp")
        );
        assert_eq!(
            illustration_store::detect_image_extension(b"plain text"),
            None
        );
    }

    #[test]
    fn plan_extension_fixes_flags_mismatched_extension() {
        let dir = unique_tempdir("plan-ext");
        let illust_dir = dir.join("挿絵");
        std::fs::create_dir_all(&illust_dir).unwrap();
        let png_bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
        std::fs::write(illust_dir.join("cover.jpg"), png_bytes).unwrap();
        std::fs::write(illust_dir.join("ok.png"), png_bytes).unwrap();

        let plans = illustration_store::plan_extension_fixes(&illust_dir).unwrap();

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].0.file_name().unwrap(), "cover.jpg");
        assert_eq!(plans[0].1.file_name().unwrap(), "cover.png");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_legacy_migrations_returns_section_index_and_url_basename() {
        let dir = unique_tempdir("plan-mig");
        let illust_dir = dir.join("挿絵");
        let raw_dir = dir.join("raw");
        std::fs::create_dir_all(&illust_dir).unwrap();
        std::fs::create_dir_all(&raw_dir).unwrap();
        // PNG bytes for both legacy files.
        let bytes: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xAA, 0xBB];
        std::fs::write(illust_dir.join("16-0.jpg"), bytes).unwrap();
        std::fs::write(illust_dir.join("image001.jpg"), bytes).unwrap();
        // Provide a raw HTML reference whose legacy basename is image001.
        std::fs::write(
            raw_dir.join("16 subtitle.html"),
            r#"<p><img src="https://example.com/path/image001.jpg" /></p>"#,
        )
        .unwrap();

        let plans = illustration_store::plan_legacy_illustration_migrations(&dir).unwrap();

        let stems: Vec<String> = plans
            .iter()
            .map(|p| {
                p.old_path
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert!(stems.contains(&"16-0".to_string()));
        assert!(stems.contains(&"image001".to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_orphan_illustrations_returns_only_unreachable_files() {
        let dir = unique_tempdir("orphan");
        let illust_dir = dir.join("挿絵");
        std::fs::create_dir_all(&illust_dir).unwrap();
        let bytes: &[u8] = b"dummy";
        let hash = illustration_store::hash_bytes(bytes);
        let hash_filename = format!("{}.jpg", hash);
        std::fs::write(illust_dir.join(&hash_filename), bytes).unwrap();
        std::fs::write(illust_dir.join("orphan.jpg"), bytes).unwrap();

        let orphans = illustration_store::find_orphan_illustrations(&dir).unwrap();
        let orphan_names: Vec<String> = orphans
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert_eq!(orphan_names, vec!["orphan.jpg".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
