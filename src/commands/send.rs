use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use narou_rs::compat::{load_local_setting_bool, load_local_setting_string};
use narou_rs::converter::device::{Device, OutputManager};
use narou_rs::db;
use narou_rs::db::NovelRecord;
use narou_rs::db::inventory::{Inventory, InventoryScope};
use narou_rs::db::paths::existing_novel_dir_for_record;
use narou_rs::mail::{get_ebook_file_paths, newest_hotentry_file_path};

use super::download;

const SEND_DEVICE_NAMES: &[&str] = &["kindle", "kobo", "epub", "ibunko", "reader", "ibooks"];

pub struct SendOptions {
    pub args: Vec<String>,
    pub without_freeze: bool,
    pub force: bool,
    pub backup_bookmark: bool,
    pub restore_bookmark: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendDevice {
    Kindle,
    Kobo,
    Epub,
    Ibunko,
    Reader,
    Ibooks,
}

impl SendDevice {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "kindle" => Some(Self::Kindle),
            "kobo" => Some(Self::Kobo),
            "epub" => Some(Self::Epub),
            "ibunko" => Some(Self::Ibunko),
            "reader" => Some(Self::Reader),
            "ibooks" => Some(Self::Ibooks),
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Kindle => "Kindle",
            Self::Kobo => "Kobo",
            Self::Epub => "EPUB",
            Self::Ibunko => "iBunko",
            Self::Reader => "Reader",
            Self::Ibooks => "iBooks",
        }
    }

    fn display_name(&self) -> &'static str {
        match self {
            Self::Reader => "SonyReader",
            Self::Ibunko => "i文庫",
            _ => self.name(),
        }
    }

    fn ebook_file_ext(&self) -> &'static str {
        match self {
            Self::Kindle => ".mobi",
            Self::Kobo => ".kepub.epub",
            Self::Epub | Self::Reader | Self::Ibooks => ".epub",
            Self::Ibunko => ".zip",
        }
    }

    fn physical_support(&self) -> bool {
        matches!(self, Self::Kindle | Self::Kobo | Self::Reader)
    }

    fn manager_device(&self) -> Option<Device> {
        match self {
            Self::Kindle => Some(Device::Mobi),
            Self::Kobo => Some(Device::Kobo),
            Self::Epub => Some(Device::Epub),
            Self::Reader => Some(Device::Reader),
            Self::Ibunko | Self::Ibooks => None,
        }
    }

    fn manager(&self) -> Option<OutputManager> {
        self.manager_device().map(OutputManager::new)
    }

    fn bookmark_backup_supported(&self) -> bool {
        matches!(self, Self::Kindle)
    }
}

pub fn cmd_send(opts: SendOptions) -> i32 {
    match cmd_send_inner(opts) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("{}", message);
            127
        }
    }
}

