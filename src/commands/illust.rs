use std::path::Path;

use narou_rs::db;
use narou_rs::application::novel_access::NovelWriteGuard;
use narou_rs::illustration_store;
use narou_rs::platform::StoreMigrationState;

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
pub async fn cmd_illust(sub: IllustSubcommand, targets: &[String], force: bool, all: bool) -> i32 {
    match cmd_illust_inner(sub, targets, force, all).await {
        Ok(()) => 0,
        Err(err) => {
            log::report_error(&err);
            1
        }
    }
}

async fn cmd_illust_inner(
    sub: IllustSubcommand,
    targets: &[String],
    force: bool,
    all: bool,
) -> Result<(), String> {
    db::init_database().map_err(|e| e.to_string())?;

    // S3 への移行・突き合わせ・dedup は小説単位ではなくライブラリ全体の操作。
    match sub {
        IllustSubcommand::S3Push => return run_s3_push(force).await,
        IllustSubcommand::S3Verify => return run_s3_verify().await,
        IllustSubcommand::S3Dedup => return run_s3_dedup(force).await,
        _ => {}
    }

    let origins = resolve_target_origins(targets, all)?;

    if origins.is_empty() {
        println!("対象の小説が見つかりませんでした。");
        return Ok(());
    }

    let total = origins.len();
    for (idx, write) in origins.into_iter().enumerate() {
        if total > 1 {
            println!("---");
        }
        let dir = narou_rs::native::materialization::directory_for_origin(&write).map_err(|error| error.to_string())?;
        println!("[{} / {}] {}", idx + 1, total, dir.display());
        let snapshot = narou_rs::native::materialization::NativeMutationSnapshot::begin_with_origin(&dir, write)
            .map_err(|error| error.to_string())?;
        match sub {
            IllustSubcommand::Orphan => run_orphan(snapshot.path(), force)?,
            IllustSubcommand::Migrate => run_migrate(snapshot.path(), force)?,
            IllustSubcommand::FixExt => run_fix_ext(snapshot.path(), force)?,
            IllustSubcommand::Rebuild => run_rebuild(snapshot.path())?,
            // ライブラリ全体の操作なので、この小説単位のループには来ない
            // (手前で return する)。
            IllustSubcommand::S3Push | IllustSubcommand::S3Verify | IllustSubcommand::S3Dedup => {
                unreachable!("S3 subcommands run before the per-novel loop")
            }
        }
        if force || sub == IllustSubcommand::Rebuild {
            snapshot.publish_changes().map_err(|error| error.to_string())?;
        }
    }
    Ok(())
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
async fn run_s3_push(force: bool) -> Result<(), String> {
    let action = if force { "copy" } else { "plan" };
    let state = narou_rs::native::illustrations::migrate_illustrations(action).await.map_err(|error| error.to_string())?;
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
async fn run_s3_verify() -> Result<(), String> {
    let state = narou_rs::native::illustrations::migrate_illustrations("verify").await.map_err(|error| error.to_string())?;
    println!("一致: {} 件", state.verified);
    report_migration_failures(&state);
    Ok(())
}
/// 旧 `挿絵/` 配置の S3 オブジェクトを dedup プールへ寄せる。
///
/// 手順: 全小説の `挿絵/` を LIST → hex 名のものを pool へ `copy` →
/// `pool_exists` で確認後、legacy の実オブジェクトを `delete`。
/// 既定は dry-run (pool へ写さず件数だけ数える)。
async fn run_s3_dedup(force: bool) -> Result<(), String> {
    let action = if force { "copy" } else { "plan" };
    let state = narou_rs::native::illustrations::deduplicate_illustrations(action).await.map_err(|error| error.to_string())?;

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

fn resolve_target_origins(targets: &[String], all: bool) -> Result<Vec<NovelWriteGuard>, String> {
    if all { return collect_all_novel_origins(); }
    let targets = if targets.is_empty() {
        super::latest_convert_target().into_iter().collect()
    } else {
        download::tagname_to_ids(targets)
    };
    let mut origins = Vec::new();
    for target in targets {
        match resolve_novel_origin(&target)? {
            Some(write) => origins.push(write),
            None => log::report_error(&format!("{} は存在しません", target)),
        }
    }
    Ok(origins)
}

fn collect_all_novel_origins() -> Result<Vec<NovelWriteGuard>, String> {
    let frozen_ids = narou_rs::compat::load_frozen_ids().map_err(|error| error.to_string())?;
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let ids = novels.scan_ids_sync(&narou_rs::platform::NovelFilter::all(), None, usize::MAX).map_err(|error| error.to_string())?;
    let mut origins = Vec::new();
    for record in novels.get_many_current_sync(&ids).map_err(|error| error.to_string())?.into_iter().flatten() {
        if narou_rs::compat::record_is_frozen(&record, &frozen_ids) { continue; }
        origins.push(narou_rs::native::materialization::capture_write_for_record(&record).map_err(|error| error.to_string())?);
    }
    Ok(origins)
}

fn resolve_novel_origin(target: &str) -> Result<Option<NovelWriteGuard>, String> {
    let Some(id) = super::resolve_target_to_id(target) else { return Ok(None); };
    narou_rs::native::materialization::capture_write_for_id(id).map(Some).map_err(|error| error.to_string())
}

fn run_orphan(archive_path: &Path, force: bool) -> Result<(), String> {
    let orphans =
        illustration_store::find_orphan_illustrations(archive_path).map_err(|e| e.to_string())?;
    if orphans.is_empty() {
        println!("  孤児挿絵はありません。");
        return Ok(());
    }
    println!("  孤児挿絵: {} 件", orphans.len());
    for path in &orphans {
        if force {
            match std::fs::remove_file(path).map_err(|error| error.to_string()) {
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



fn run_migrate(archive_path: &Path, force: bool) -> Result<(), String> {
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
    println!("  移行完了: {} 件", renamed);
    Ok(())
}

fn run_fix_ext(archive_path: &Path, force: bool) -> Result<(), String> {
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
    println!("  改名完了: {} 件", renamed);
    Ok(())
}


fn run_rebuild(archive_path: &Path) -> Result<(), String> {
    let count =
        illustration_store::rebuild_illustration_cache(archive_path).map_err(|e| e.to_string())?;
    println!("  cache 再構築完了: {} エントリ", count);
    Ok(())
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
