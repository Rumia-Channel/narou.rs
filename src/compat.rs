use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::converter::NovelConverter;
use crate::converter::device::Device;
use crate::converter::settings::NovelSettings;
use crate::converter::user_converter::UserConverter;
use crate::db::inventory::InventoryScope;
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
            .convert_novel_by_id_with_device(id, novel_dir, device)
            .map_err(|e| e.to_string())?,
        None => PathBuf::from(
            converter
                .convert_novel_by_id(id, novel_dir)
                .map_err(|e| e.to_string())?,
        ),
    };

    println!("  Converted: {}", output_path.display());

    if let Some(device) = device {
        let _ = copy_to_converted_file(&output_path, Some(device), id);
        let _ = send_file_to_device(&output_path, device);
    }

    if !no_open && !load_local_setting_bool("convert.no-open") {
        open_directory(novel_dir, Some("小説の保存フォルダを開きますか"));
    }

    Ok(output_path)
}

fn copy_to_converted_file(
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
    println!("{} へコピーしました", dst.display());
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
        && novel_id > 0
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

fn send_file_to_device(ebook_file: &Path, device: Device) -> std::result::Result<(), String> {
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
    use super::sanitize_backup_name;

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
}
