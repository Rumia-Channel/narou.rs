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
        /// Write the files back to their real locations (`.narou/*.yaml`,
        /// `~/.narousetting/global_setting.yaml`) and switch the storage
        /// marker back to `yaml`, restoring full narou.rb compatibility.
        #[arg(long)]
        in_place: bool,
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
        DbAction::ExportYaml { out, in_place } => cmd_export_yaml(out, in_place),
        DbAction::Vacuum => cmd_vacuum(),
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
