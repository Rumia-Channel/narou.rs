use std::collections::HashMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use lettre::message::header::ContentType;
use lettre::message::{Attachment, Body, Mailbox, Message, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{SmtpTransport, Transport};

use crate::converter::output::create_output_text_filename;
use crate::converter::settings::NovelSettings;
use crate::db::NovelRecord;
use crate::db::inventory::Inventory;
use crate::downloader::site_setting::SiteSetting;
use crate::downloader::{Downloader, TargetType, TocObject};
use crate::error::Result;

pub const MAIL_SETTING_FILE: &str = "mail_setting.yaml";
pub const MAIL_INTERRUPTED_MESSAGE: &str = "メール送信を中断しました";

#[derive(Debug, Clone)]
pub struct MailSetting {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub via: String,
    pub via_options: HashMap<String, serde_yaml::Value>,
    pub extras: HashMap<String, serde_yaml::Value>,
}

#[derive(Debug)]
pub enum MailSettingLoadError {
    NotFound(PathBuf),
    Incomplete(PathBuf),
    Io(std::io::Error),
    Yaml(serde_yaml::Error),
}

impl fmt::Display for MailSettingLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MailSettingLoadError::NotFound(path) => write!(f, "{} not found", path.display()),
            MailSettingLoadError::Incomplete(path) => write!(
                f,
                "設定ファイルの書き換えが終了していないようです。\n設定ファイルは {} にあります",
                path.display()
            ),
            MailSettingLoadError::Io(e) => write!(f, "{}", e),
            MailSettingLoadError::Yaml(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for MailSettingLoadError {}

impl From<std::io::Error> for MailSettingLoadError {
    fn from(value: std::io::Error) -> Self {
        MailSettingLoadError::Io(value)
    }
}

impl From<serde_yaml::Error> for MailSettingLoadError {
    fn from(value: serde_yaml::Error) -> Self {
        MailSettingLoadError::Yaml(value)
    }
}

pub fn mail_setting_path() -> Result<PathBuf> {
    let inv = Inventory::with_default_root()?;
    Ok(inv.root_dir().join(MAIL_SETTING_FILE))
}

pub fn ensure_mail_setting_file() -> Result<PathBuf> {
    let path = mail_setting_path()?;
    if path.exists() {
        return Ok(path);
    }

    let preset = preset_dir()?.join(MAIL_SETTING_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&preset, &path)?;
    seed_last_mail_dates()?;

    if let Some(parent) = path.parent() {
        crate::compat::open_directory(parent, Some("設定ファイルがあるフォルダを開きますか"));
    }

    Ok(path)
}

pub fn seed_last_mail_dates() -> Result<()> {
    crate::db::with_database_mut(|db| -> Result<()> {
        let now = chrono::Utc::now();
        for record in db.all_records_mut().values_mut() {
            if record.last_mail_date.is_none() {
                record.last_mail_date = Some(now);
            }
        }
        db.save()
    })
}

pub fn load_mail_setting() -> std::result::Result<MailSetting, MailSettingLoadError> {
    let path = mail_setting_path()
        .map_err(|e| MailSettingLoadError::Io(std::io::Error::other(e.to_string())))?;
    if !path.exists() {
        return Err(MailSettingLoadError::NotFound(path));
    }

    let raw = std::fs::read_to_string(&path)?;
    let map = parse_symbolic_yaml_map(&raw)?;

    if !yaml_bool(map.get("complete")).unwrap_or(false) {
        return Err(MailSettingLoadError::Incomplete(path));
    }

    let from = yaml_string(map.get("from")).unwrap_or_default();
    let to = yaml_string(map.get("to")).unwrap_or_default();
    if from.is_empty() || to.is_empty() {
        return Err(MailSettingLoadError::Incomplete(path));
    }

    let subject = yaml_string(map.get("subject")).unwrap_or_default();
    let via = yaml_string(map.get("via")).unwrap_or_else(|| "smtp".to_string());
    let via_options = map
        .get("via_options")
        .and_then(yaml_map_owned)
        .unwrap_or_default();

    let mut extras = map;
    for key in ["from", "to", "subject", "via", "via_options", "complete"] {
        extras.remove(key);
    }

    Ok(MailSetting {
        from,
        to,
        subject,
        via,
        via_options,
        extras,
    })
}

pub fn send_target_with_setting(
    setting: &MailSetting,
    target: &str,
    send_all: bool,
    force: bool,
) -> std::result::Result<bool, String> {
    send_target_with_setting_interruptible(setting, target, send_all, force, None)
}

pub fn send_target_with_setting_interruptible(
    setting: &MailSetting,
    target: &str,
    send_all: bool,
    force: bool,
    interrupted: Option<&AtomicBool>,
) -> std::result::Result<bool, String> {
    let target = alias_to_target(target);

    if target == "hotentry" {
        return send_hotentry(setting, send_all, interrupted);
    }

    let record = match resolve_record(&target)? {
        Some(record) => record,
        None => {
            if !send_all {
                eprintln!("{} は存在しません", target);
            }
            return Ok(false);
        }
    };

    if send_all && !force {
        if let (Some(new_arrivals_date), Some(last_mail_date)) =
            (record.new_arrivals_date, record.last_mail_date)
        {
            if new_arrivals_date < last_mail_date {
                return Ok(false);
            }
        }
    }

    let novel_dir = crate::db::with_database(|db| -> Result<PathBuf> {
        Ok(crate::db::existing_novel_dir_for_record(
            db.archive_root(),
            &record,
        ))
    })
    .map_err(|e| e.to_string())?;

    let ext = current_device_ext().unwrap_or_else(|| ".epub".to_string());
    let ebook_paths = get_ebook_file_paths(&record, &novel_dir, &ext)?;
    let Some(first) = ebook_paths.first() else {
        return Ok(false);
    };

    if !first.exists() {
        if !send_all {
            eprintln!(
                "まだファイル({})が無いようです",
                first
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
            );
        }
        return Ok(false);
    }

    println!("{}", green_bold(&format!("ID:{}　{}", record.id, record.title)));

    for ebook_path in ebook_paths {
        if !ebook_path.exists() {
            continue;
        }
        let body = ebook_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        send_mail_with_progress(
            setting,
            &record.id.to_string(),
            &body,
            &ebook_path,
            interrupted,
        )?;
        println!(
            "{} をメールで送信しました",
            ebook_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
        );
        update_last_mail_date(record.id)?;
    }

    Ok(true)
}

fn send_hotentry(
    setting: &MailSetting,
    send_all: bool,
    interrupted: Option<&AtomicBool>,
) -> std::result::Result<bool, String> {
    let ext = current_device_ext().unwrap_or_else(|| ".epub".to_string());
    let Some(path) = newest_hotentry_file_path(&ext)? else {
        return Ok(false);
    };

    if !path.exists() {
        if !send_all {
            eprintln!(
                "まだファイル({})が無いようです",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
            );
        }
        return Ok(false);
    }

    println!("{}", green_bold("hotentry"));
    let body = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    send_mail_with_progress(setting, "hotentry", &body, &path, interrupted)?;
    println!(
        "{} をメールで送信しました",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
    );
    Ok(true)
}

pub fn send_mail(
    setting: &MailSetting,
    id: &str,
    body: &str,
    attachment_path: &Path,
) -> std::result::Result<(), String> {
    if !setting.via.eq_ignore_ascii_case("smtp") {
        return Err(format!("unsupported mail via: {}", setting.via));
    }

    let from = parse_mailbox(&setting.from)?;
    let to = parse_mailbox(&setting.to)?;
    let attachment_name = attachment_name(id, attachment_path);
    let attachment_bytes = std::fs::read(attachment_path).map_err(|e| e.to_string())?;

    let message = Message::builder()
        .from(from)
        .to(to)
        .subject(setting.subject.clone())
        .multipart(
            MultiPart::mixed()
                .singlepart(SinglePart::plain(body.to_string()))
                .singlepart(Attachment::new(attachment_name).body(
                    Body::new(attachment_bytes),
                    guess_attachment_content_type(attachment_path),
                )),
        )
        .map_err(|e| e.to_string())?;

    let transport = build_transport(setting)?;
    transport.send(&message).map_err(|e| e.to_string())?;
    Ok(())
}

fn send_mail_with_progress(
    setting: &MailSetting,
    id: &str,
    body: &str,
    attachment_path: &Path,
    interrupted: Option<&AtomicBool>,
) -> std::result::Result<(), String> {
    let setting = setting.clone();
    let id = id.to_string();
    let body = body.to_string();
    let attachment_path = attachment_path.to_path_buf();
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let _ = tx.send(send_mail(&setting, &id, &body, &attachment_path));
    });

    print!("メールを送信しています");
    let _ = std::io::stdout().flush();

    loop {
        if interrupted.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            println!();
            return Err(MAIL_INTERRUPTED_MESSAGE.to_string());
        }

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(result) => {
                println!();
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                print!(".");
                let _ = std::io::stdout().flush();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                println!();
                return Err("メール送信に失敗しました".to_string());
            }
        }
    }
}