fn cmd_send_inner(opts: SendOptions) -> Result<(), String> {
    db::init_database().map_err(|e| e.to_string())?;

    let (device, raw_targets) = resolve_device_and_targets(&opts.args)?;
    if !device.physical_support() {
        return Err(format!(
            "{} への直接送信は対応していません",
            device.display_name()
        ));
    }

    let manager = device
        .manager()
        .ok_or_else(|| "送信に使う端末設定が不正です".to_string())?;
    if !manager.connecting() {
        return Err(format!("{} が接続されていません", device.display_name()));
    }

    let hotentry_enabled = load_local_setting_bool("hotentry");
    let send_all = raw_targets.is_empty();
    let (targets, titles) = if send_all {
        collect_all_targets(hotentry_enabled)?
    } else {
        (raw_targets, HashMap::new())
    };

    if opts.backup_bookmark {
        process_backup_bookmark(device, &manager);
        return Ok(());
    }
    if opts.restore_bookmark {
        process_restore_bookmark(device, &manager);
        return Ok(());
    }

    let without_freeze = opts.without_freeze || load_local_setting_bool("send.without-freeze");
    let auto_backup_bookmark = load_local_setting_bool("send.backup-bookmark");
    let frozen_ids = load_frozen_ids()?;

    for target in download::tagname_to_ids(&targets) {
        if without_freeze && is_target_frozen(&target, &frozen_ids) {
            continue;
        }

        let (display_target, ebook_paths) = match resolve_send_target(&target, device, &titles)? {
            Some(data) => data,
            None => continue,
        };

        let Some(first_path) = ebook_paths.first() else {
            eprintln!("{} は存在しません", target);
            continue;
        };
        if !first_path.exists() {
            if !send_all {
                eprintln!(
                    "まだファイル({})が無いようです",
                    first_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default()
                );
            }
            continue;
        }

        if !opts.force && !manager.ebook_file_old(first_path) {
            continue;
        }

        println!("{}", highlight_target(&display_target));
        for ebook_path in ebook_paths {
            let copied_to = copy_with_progress(device, &ebook_path)?;
            match copied_to {
                Some(path) => println!("{} へコピーしました", path.display()),
                None => {
                    return Err(format!(
                        "{}が見つからなかったためコピー出来ませんでした",
                        device.name()
                    ));
                }
            }
        }
    }

    if send_all && auto_backup_bookmark {
        process_backup_bookmark(device, &manager);
    }

    Ok(())
}

fn resolve_device_and_targets(args: &[String]) -> Result<(SendDevice, Vec<String>), String> {
    let mut targets = args.to_vec();
    if let Some(first) = targets.first()
        && let Some(device) = SendDevice::parse(first)
    {
        targets.remove(0);
        return Ok((device, targets));
    }

    if let Some(raw) = load_local_setting_string("device")
        && let Some(device) = SendDevice::parse(&raw)
    {
        return Ok((device, targets));
    }

    Err(format!(
        "デバイス名が指定されていないか、間違っています。\n\
narou setting device=デバイス名 で指定出来ます。\n\
指定出来るデバイス名：{}",
        SEND_DEVICE_NAMES.join(", ")
    ))
}

fn collect_all_targets(
    hotentry_enabled: bool,
) -> Result<(Vec<String>, HashMap<String, String>), String> {
    db::with_database(|db| {
        let mut ids = db.ids();
        ids.sort_unstable();

        let mut targets = Vec::with_capacity(ids.len() + usize::from(hotentry_enabled));
        let mut titles = HashMap::new();
        for id in ids {
            let key = id.to_string();
            targets.push(key.clone());
            if let Some(record) = db.get(id) {
                titles.insert(key, record.title.clone());
            }
        }
        if hotentry_enabled {
            targets.push("hotentry".to_string());
            titles.insert("hotentry".to_string(), "hotentry".to_string());
        }
        Ok((targets, titles))
    })
    .map_err(|e| e.to_string())
}

fn load_frozen_ids() -> Result<HashSet<i64>, String> {
    db::with_database(|db| {
        let freeze_map: HashMap<i64, serde_yaml::Value> =
            db.inventory().load("freeze", InventoryScope::Local)?;
        Ok(freeze_map.into_keys().collect::<HashSet<_>>())
    })
    .map_err(|e| e.to_string())
}

fn is_target_frozen(target: &str, frozen_ids: &HashSet<i64>) -> bool {
    download::get_data_by_target(target)
        .map(|data| frozen_ids.contains(&data.id))
        .unwrap_or(false)
}

fn resolve_send_target(
    target: &str,
    device: SendDevice,
    titles: &HashMap<String, String>,
) -> Result<Option<(String, Vec<PathBuf>)>, String> {
    if target == "hotentry" {
        let path = newest_hotentry_file_path(device.ebook_file_ext())?;
        return Ok(path.map(|value| ("hotentry".to_string(), vec![value])));
    }

    let Some(data) = download::get_data_by_target(target) else {
        eprintln!("{} は存在しません", target);
        return Ok(None);
    };
    let record = load_record(data.id)?;
    let novel_dir =
        db::with_database(|db| Ok(existing_novel_dir_for_record(db.archive_root(), &record)))
            .map_err(|e| e.to_string())?;
    let ebook_paths = get_ebook_file_paths(&record, &novel_dir, device.ebook_file_ext())?;
    let title = titles
        .get(target)
        .cloned()
        .unwrap_or_else(|| data.title.clone());
    Ok(Some((format!("ID:{}　{}", target, title), ebook_paths)))
}

