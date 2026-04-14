use std::path::PathBuf;

use narou_rs::compat::open_directory;
use narou_rs::db;
use narou_rs::db::paths::novel_dir_for_record;

use super::download;
use super::help;
use super::log;

pub fn cmd_folder(targets: &[String], no_open: bool) -> i32 {
    match cmd_folder_inner(targets, no_open) {
        Ok(()) => 0,
        Err(err) => {
            log::report_error(&err);
            1
        }
    }
}

fn cmd_folder_inner(targets: &[String], no_open: bool) -> Result<(), String> {
    db::init_database().map_err(|e| e.to_string())?;

    if targets.is_empty() {
        help::display_command_help("folder");
        return Ok(());
    }

    let expanded = download::tagname_to_ids(targets);
    for target in expanded {
        let Some(dir) = resolve_novel_dir(&target) else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };

        if no_open {
            println!("{}", dir.display());
        } else {
            open_directory(&dir, None);
        }
    }

    Ok(())
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