fn build_transport(setting: &MailSetting) -> std::result::Result<SmtpTransport, String> {
    let via_options = &setting.via_options;
    let host = yaml_string(via_options.get("address"))
        .ok_or_else(|| "mail_setting.yaml の via_options.address が未設定です".to_string())?;
    let port = yaml_u16(via_options.get("port")).unwrap_or(587);
    let user_name = yaml_string(via_options.get("user_name")).unwrap_or_default();
    let password = yaml_string(via_options.get("password")).unwrap_or_default();
    let authentication = yaml_string(via_options.get("authentication"));
    let enable_starttls_auto = yaml_bool(via_options.get("enable_starttls_auto")).unwrap_or(true);

    let mut builder = if port == 465 {
        SmtpTransport::relay(&host)
            .map_err(|e| e.to_string())?
            .port(port)
    } else if enable_starttls_auto {
        SmtpTransport::starttls_relay(&host)
            .map_err(|e| e.to_string())?
            .port(port)
    } else {
        SmtpTransport::builder_dangerous(&host).port(port)
    };

    if !user_name.is_empty() {
        builder = builder.credentials(Credentials::new(user_name, password));
    }

    if let Some(auth) = authentication.as_deref() {
        if auth.eq_ignore_ascii_case(":plain") || auth.eq_ignore_ascii_case("plain") {
            builder = builder.authentication(vec![Mechanism::Plain]);
        }
    }

    Ok(builder.build())
}