fn load_record(id: i64) -> Result<NovelRecord, String> {
    db::with_database(|db| Ok(db.get(id).cloned()))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{} は存在しません", id))
}

fn copy_with_progress(device: SendDevice, ebook_path: &Path) -> Result<Option<PathBuf>, String> {
    let manager_device = device
        .manager_device()
        .ok_or_else(|| "送信先端末が不正です".to_string())?;
    print!("{}へ送信しています", device.name());
    let src_file = ebook_path.to_path_buf();
    let handle =
        thread::spawn(move || OutputManager::new(manager_device).copy_to_documents(&src_file));

    while !handle.is_finished() {
        thread::sleep(Duration::from_millis(500));
        if !handle.is_finished() {
            print!(".");
        }
    }

    println!();

    let copied_to = handle
        .join()
        .map_err(|_| "送信を中断しました".to_string())?
        .map_err(|e| e.to_string())?;
    Ok(copied_to)
}

fn process_backup_bookmark(device: SendDevice, manager: &OutputManager) {
    if !device.bookmark_backup_supported() {
        eprintln!("ご利用の端末での栞データのバックアップは対応していません");
        return;
    }

    match backup_bookmark_files(device, manager) {
        Ok(count) if count > 0 => println!("端末の栞データをバックアップしました"),
        Ok(_) => {}
        Err(message) => eprintln!("{}", message),
    }
}

fn process_restore_bookmark(device: SendDevice, manager: &OutputManager) {
    if !device.bookmark_backup_supported() {
        eprintln!("ご利用の端末での栞データのバックアップは対応していません");
        return;
    }

    match restore_bookmark_files(device, manager) {
        Ok(count) if count > 0 => println!("栞データを端末にコピーしました"),
        Ok(_) => println!("栞データが無いようです"),
        Err(message) => eprintln!("{}", message),
    }
}

fn backup_bookmark_files(device: SendDevice, manager: &OutputManager) -> Result<usize, String> {
    let Some(documents_path) = manager.get_documents_path() else {
        return Err("端末が接続されていません".to_string());
    };
    let files = collect_kindle_bookmark_files(&documents_path)?;
    let storage = bookmark_storage_path(device)?;
    copy_grouped_files(&files, &storage, false)?;
    Ok(files.len())
}

fn restore_bookmark_files(device: SendDevice, manager: &OutputManager) -> Result<usize, String> {
    let Some(documents_path) = manager.get_documents_path() else {
        return Err("端末が接続されていません".to_string());
    };
    let storage = bookmark_storage_path(device)?;
    if !storage.exists() {
        return Ok(0);
    }
    let files = collect_stored_bookmark_files(&storage)?;
    copy_grouped_files(&files, &documents_path, false)?;
    Ok(files.len())
}

fn bookmark_storage_path(device: SendDevice) -> Result<PathBuf, String> {
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    Ok(inventory
        .root_dir()
        .join("misc")
        .join("bookmark")
        .join(device.name().to_ascii_lowercase()))
}

fn collect_kindle_bookmark_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    collect_files_matching(root, &|path| {
        let Some(parent) = path
            .parent()
            .and_then(|value| value.file_name())
            .and_then(|v| v.to_str())
        else {
            return false;
        };
        if !parent.ends_with(".sdr") {
            return false;
        }
        matches!(
            path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()),
            Some(ext) if ext == "azw3f" || ext == "azw3r"
        )
    })
}

fn collect_stored_bookmark_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    collect_files_matching(root, &|path| {
        path.parent()
            .and_then(|value| value.file_name())
            .and_then(|v| v.to_str())
            .is_some_and(|name| name.ends_with(".sdr"))
    })
}

