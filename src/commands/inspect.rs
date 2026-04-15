use std::fs;
use std::path::{Path, PathBuf};

use narou_rs::converter::inspector::INSPECT_LOG_NAME;
use narou_rs::db;

use super::download;
use super::log;

const HR_TEXT: &str = "―――――――――――――――――――――――――――――――――――";

pub fn cmd_inspect(targets: &[String]) -> i32 {
    match cmd_inspect_inner(targets) {
        Ok(()) => 0,
        Err(err) => {
            log::report_error(&err);
            1
        }
    }
}

fn cmd_inspect_inner(targets: &[String]) -> Result<(), String> {
    db::init_database().map_err(|e| e.to_string())?;

    if targets.is_empty() {
        let Some(target) = super::latest_convert_target() else {
            return Ok(());
        };
        let Some(entry) = resolve_inspect_entry(&target) else {
            return Ok(());
        };
        display_log(&entry);
        return Ok(());
    }

    let expanded = download::tagname_to_ids(targets);
    for (i, target) in expanded.iter().enumerate() {
        if i > 0 {
            println!("{}", HR_TEXT);
        }

        let Some(entry) = resolve_inspect_entry(target) else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };
        display_log(&entry);
    }

    Ok(())
}

struct InspectEntry {
    title: String,
    archive_path: PathBuf,
}

fn resolve_inspect_entry(target: &str) -> Option<InspectEntry> {
    let id = super::resolve_target_to_id(target)?;
    db::with_database(|db| {
        Ok(db.get(id).map(|record| InspectEntry {
            title: record.title.clone(),
            archive_path: narou_rs::db::existing_novel_dir_for_record(db.archive_root(), record),
        }))
    })
    .ok()
    .flatten()
}

fn display_log(entry: &InspectEntry) {
    println!("({} の小説状態調査状況ログ)", entry.title);
    match read_inspect_log(&entry.archive_path) {
        Some(content) => {
            print!("{}", content);
            if !content.ends_with('\n') {
                println!();
            }
        }
        None => println!("調査ログがまだ無いようです"),
    }
}

fn read_inspect_log(archive_path: &Path) -> Option<String> {
    let path = archive_path.join(INSPECT_LOG_NAME);
    fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::read_inspect_log;

    #[test]
    fn read_inspect_log_returns_file_content() {
        let base = std::env::temp_dir().join(format!(
            "narou-rs-inspect-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join(super::INSPECT_LOG_NAME), "sample log\n").unwrap();

        assert_eq!(read_inspect_log(&base).as_deref(), Some("sample log\n"));

        std::fs::remove_dir_all(base).unwrap();
    }
}