fn resolve_record(target: &str) -> std::result::Result<Option<NovelRecord>, String> {
    if let Ok(id) = target.parse::<i64>() {
        let record = crate::db::with_database(|db| -> Result<Option<NovelRecord>> {
            Ok(db.get(id).cloned())
        })
        .map_err(|e| e.to_string())?;
        if record.is_some() {
            return Ok(record);
        }
    }

    match Downloader::get_target_type(target) {
        TargetType::Url => {
            let site_settings = SiteSetting::load_all().map_err(|e| e.to_string())?;
            for setting in &site_settings {
                if setting.matches_url(target) {
                    let toc_url = setting
                        .toc_url_with_url_captures(target)
                        .unwrap_or_else(|| setting.toc_url());
                    let record = crate::db::with_database(|db| -> Result<Option<NovelRecord>> {
                        Ok(db.get_by_toc_url(&toc_url).cloned())
                    })
                    .map_err(|e| e.to_string())?;
                    if record.is_some() {
                        return Ok(record);
                    }
                }
            }
            Ok(None)
        }
        TargetType::Ncode => {
            let ncode = target.to_lowercase();
            let record = crate::db::with_database(|db| -> Result<Option<NovelRecord>> {
                Ok(db
                    .all_records()
                    .values()
                    .find(|r| {
                        r.ncode.as_deref() == Some(ncode.as_str())
                            || r.toc_url
                                .to_lowercase()
                                .trim_end_matches('/')
                                .ends_with(&format!("/{}", ncode))
                    })
                    .cloned())
            })
            .map_err(|e| e.to_string())?;
            Ok(record)
        }
        TargetType::Other => {
            let record = crate::db::with_database(|db| -> Result<Option<NovelRecord>> {
                Ok(db.find_by_title(target).cloned())
            })
            .map_err(|e| e.to_string())?;
            Ok(record)
        }
        TargetType::Id => Ok(None),
    }
}

