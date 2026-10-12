use std::path::PathBuf;

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
    archive_root: PathBuf,
}

fn resolve_inspect_entry(target: &str) -> Option<InspectEntry> {
    let id = super::resolve_target_to_id(target)?;
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let record = novels.get_sync(id.into()).ok().flatten()?;
    let archive_root = narou_rs::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
        .unwrap_or_else(|_| PathBuf::from(narou_rs::downloader::ARCHIVE_ROOT_DIR));
    Some(InspectEntry {
        title: record.title.clone(),
        archive_path: narou_rs::db::existing_novel_dir_for_record(&archive_root, &record),
        archive_root,
    })
}

fn display_log(entry: &InspectEntry) {
    println!("({} の小説状態調査状況ログ)", entry.title);
    match read_inspect_log(entry) {
        Ok(Some(content)) => {
            print!("{}", content);
            if !content.ends_with('\n') {
                println!();
            }
        }
        Ok(None) => println!("調査ログがまだ無いようです"),
        Err(error) => log::report_error(&error.to_string()),
    }
}

fn read_inspect_log(entry: &InspectEntry) -> narou_rs::error::Result<Option<String>> {
    let path = entry.archive_path.join(INSPECT_LOG_NAME);
    let key = narou_rs::native::object_store::object_key_for_file(&entry.archive_root, &path)
        .ok_or_else(|| narou_rs::error::NarouError::Database(format!("調査ログが保存フォルダの外です: {}", path.display())))?;
    let store = narou_rs::native::object_store::NativeStore::for_current_root()?;
    let bytes = futures::executor::block_on(narou_rs::platform::ObjectStore::read_small(&store, &key))?;
    bytes.map(|bytes| String::from_utf8(bytes).map_err(|error| {
        narou_rs::error::NarouError::Database(format!("調査ログを読めません: {error}"))
    })).transpose()
}
