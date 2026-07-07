use std::path::{Path, PathBuf};

use narou_rs::db;
use narou_rs::db::paths::novel_dir_for_record;
use narou_rs::illustration_store;

use super::download;
use super::log;

/// Subcommand for the `narou illust` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IllustSubcommand {
    Orphan,
    Migrate,
    FixExt,
    Rebuild,
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
        }
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
    db::with_database(|db| {
        let archive_root = db.archive_root().to_path_buf();
        let mut dirs = Vec::new();
        for record in db.all_records().values() {
            if narou_rs::compat::record_is_frozen(record, &frozen_ids) {
                continue;
            }
            dirs.push(novel_dir_for_record(&archive_root, record));
        }
        Ok::<Vec<PathBuf>, narou_rs::error::NarouError>(dirs)
    })
    .map_err(|e| e.to_string())
}

fn resolve_novel_dir(target: &str) -> Option<PathBuf> {
    let id = super::resolve_target_to_id(target)?;
    db::with_database(|db| {
        let archive_root = db.archive_root().to_path_buf();
        Ok(db
            .get(id)
            .map(|record| novel_dir_for_record(&archive_root, record)))
    })
    .ok()
    .flatten()
}

fn run_orphan(archive_path: &Path, force: bool) -> Result<(), String> {
    let orphans = illustration_store::find_orphan_illustrations(archive_path)
        .map_err(|e| e.to_string())?;
    if orphans.is_empty() {
        println!("  孤児挿絵はありません。");
        return Ok(());
    }
    println!("  孤児挿絵: {} 件", orphans.len());
    for path in &orphans {
        if force {
            if let Err(err) = std::fs::remove_file(path) {
                log::report_error(&format!(
                    "{} の削除に失敗しました: {}",
                    path.display(),
                    err
                ));
            } else {
                println!("  [削除] {}", path.display());
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
    let renamed =
        illustration_store::apply_legacy_illustration_migrations(archive_path)
            .map_err(|e| e.to_string())?;
    println!("  移行完了: {} 件", renamed);
    Ok(())
}

fn run_fix_ext(archive_path: &Path, force: bool) -> Result<(), String> {
    let illust_dir = archive_path.join("挿絵");
    let plans = illustration_store::plan_extension_fixes(&illust_dir)
        .map_err(|e| e.to_string())?;
    if plans.is_empty() {
        println!("  拡張子の修正対象はありません。");
        return Ok(());
    }
    println!("  拡張子の修正対象: {} 件", plans.len());
    for (old_path, new_path) in &plans {
        if force {
            println!(
                "  [予定] {} -> {}",
                old_path.display(),
                new_path.display()
            );
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
        assert_eq!(illustration_store::detect_image_extension(&jpeg), Some("jpg"));
        assert_eq!(illustration_store::detect_image_extension(&png), Some("png"));
        assert_eq!(illustration_store::detect_image_extension(&gif), Some("gif"));
        assert_eq!(illustration_store::detect_image_extension(&webp), Some("webp"));
        assert_eq!(illustration_store::detect_image_extension(&bmp), Some("bmp"));
        assert_eq!(illustration_store::detect_image_extension(b"plain text"), None);
    }

    #[test]
    fn plan_extension_fixes_flags_mismatched_extension() {
        let dir = unique_tempdir("plan-ext");
        let illust_dir = dir.join("挿絵");
        std::fs::create_dir_all(&illust_dir).unwrap();
        let png_bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
        std::fs::write(illust_dir.join("cover.jpg"), &png_bytes).unwrap();
        std::fs::write(illust_dir.join("ok.png"), &png_bytes).unwrap();

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