use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::converter::NovelConverter;
use crate::converter::device::Device;
use crate::converter::settings::NovelSettings;
use crate::converter::user_converter::UserConverter;
use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::{NarouError, Result};
use unicode_normalization::UnicodeNormalization;

const DIGEST_CHOICES: &[(&str, &str)] = &[
    ("1", "このまま更新する"),
    ("2", "更新をキャンセル"),
    ("3", "更新をキャンセルして小説を凍結する"),
    ("4", "バックアップを作成する"),
    ("5", "最新のあらすじを表示する"),
    ("6", "小説ページをブラウザで開く"),
    ("7", "保存フォルダを開く"),
    ("8", "変換する"),
];
const DIGEST_DEFAULT: &str = "2";
pub const HIDE_CONSOLE_ENV: &str = "NAROU_RS_HIDE_CONSOLE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestChoice {
    Update,
    Cancel,
    CancelAndFreeze,
    Backup,
    ShowStory,
    OpenBrowser,
    OpenFolder,
    Convert,
}

pub fn inherited_hide_console_requested() -> bool {
    matches!(
        std::env::var(HIDE_CONSOLE_ENV).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    ) || std::env::args().any(|arg| arg == "--hide-console")
}

pub fn configure_hidden_console_command(command: &mut Command) {
    if !inherited_hide_console_requested() {
        return;
    }

    command.env(HIDE_CONSOLE_ENV, "1");

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        command.creation_flags(CREATE_NO_WINDOW);
    }
}

pub fn load_local_setting_value(key: &str) -> Option<serde_yaml::Value> {
    crate::db::with_database(|db| {
        let settings: HashMap<String, serde_yaml::Value> = db
            .inventory()
            .load("local_setting", InventoryScope::Local)?;
        Ok(settings.get(key).cloned())
    })
    .ok()
    .flatten()
}

pub fn load_local_setting_string(key: &str) -> Option<String> {
    load_local_setting_value(key).and_then(|v| yaml_value_to_string(&v))
}

pub fn load_local_setting_bool(key: &str) -> bool {
    load_local_setting_value(key)
        .and_then(|v| match v {
            serde_yaml::Value::Bool(b) => Some(b),
            serde_yaml::Value::String(s) => Some(matches!(s.as_str(), "true" | "yes" | "on" | "1")),
            serde_yaml::Value::Number(n) => Some(n.as_i64().unwrap_or(0) != 0),
            _ => None,
        })
        .unwrap_or(false)
}

pub fn relay_web_stream_to_console<R: io::Read>(
    reader: R,
    target_console: &str,
) -> std::result::Result<(), String> {
    let reader = BufReader::new(reader);
    for line in reader.lines() {
        println!(
            "{}",
            reroute_web_line_to_console(&line.map_err(|e| e.to_string())?, target_console)
        );
    }
    Ok(())
}

pub fn reroute_web_line_to_console(text: &str, target_console: &str) -> String {
    if let Some(json_str) = text.strip_prefix(crate::progress::WS_LINE_PREFIX) {
        if let Ok(mut message) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json_str)
        {
            message.insert(
                "target_console".to_string(),
                serde_json::Value::String(target_console.to_string()),
            );
            return format!(
                "{}{}",
                crate::progress::WS_LINE_PREFIX,
                serde_json::Value::Object(message)
            );
        }
    }
    format!(
        "{}{}",
        crate::progress::WS_LINE_PREFIX,
        serde_json::json!({
            "type": "echo",
            "body": text,
            "target_console": target_console
        })
    )
}