pub fn get_ebook_file_paths(
    record: &NovelRecord,
    novel_dir: &Path,
    ext: &str,
) -> std::result::Result<Vec<PathBuf>, String> {
    let settings =
        NovelSettings::load_for_novel(record.id, &record.title, &record.author, novel_dir);
    let toc = TocObject {
        title: record.title.clone(),
        author: record.author.clone(),
        toc_url: record.toc_url.clone(),
        story: None,
        subtitles: Vec::new(),
        novel_type: Some(record.novel_type),
    };
    let txt_name = create_output_text_filename(&settings, record.id, &toc);
    let base = PathBuf::from(txt_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "Invalid converted filename".to_string())?
        .to_string();

    let mut paths = vec![novel_dir.join(format!("{}{}", base, ext))];
    let mut idx = 2usize;
    loop {
        let next = novel_dir.join(format!("{}_{}{}", base, idx, ext));
        if next.exists() {
            paths.push(next);
            idx += 1;
        } else {
            break;
        }
    }
    Ok(paths)
}

pub fn newest_hotentry_file_path(ext: &str) -> std::result::Result<Option<PathBuf>, String> {
    let root = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let hotentry_dir = root.root_dir().join("hotentry");
    if !hotentry_dir.exists() {
        return Ok(None);
    }

    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(&hotentry_dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with("hotentry_") && name.ends_with(ext) {
            candidates.push(path);
        }
    }
    candidates.sort();
    Ok(candidates.pop())
}

fn update_last_mail_date(id: i64) -> std::result::Result<(), String> {
    crate::db::with_database_mut(|db| -> Result<()> {
        if let Some(record) = db.get(id).cloned() {
            let mut updated = record;
            updated.last_mail_date = Some(chrono::Utc::now());
            db.insert(updated);
            db.save()?;
        }
        Ok(())
    })
    .map_err(|e| e.to_string())
}

fn current_device_ext() -> Option<String> {
    crate::compat::current_device().map(|device| device.ebook_file_ext().to_string())
}

fn alias_to_target(target: &str) -> String {
    crate::db::with_database(|db| -> Result<Option<String>> {
        let aliases: HashMap<String, serde_yaml::Value> = db
            .inventory()
            .load("alias", crate::db::inventory::InventoryScope::Local)?;
        Ok(aliases
            .get(target)
            .and_then(|v| yaml_value_to_string(Some(v))))
    })
    .ok()
    .flatten()
    .unwrap_or_else(|| target.to_string())
}

fn attachment_name(id: &str, path: &Path) -> String {
    let ext = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.find('.').map(|idx| &name[idx..]))
        .unwrap_or("");
    format!("{}{}", id, ext)
}

fn green_bold(text: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        text.to_string()
    } else {
        format!("\x1b[1;32m{}\x1b[0m", text)
    }
}

