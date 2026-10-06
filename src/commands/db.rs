//! `narou db` — maintenance operations for the SQLite management database.
//!
//! - `verify`: run `PRAGMA integrity_check` (SQLite) / YAML re-parse (legacy)
//! - `export-yaml`: regenerate the legacy file bundle for rollback to
//!   pre-P2 versions (backward-compatibility escape hatch)
//! - `vacuum`: reclaim space after history pruning
//! - `repair-freeze`: restore freeze state from `freeze.yaml.imported-*`
//!   fragments written by builds before the freeze-state fix (issue #35)

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum DbAction {
    /// Check database integrity.
    Verify,
    /// Export the current state as the legacy YAML/TXT bundle.
    ExportYaml {
        /// Output directory (default: `narou-yaml-export` under the archive root).
        #[arg(long)]
        out: Option<String>,
        /// Write the files back to their real locations (`.narou/*.yaml`,
        /// `~/.narousetting/global_setting.yaml`) and switch the storage
        /// marker back to `yaml`, restoring full narou.rb compatibility.
        #[arg(long)]
        in_place: bool,
    },
    /// Rebuild the database file to reclaim free space.
    Vacuum,
    /// Restore frozen novels that older builds lost to `freeze.yaml.imported-*`
    /// fragments (union of every fragment, never unfreezes anything).
    RepairFreeze {
        /// Only report what would be restored.
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn cmd_db(action: DbAction) -> narou_rs::error::Result<()> {
    // Every action operates on the live handle; this also triggers the
    // legacy import on first run.
    narou_rs::db::init_database()?;
    match action {
        DbAction::Verify => cmd_verify(),
        DbAction::ExportYaml { out, in_place } => cmd_export_yaml(out, in_place),
        DbAction::Vacuum => cmd_vacuum(),
        DbAction::RepairFreeze { dry_run } => cmd_repair_freeze(dry_run),
    }
}

fn cmd_verify() -> narou_rs::error::Result<()> {
    match narou_rs::db::with_database(|db| db.sqlite_integrity())? {
        Some(report) => println!("integrity: {report}"),
        None => println!("レガシーYAMLモードのため整合性チェックをスキップしました"),
    }
    // Payload-level check: every objects/section_bodies row's stored CRC-32
    // must match its decompressed payload. PRAGMA integrity_check covers
    // B-tree structure, not application-level corruption.
    match narou_rs::db::with_database(|db| db.sqlite_payload_check())? {
        Some(0) => println!("payloads: ok"),
        Some(bad) => println!("payloads: {bad} corrupted"),
        None => {}
    }
    // Freeze fragments written by older builds still hold ids the stored
    // payload may have lost (issue #35); point at the repair command.
    if let Ok(inventory) = narou_rs::db::inventory::Inventory::with_default_root() {
        let narou_dir = inventory.root_dir().join(".narou");
        let fragments =
            narou_rs::native::sqlite::freeze_repair::imported_freeze_files(&narou_dir).len();
        if fragments > 0 {
            println!(
                "freeze: 取り込み済み freeze.yaml が {fragments} 件あります（`narou db repair-freeze` で復旧できます）"
            );
        }
    }
    Ok(())
}

fn cmd_export_yaml(out: Option<String>, in_place: bool) -> narou_rs::error::Result<()> {
    let dir = narou_rs::native::sqlite::export_yaml::export_yaml(
        out.map(std::path::PathBuf::from),
        in_place,
    )?;
    if in_place {
        println!("前方互換ファイルを .narou/ に書き戻し、YAML モードへ切り替えました");
    } else {
        println!("エクスポートしました: {}", dir.display());
    }
    Ok(())
}

fn cmd_vacuum() -> narou_rs::error::Result<()> {
    #[cfg(feature = "native-runtime")]
    if !narou_rs::native::sqlite::state::legacy_yaml_active() {
        let narou_dir = narou_rs::db::inventory::Inventory::with_default_root()?
            .root_dir()
            .join(".narou");
        if let Some(state) = narou_rs::native::sqlite::state::active_for(&narou_dir) {
            let conn = state.conn();
            let guard = conn.lock().expect("sqlite mutex poisoned");
            guard.execute_batch("VACUUM").map_err(|error| {
                narou_rs::error::NarouError::Database(format!("VACUUM failed: {error}"))
            })?;
            drop(guard);
            println!("最適化しました");
            return Ok(());
        }
    }
    println!("SQLite バックエンドが無効のため、最適化は不要です");
    Ok(())
}

/// Restore freeze state that older builds scattered across
/// `freeze.yaml.imported-*` fragments (issue #35).
///
/// The stored payload is the source of truth, so the command only ever *adds*
/// the ids found in the fragments; it never unfreezes a novel. That is the
/// intended direction: the old builds could only lose frozen ids, and a novel
/// that was unfrozen on purpose can be released again with `narou freeze --off`.
fn cmd_repair_freeze(dry_run: bool) -> narou_rs::error::Result<()> {
    use narou_rs::native::sqlite::freeze_repair;

    let inventory = narou_rs::db::inventory::Inventory::with_default_root()?;
    let plan = freeze_repair::plan(&inventory)?;
    if plan.files == 0 {
        println!("freeze.yaml.imported-* が見つかりません（修復の必要はありません）");
        return Ok(());
    }
    println!("取り込み済み freeze.yaml: {} 件", plan.files);
    if plan.unreadable > 0 {
        println!("  読み取れなかったファイル: {} 件", plan.unreadable);
    }
    if plan.is_empty() {
        println!("凍結状態は最新です（修復対象なし）");
        return Ok(());
    }

    let titles = freeze_titles(&plan.missing_ids);
    if dry_run {
        println!("復旧予定: {} 件", plan.missing_ids.len());
        for (id, title) in plan.missing_ids.iter().zip(&titles) {
            println!("  {id}: {title}");
        }
        println!("--dry-run のため変更していません");
        return Ok(());
    }

    freeze_repair::apply(&inventory, &plan.missing_ids)?;
    println!("{} 件の凍結状態を復旧しました", plan.missing_ids.len());
    for (id, title) in plan.missing_ids.iter().zip(&titles) {
        println!("  {id}: {title}");
    }
    Ok(())
}

/// Titles for the repaired ids, `"<不明>"` for records that no longer exist.
fn freeze_titles(ids: &[i64]) -> Vec<String> {
    narou_rs::db::with_database(|db| {
        Ok(ids
            .iter()
            .map(|id| {
                db.get(*id)
                    .map(|record| record.title.clone())
                    .unwrap_or_else(|| "<不明>".to_string())
            })
            .collect::<Vec<_>>())
    })
    .unwrap_or_else(|_| ids.iter().map(|_| "<不明>".to_string()).collect())
}
