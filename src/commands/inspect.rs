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
        let Some(entry) = resolve_inspect_entry(&target)? else {
            return Ok(());
        };
        display_log(&entry)?;
        return Ok(());
    }

    let expanded = download::tagname_to_ids(targets);
    for (i, target) in expanded.iter().enumerate() {
        if i > 0 {
            println!("{}", HR_TEXT);
        }

        let Some(entry) = resolve_inspect_entry(target)? else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };
        display_log(&entry)?;
    }

    Ok(())
}

struct InspectEntry {
    id: i64,
    keys: narou_rs::platform::NovelObjectKeys,
}

fn resolve_inspect_entry(target: &str) -> Result<Option<InspectEntry>, String> {
    let Some(id) = super::resolve_target_to_id(target) else { return Ok(None); };
    let record = narou_rs::native::novel_repository::NativeNovelRepository::new().get_current_sync(id.into()).map_err(|error| error.to_string())?;
    let Some(record) = record else { return Ok(None); };
    let keys = narou_rs::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory).map_err(|error| error.to_string())?;
    Ok(Some(InspectEntry { id, keys }))
}

fn display_log(entry: &InspectEntry) -> Result<(), String> {
    match read_inspect_log(entry) {
        Ok((title, Some(content))) => {
            println!("({title} の小説状態調査状況ログ)");
            print!("{}", content);
            if !content.ends_with('\n') {
                println!();
            }
        }
        Ok((title, None)) => println!("({title} の小説状態調査状況ログ)\n調査ログがまだ無いようです"),
        Err(error) => return Err(error.to_string()),
    }
    Ok(())
}

fn read_inspect_log(entry: &InspectEntry) -> narou_rs::error::Result<(String, Option<String>)> {
    let key = entry.keys.prefix().join(INSPECT_LOG_NAME)?;
    let prefix = entry.keys.prefix().clone();
    let inventory = narou_rs::db::inventory::Inventory::with_default_root()?;
    let store = narou_rs::native::materialization::read_store(inventory.root_dir())?;
    futures::executor::block_on(async {
        let mut lease = narou_rs::application::novel_access::NovelReadLease::admit(
            &store, &narou_rs::platform::ObjectPrefix::from(prefix.clone()), "native-inspect",
        ).await?;
        let result = async {
            let record = narou_rs::native::novel_repository::NativeNovelRepository::new().get_current_sync(entry.id.into())?
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", entry.id)))?;
            let keys = narou_rs::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
            if keys.prefix() != &prefix { return Err(narou_rs::error::NarouError::Platform("novel_busy: inspection prefix changed".into())); }
            let bytes = lease.read_small(&key).await?;
            lease.validate().await?;
            let content = bytes.map(|bytes| String::from_utf8(bytes).map_err(|error| {
                narou_rs::error::NarouError::Database(format!("調査ログを読めません: {error}"))
            })).transpose()?;
            Ok((record.title, content))
        }.await;
        let released = lease.release().await;
        match result { Ok(value) => { released?; Ok(value) }, Err(error) => Err(error) }
    })
}