fn guess_attachment_content_type(path: &Path) -> ContentType {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "mobi" => {
            ContentType::parse("application/x-mobipocket-ebook").unwrap_or(ContentType::TEXT_PLAIN)
        }
        "epub" | "kepub" => {
            ContentType::parse("application/epub+zip").unwrap_or(ContentType::TEXT_PLAIN)
        }
        _ => ContentType::TEXT_PLAIN,
    }
}

fn parse_mailbox(raw: &str) -> std::result::Result<Mailbox, String> {
    raw.trim().parse::<Mailbox>().map_err(|e| e.to_string())
}

fn parse_symbolic_yaml_map(
    raw: &str,
) -> std::result::Result<HashMap<String, serde_yaml::Value>, MailSettingLoadError> {
    let value: serde_yaml::Value = serde_yaml::from_str(raw)?;
    let serde_yaml::Value::Mapping(map) = value else {
        return Ok(HashMap::new());
    };

    let mut out = HashMap::new();
    for (k, v) in map {
        if let Some(key) = yaml_value_to_string(Some(&k)) {
            out.insert(key.trim_start_matches(':').to_string(), v);
        }
    }
    Ok(out)
}

fn yaml_map_owned(value: &serde_yaml::Value) -> Option<HashMap<String, serde_yaml::Value>> {
    match value {
        serde_yaml::Value::Mapping(map) => {
            let mut out = HashMap::new();
            for (k, v) in map {
                if let Some(key) = yaml_value_to_string(Some(k)) {
                    out.insert(key.trim_start_matches(':').to_string(), v.clone());
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn yaml_value_to_string(value: Option<&serde_yaml::Value>) -> Option<String> {
    let value = value?;
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn yaml_string(value: Option<&serde_yaml::Value>) -> Option<String> {
    yaml_value_to_string(value)
}

fn yaml_bool(value: Option<&serde_yaml::Value>) -> Option<bool> {
    let value = value?;
    match value {
        serde_yaml::Value::Bool(b) => Some(*b),
        serde_yaml::Value::String(s) => Some(matches!(s.as_str(), "true" | "yes" | "on" | "1")),
        serde_yaml::Value::Number(n) => Some(n.as_i64().unwrap_or(0) != 0),
        _ => None,
    }
}

fn yaml_u16(value: Option<&serde_yaml::Value>) -> Option<u16> {
    let value = value?;
    match value {
        serde_yaml::Value::Number(n) => n.as_u64().and_then(|v| u16::try_from(v).ok()),
        serde_yaml::Value::String(s) => s.parse::<u16>().ok(),
        _ => None,
    }
}

fn preset_dir() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("preset"));
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(manifest_dir.join("preset"));
    candidates.push(manifest_dir.join("sample").join("narou").join("preset"));

    candidates
        .into_iter()
        .find(|path| path.is_dir())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "narou preset directory not found",
            )
            .into()
        })
}

#[cfg(test)]
mod tests {
    use super::{MAIL_INTERRUPTED_MESSAGE, MailSetting, attachment_name, send_mail_with_progress};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    #[test]
    fn attachment_name_reuses_original_extension() {
        let path = std::path::Path::new("example.kepub.epub");
        assert_eq!(attachment_name("hotentry", path), "hotentry.kepub.epub");
    }

    #[test]
    fn send_mail_with_progress_returns_interrupt_message() {
        let tmp = TempDir::new().unwrap();
        let attachment = tmp.path().join("sample.epub");
        std::fs::write(&attachment, "dummy").unwrap();

        let interrupted = AtomicBool::new(true);
        let setting = MailSetting {
            from: "sender@example.com".to_string(),
            to: "receiver@example.com".to_string(),
            subject: "subject".to_string(),
            via: "unsupported".to_string(),
            via_options: HashMap::new(),
            extras: HashMap::new(),
        };

        let err = send_mail_with_progress(
            &setting,
            "1",
            "body",
            &attachment,
            Some(&interrupted),
        )
        .unwrap_err();

        assert_eq!(err, MAIL_INTERRUPTED_MESSAGE);
    }
}