fn collect_files_matching<F>(root: &Path, predicate: &F) -> Result<Vec<PathBuf>, String>
where
    F: Fn(&Path) -> bool,
{
    let mut collected = Vec::new();
    if !root.exists() {
        return Ok(collected);
    }
    collect_files_matching_inner(root, predicate, &mut collected)?;
    Ok(collected)
}

fn collect_files_matching_inner<F>(
    current: &Path,
    predicate: &F,
    collected: &mut Vec<PathBuf>,
) -> Result<(), String>
where
    F: Fn(&Path) -> bool,
{
    for entry in std::fs::read_dir(current).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            collect_files_matching_inner(&path, predicate, collected)?;
        } else if path.is_file() && predicate(&path) {
            collected.push(path);
        }
    }
    Ok(())
}

fn copy_grouped_files(
    files: &[PathBuf],
    dest_dir: &Path,
    check_timestamp: bool,
) -> Result<(), String> {
    for src in files {
        let Some(dirname) = src.parent().and_then(|path| path.file_name()) else {
            continue;
        };
        let save_dir = dest_dir.join(dirname);
        std::fs::create_dir_all(&save_dir).map_err(|e| e.to_string())?;
        let Some(basename) = src.file_name() else {
            continue;
        };
        let dest = save_dir.join(basename);
        if check_timestamp && dest.exists() {
            let src_mtime = std::fs::metadata(src)
                .and_then(|value| value.modified())
                .map_err(|e| e.to_string())?;
            let dest_mtime = std::fs::metadata(&dest)
                .and_then(|value| value.modified())
                .map_err(|e| e.to_string())?;
            if dest_mtime >= src_mtime {
                continue;
            }
        }
        let _ = std::fs::copy(src, dest);
    }
    Ok(())
}

fn highlight_target(value: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        value.to_string()
    } else {
        format!("\x1b[1;32m{}\x1b[0m", value)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::{
        SendDevice, collect_kindle_bookmark_files, collect_stored_bookmark_files,
        copy_grouped_files,
    };

    #[test]
    fn send_device_parses_supported_names() {
        assert_eq!(SendDevice::parse("kindle"), Some(SendDevice::Kindle));
        assert_eq!(SendDevice::parse("reader"), Some(SendDevice::Reader));
        assert_eq!(SendDevice::parse("ibooks"), Some(SendDevice::Ibooks));
        assert_eq!(SendDevice::parse("unknown"), None);
    }

    #[test]
    fn bookmark_backup_copies_sdr_structure() {
        let src_root = TempDir::new().unwrap();
        let dst_root = TempDir::new().unwrap();

        let bookmark_dir = src_root.path().join("foo.sdr");
        std::fs::create_dir_all(&bookmark_dir).unwrap();
        std::fs::write(bookmark_dir.join("bookmark.azw3f"), "f").unwrap();
        std::fs::write(bookmark_dir.join("bookmark.azw3r"), "r").unwrap();
        std::fs::write(src_root.path().join("ignored.txt"), "x").unwrap();

        let files = collect_kindle_bookmark_files(src_root.path()).unwrap();
        assert_eq!(files.len(), 2);

        copy_grouped_files(&files, dst_root.path(), false).unwrap();
        assert!(
            dst_root
                .path()
                .join("foo.sdr")
                .join("bookmark.azw3f")
                .exists()
        );
        assert!(
            dst_root
                .path()
                .join("foo.sdr")
                .join("bookmark.azw3r")
                .exists()
        );
    }

    #[test]
    fn stored_bookmarks_are_restored_from_sdr_directories() {
        let storage_root = TempDir::new().unwrap();
        let bookmark_dir = storage_root.path().join("bar.sdr");
        std::fs::create_dir_all(&bookmark_dir).unwrap();
        std::fs::write(bookmark_dir.join("bookmark.azw3f"), "f").unwrap();

        let files = collect_stored_bookmark_files(storage_root.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].file_name().and_then(|value| value.to_str()),
            Some("bookmark.azw3f")
        );
    }
}