pub fn load_local_setting_list(key: &str) -> Vec<String> {
    match load_local_setting_value(key) {
        Some(serde_yaml::Value::Sequence(values)) => values
            .into_iter()
            .filter_map(|v| yaml_value_to_string(&v))
            .collect(),
        Some(serde_yaml::Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

pub fn yaml_value_to_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn current_device() -> Option<Device> {
    let raw = load_local_setting_string("device")?;
    let device = Device::from_str(&raw);
    (device != Device::Text).then_some(device)
}

pub fn load_frozen_ids() -> Result<HashSet<i64>> {
    crate::db::with_database(|db| load_frozen_ids_from_inventory(db.inventory()))
}

pub fn load_frozen_ids_from_inventory(inventory: &Inventory) -> Result<HashSet<i64>> {
    let frozen: HashMap<i64, serde_yaml::Value> =
        inventory.load("freeze", InventoryScope::Local)?;
    Ok(frozen.into_keys().collect())
}

pub fn load_locked_ids_from_inventory(inventory: &Inventory) -> Result<HashSet<i64>> {
    let locked: HashMap<i64, serde_yaml::Value> = inventory.load("lock", InventoryScope::Local)?;
    Ok(locked.into_keys().collect())
}

pub struct NovelLockGuard {
    inventory: Option<Inventory>,
    id: Option<i64>,
}

impl NovelLockGuard {
    pub fn acquire(id: Option<i64>) -> Result<Self> {
        let Some(id) = id else {
            return Ok(Self {
                inventory: None,
                id: None,
            });
        };

        let inventory = Inventory::with_default_root()?;
        let mut locked: HashMap<i64, serde_yaml::Value> =
            inventory.load("lock", InventoryScope::Local)?;
        locked.insert(id, current_lock_timestamp());
        inventory.save("lock", InventoryScope::Local, &locked)?;

        Ok(Self {
            inventory: Some(inventory),
            id: Some(id),
        })
    }
}

impl Drop for NovelLockGuard {
    fn drop(&mut self) {
        let (Some(inventory), Some(id)) = (&self.inventory, self.id) else {
            return;
        };
        let mut locked: HashMap<i64, serde_yaml::Value> = inventory
            .load("lock", InventoryScope::Local)
            .unwrap_or_default();
        if locked.remove(&id).is_some() {
            let _ = inventory.save("lock", InventoryScope::Local, &locked);
        }
    }
}

fn current_lock_timestamp() -> serde_yaml::Value {
    serde_yaml::Value::String(
        chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S%.9f %:z")
            .to_string(),
    )
}

pub fn record_is_frozen(record: &crate::db::NovelRecord, frozen_ids: &HashSet<i64>) -> bool {
    frozen_ids.contains(&record.id) || record.tags.iter().any(|tag| tag == "frozen")
}

pub fn is_frozen_id(id: i64) -> bool {
    let frozen_ids = load_frozen_ids().unwrap_or_default();
    if frozen_ids.contains(&id) {
        return true;
    }

    crate::db::with_database(|db| {
        Ok(db
            .get(id)
            .map(|record| record_is_frozen(record, &frozen_ids))
            .unwrap_or(false))
    })
    .unwrap_or(false)
}

pub fn set_frozen_state(id: i64, frozen: bool) -> Result<()> {
    crate::db::with_database_mut(|db| {
        let mut frozen_list: HashMap<i64, serde_yaml::Value> =
            db.inventory().load("freeze", InventoryScope::Local)?;
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;

        if frozen {
            frozen_list.insert(id, serde_yaml::Value::Bool(true));
            if !updated.tags.iter().any(|tag| tag == "frozen") {
                updated.tags.push("frozen".to_string());
            }
        } else {
            frozen_list.remove(&id);
            updated.tags.retain(|tag| tag != "frozen" && tag != "404");
        }

        db.insert(updated);
        db.inventory()
            .save("freeze", InventoryScope::Local, &frozen_list)?;
        db.save()
    })
}

pub fn open_directory(path: &Path, confirm_message: Option<&str>) {
    if let Some(message) = confirm_message {
        if !confirm(message, false, false) {
            return;
        }
    }

    let path = path.to_string_lossy().to_string();
    if cfg!(windows) {
        let _ = std::process::Command::new("explorer")
            .arg(format!("file:///{}", path.replace('\\', "/")))
            .spawn();
    } else if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("open").arg(&path).spawn();
    } else {
        let _ = std::process::Command::new("xdg-open").arg(&path).spawn();
    }
}

pub fn open_browser(url: &str) {
    let _ = open::that(url);
}

pub fn confirm(message: &str, default: bool, nontty_default: bool) -> bool {
    if !io::stdin().is_terminal() {
        return nontty_default;
    }

    print!("{} (y/n)?: ", message);
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).ok().unwrap_or(0) == 0 {
        return nontty_default;
    }
    let input = input.trim().to_lowercase();
    if input.is_empty() {
        return default;
    }
    matches!(input.as_str(), "y" | "yes")
}

pub fn choose_digest_action(title: &str, message: &str) -> DigestChoice {
    let auto_choices = load_local_setting_string("download.choices-of-digest-options")
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut queue = auto_choices;
    loop {
        let choice = if !queue.is_empty() {
            let choice = queue.remove(0);
            println!("{}", title);
            println!("{}", message);
            for (key, label) in DIGEST_CHOICES {
                println!("{}: {}", key, label);
            }
            println!("> {}", choice);
            choice
        } else if !io::stdin().is_terminal() {
            DIGEST_DEFAULT.to_string()
        } else {
            println!("{}", title);
            println!("{}", message);
            for (key, label) in DIGEST_CHOICES {
                println!("{}: {}", key, label);
            }
            print!("> ");
            let _ = io::stdout().flush();
            let mut input = String::new();
            if io::stdin().read_line(&mut input).ok().unwrap_or(0) == 0 {
                DIGEST_DEFAULT.to_string()
            } else {
                input.trim().to_string()
            }
        };

        match choice.as_str() {
            "1" => return DigestChoice::Update,
            "2" => return DigestChoice::Cancel,
            "3" => return DigestChoice::CancelAndFreeze,
            "4" => return DigestChoice::Backup,
            "5" => return DigestChoice::ShowStory,
            "6" => return DigestChoice::OpenBrowser,
            "7" => return DigestChoice::OpenFolder,
            "8" => return DigestChoice::Convert,
            _ => {
                if queue.is_empty() && !io::stdin().is_terminal() {
                    return DigestChoice::Cancel;
                }
                if queue.is_empty() {
                    println!("選択肢の中にありません。もう一度入力して下さい");
                }
            }
        }
    }
}

pub fn create_backup(novel_dir: &Path, title: &str) -> Result<String> {
    let backup_dir = novel_dir.join("backup");
    fs::create_dir_all(&backup_dir)?;
    let backup_name = format!(
        "{}_{}.zip",
        sanitize_backup_name(title),
        chrono::Local::now().format("%Y%m%d%H%M%S")
    );
    let backup_path = backup_dir.join(&backup_name);

    let file = fs::File::create(&backup_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    add_directory_to_zip(&mut zip, novel_dir, novel_dir, options)?;
    zip.finish()
        .map_err(|e| NarouError::Conversion(e.to_string()))?;
    Ok(backup_name)
}

pub fn convert_existing_novel(
    id: i64,
    title: &str,
    author: &str,
    novel_dir: &Path,
    no_open: bool,
) -> std::result::Result<PathBuf, String> {
    let _lock = NovelLockGuard::acquire(Some(id)).map_err(|e| e.to_string())?;
    let settings = NovelSettings::load_for_novel(id, title, author, novel_dir);
    let mut converter =
        if let Some(user_converter) = UserConverter::load_with_title(novel_dir, title) {
            NovelConverter::with_user_converter(settings, user_converter)
        } else {
            NovelConverter::new(settings)
        };
    converter.set_progress(Box::new(crate::progress::NoProgress));

    let device = current_device();
    let output_path = match device {
        Some(device) => converter
            .convert_novel_by_id_with_device(id, novel_dir, device, false, false)
            .map_err(|e| e.to_string())?,
        None => PathBuf::from(
            converter
                .convert_novel_by_id(id, novel_dir)
                .map_err(|e| e.to_string())?,
        ),
    };

    println!("  Converted: {}", output_path.display());
    if let Some(inspection) = converter.take_inspection_output() {
        println!("{}", inspection);
    }

    if let Some(device) = device {
        if let Ok(Some(path)) = copy_to_converted_file(&output_path, Some(device), id) {
            println!("{} へコピーしました", path.display());
        }
        let _ = send_file_to_device(&output_path, device);
    }

    if !no_open && !load_local_setting_bool("convert.no-open") {
        open_directory(novel_dir, Some("小説の保存フォルダを開きますか"));
    }

    Ok(output_path)
}

pub fn copy_to_converted_file(
    src_path: &Path,
    device: Option<Device>,
    novel_id: i64,
) -> std::result::Result<Option<PathBuf>, String> {
    let copy_to_dir = get_copy_to_directory(device, novel_id)?;
    let Some(copy_to_dir) = copy_to_dir else {
        return Ok(None);
    };

    fs::create_dir_all(&copy_to_dir).map_err(|e| e.to_string())?;
    let dst = copy_to_dir.join(
        src_path
            .file_name()
            .ok_or_else(|| "Invalid converted filename".to_string())?,
    );
    fs::copy(src_path, &dst).map_err(|e| e.to_string())?;
    Ok(Some(dst))
}

fn get_copy_to_directory(
    device: Option<Device>,
    novel_id: i64,
) -> std::result::Result<Option<PathBuf>, String> {
    let copy_to_dir = load_local_setting_string("convert.copy-to")
        .or_else(|| load_local_setting_string("convert.copy_to"));
    let Some(copy_to_dir) = copy_to_dir else {
        return Ok(None);
    };

    let base = PathBuf::from(&copy_to_dir);
    if !base.is_dir() {
        return Err(format!(
            "{} はフォルダではないかすでに削除されています。コピー出来ませんでした",
            copy_to_dir
        ));
    }

    let grouping = load_local_setting_list("convert.copy-to-grouping");
    let mut dir = base;
    if grouping
        .iter()
        .any(|value| value.eq_ignore_ascii_case("device"))
    {
        if let Some(device) = device {
            dir.push(device.display_name());
        }
    }
    if grouping
        .iter()
        .any(|value| value.eq_ignore_ascii_case("site"))
    {
        let sitename =
            crate::db::with_database(|db| Ok(db.get(novel_id).map(|r| r.sitename.clone())))
                .ok()
                .flatten();
        if let Some(sitename) = sitename.filter(|value| !value.is_empty()) {
            dir.push(sitename);
        }
    }
    Ok(Some(dir))
}

pub fn send_file_to_device(ebook_file: &Path, device: Device) -> std::result::Result<(), String> {
    let manager = crate::converter::device::OutputManager::new(device);
    if !device.physical_support() || !manager.connecting() || !device.matches_ebook_file(ebook_file)
    {
        return Ok(());
    }
    if !manager.ebook_file_old(ebook_file) {
        return Ok(());
    }

    println!("{}へ送信しています", device.display_name());
    match manager
        .copy_to_documents(ebook_file)
        .map_err(|e| e.to_string())?
    {
        Some(path) => {
            println!("{} へコピーしました", path.display());
            Ok(())
        }
        None => Err(format!(
            "{}が見つからなかったためコピー出来ませんでした",
            device.display_name()
        )),
    }
}

fn add_directory_to_zip(
    zip: &mut zip::ZipWriter<fs::File>,
    base_dir: &Path,
    current_dir: &Path,
    options: zip::write::SimpleFileOptions,
) -> Result<()> {
    let mut files = Vec::new();
    collect_backup_files(base_dir, current_dir, &mut files)?;

    let mut entries: Vec<(String, PathBuf)> = files
        .into_iter()
        .map(|path| {
            let rel_name = relative_backup_path(base_dir, &path)?;
            Ok((rel_name, path))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (rel_name, path) in entries {
        let mut file = fs::File::open(&path)?;
        zip.start_file(rel_name.replace('\\', "/"), options)
            .map_err(|e| NarouError::Conversion(e.to_string()))?;
        std::io::copy(&mut file, zip)?;
    }
    Ok(())
}

fn collect_backup_files(
    base_dir: &Path,
    current_dir: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(current_dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base_dir)
            .map_err(|e| NarouError::Conversion(e.to_string()))?;
        if rel.components().next().map(|c| c.as_os_str()) == Some(std::ffi::OsStr::new("backup")) {
            continue;
        }
        if path.is_dir() {
            collect_backup_files(base_dir, &path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn relative_backup_path(base_dir: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(base_dir)
        .map_err(|e| NarouError::Conversion(e.to_string()))?;
    Ok(rel.to_string_lossy().to_string())
}

fn sanitize_backup_name(title: &str) -> String {
    let mut cleaned = String::with_capacity(title.len());
    for ch in title.chars() {
        match ch {
            '/' => cleaned.push('／'),
            '\\' => cleaned.push('￥'),
            ':' => cleaned.push('：'),
            '*' => cleaned.push('＊'),
            '?' => cleaned.push('？'),
            '"' => cleaned.push('”'),
            '<' => cleaned.push('〈'),
            '>' => cleaned.push('〉'),
            '[' => cleaned.push('［'),
            ']' => cleaned.push('］'),
            '{' => cleaned.push('｛'),
            '}' => cleaned.push('｝'),
            '|' => cleaned.push('｜'),
            '.' => cleaned.push('．'),
            '`' => cleaned.push('｀'),
            '\0' | '\t' | '\n' | '\r' => {}
            _ => cleaned.push(ch),
        }
    }
    if load_local_setting_bool("normalize-filename") {
        cleaned = cleaned.nfc().collect();
    }
    while cleaned.as_bytes().len() > 180 {
        cleaned.pop();
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use crate::db::inventory::Inventory;
    use crate::progress::WS_LINE_PREFIX;
    use chrono::{TimeZone, Utc};

    use super::{
        NovelLockGuard, get_copy_to_directory, load_frozen_ids_from_inventory,
        load_locked_ids_from_inventory, record_is_frozen, reroute_web_line_to_console,
        sanitize_backup_name,
    };
    use crate::db::NovelRecord;

    fn sample_record(id: i64, tags: &[&str]) -> NovelRecord {
        NovelRecord {
            id,
            author: "author".to_string(),
            title: format!("title-{}", id),
            file_title: format!("file-{}", id),
            toc_url: format!("https://example.com/{}/", id),
            sitename: "site".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 4, 14, 0, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn sanitize_backup_name_matches_ruby_replacements() {
        assert_eq!(
            sanitize_backup_name("a/b\\c:d*e?f\"g<h>i[j]k{l}m|n.o`p\tq\nr"),
            "a／b￥c：d＊e？f”g〈h〉i［j］k｛l｝m｜n．o｀pqr"
        );
    }

    #[test]
    fn sanitize_backup_name_truncates_by_byte_length() {
        let name = sanitize_backup_name(&"あ".repeat(100));
        assert!(name.as_bytes().len() <= 180);
        assert!(name.chars().all(|ch| ch == 'あ'));
    }

    #[test]
    fn sanitize_backup_name_falls_back_when_empty() {
        assert_eq!(sanitize_backup_name(""), "");
    }

    #[test]
    fn reroute_web_line_to_console_wraps_plain_text_for_requested_console() {
        let routed = reroute_web_line_to_console("Converted: test.txt", "stdout2");
        assert!(routed.starts_with(WS_LINE_PREFIX));
        let json = routed.trim_start_matches(WS_LINE_PREFIX);
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["type"], "echo");
        assert_eq!(value["body"], "Converted: test.txt");
        assert_eq!(value["target_console"], "stdout2");
    }

    #[test]
    fn reroute_web_line_to_console_retargets_structured_messages() {
        let source = format!(
            "{}{}",
            WS_LINE_PREFIX,
            serde_json::json!({
                "type": "progressbar.step",
                "data": { "current": 1, "total": 2, "percent": 50.0, "topic": "convert" }
            })
        );
        let routed = reroute_web_line_to_console(&source, "stdout2");
        let json = routed.trim_start_matches(WS_LINE_PREFIX);
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["type"], "progressbar.step");
        assert_eq!(value["target_console"], "stdout2");
        assert_eq!(value["data"]["topic"], "convert");
    }

    #[test]
    fn record_is_frozen_checks_freeze_inventory_before_tags() {
        let mut frozen_ids = std::collections::HashSet::new();
        frozen_ids.insert(1);

        assert!(record_is_frozen(&sample_record(1, &[]), &frozen_ids));
        assert!(record_is_frozen(
            &sample_record(2, &["frozen"]),
            &frozen_ids
        ));
        assert!(!record_is_frozen(&sample_record(3, &[]), &frozen_ids));
    }

    #[test]
    fn database_parity_load_frozen_ids_from_inventory_reads_zero_id() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        std::fs::write(
            temp.path().join(".narou").join("freeze.yaml"),
            "0: true\n3: true\n",
        )
        .unwrap();

        let inventory = Inventory::new(temp.path().to_path_buf());
        let frozen_ids = load_frozen_ids_from_inventory(&inventory).unwrap();

        assert!(frozen_ids.contains(&0));
        assert!(frozen_ids.contains(&3));
        assert_eq!(frozen_ids.len(), 2);
    }

    #[test]
    fn novel_lock_guard_writes_and_clears_lock_yaml() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());

        {
            let _lock = NovelLockGuard::acquire(Some(7)).unwrap();
            let inventory = Inventory::new(temp.path().to_path_buf());
            let locked_ids = load_locked_ids_from_inventory(&inventory).unwrap();
            assert_eq!(locked_ids, std::collections::HashSet::from([7]));
            let raw =
                std::fs::read_to_string(temp.path().join(".narou").join("lock.yaml")).unwrap();
            assert!(raw.contains("7:"));
            assert!(raw.contains(" +"));
            assert!(!raw.contains('T'));
        }

        let inventory = Inventory::new(temp.path().to_path_buf());
        let locked_ids = load_locked_ids_from_inventory(&inventory).unwrap();
        assert!(locked_ids.is_empty());
    }

    #[test]
    fn novel_lock_guard_preserves_other_locked_ids() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        std::fs::write(
            temp.path().join(".narou").join("lock.yaml"),
            "3: 2026-04-20T00:00:00+09:00\n",
        )
        .unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());

        {
            let _lock = NovelLockGuard::acquire(Some(7)).unwrap();
            let inventory = Inventory::new(temp.path().to_path_buf());
            let locked_ids = load_locked_ids_from_inventory(&inventory).unwrap();
            assert_eq!(locked_ids, std::collections::HashSet::from([3, 7]));
        }

        let inventory = Inventory::new(temp.path().to_path_buf());
        let locked_ids = load_locked_ids_from_inventory(&inventory).unwrap();
        assert_eq!(locked_ids, std::collections::HashSet::from([3]));
    }

    #[test]
    fn database_parity_get_copy_to_directory_includes_site_for_zero_id() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let copy_to = temp.path().join("copy-to");
        std::fs::create_dir_all(&copy_to).unwrap();
        std::fs::write(
            temp.path().join(".narou").join("local_setting.yaml"),
            format!(
                "convert.copy-to: \"{}\"\nconvert.copy-to-grouping:\n  - site\n",
                copy_to.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();

        *crate::db::DATABASE.lock() = None;
        crate::db::init_database().unwrap();
        crate::db::with_database_mut(|db| {
            db.insert(sample_record(0, &[]));
            Ok(())
        })
        .unwrap();

        let dir = get_copy_to_directory(None, 0).unwrap().unwrap();
        assert_eq!(dir, copy_to.join("site"));

        *crate::db::DATABASE.lock() = None;
    }
}
