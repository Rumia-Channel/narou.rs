//! `narou db` — maintenance operations for the SQLite management database.
//!
//! - `verify`: run `PRAGMA integrity_check` (SQLite) / YAML re-parse (legacy)
//! - `export-yaml`: regenerate the legacy file bundle for rollback to
//!   pre-P2 versions (backward-compatibility escape hatch)
//! - `vacuum`: reclaim space after history pruning

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
    },
    /// Rebuild the database file to reclaim free space.
    Vacuum,
}

pub fn cmd_db(action: DbAction) -> narou_rs::error::Result<()> {
    // Every action operates on the live handle; this also triggers the
    // legacy import on first run.
    narou_rs::db::init_database()?;
    match action {
        DbAction::Verify => cmd_verify(),
        DbAction::ExportYaml { out } => cmd_export_yaml(out),
        DbAction::Vacuum => cmd_vacuum(),
    }
}

fn cmd_verify() -> narou_rs::error::Result<()> {
    match narou_rs::db::with_database(|db| db.sqlite_integrity())? {
        Some(report) => println!("integrity: {report}"),
        None => println!("レガシーYAMLモードのため整合性チェックをスキップしました"),
    }
    Ok(())
}

fn cmd_export_yaml(out: Option<String>) -> narou_rs::error::Result<()> {
    use std::collections::BTreeMap;

    let inventory = narou_rs::db::inventory::Inventory::with_default_root()?;
    let root = inventory.root_dir().to_path_buf();
    let out_dir = out
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("narou-yaml-export"));
    std::fs::create_dir_all(&out_dir)?;

    // 1. database.yaml from live records.
    let records: BTreeMap<i64, narou_rs::db::NovelRecord> = narou_rs::db::with_database(|db| {
        Ok(db
            .all_records()
            .iter()
            .map(|(&id, record)| {
                let mut normalized = record.clone();
                normalized.id = id;
                (id, normalized)
            })
            .collect())
    })?;
    let yaml = serde_yaml::to_string(&records)?;
    std::fs::write(out_dir.join("database.yaml"), yaml)?;

    // 2. Management states verbatim from app_state (freeze, alias, ...).
    #[cfg(feature = "native-runtime")]
    if let Some(state) = (|| {
        let narou_dir = narou_rs::db::inventory::Inventory::with_default_root()
            .ok()?
            .root_dir()
            .join(".narou");
        narou_rs::native::sqlite::state::active_for(&narou_dir)
    })() {
        for entry in [
            ("freeze", "freeze.yaml"),
            ("alias", "alias.yaml"),
            ("tag_colors", "tag_colors.yaml"),
            ("latest_convert", "latest_convert.yaml"),
            ("local_setting", "local_setting.yaml"),
            ("queue", "queue.yaml"),
            ("notepad", "notepad.txt"),
        ] {
            if let Some(payload) = state.get_raw("inv", entry.0)? {
                std::fs::write(out_dir.join(entry.1), payload)?;
            }
        }
        if let Some(payload) = state.get_raw("global", "global_setting")? {
            let global_dir = out_dir.join(".narousetting");
            std::fs::create_dir_all(&global_dir)?;
            std::fs::write(global_dir.join("global_setting.yaml"), payload)?;
        }
    }

    println!("エクスポートしました: {}", out_dir.display());
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
