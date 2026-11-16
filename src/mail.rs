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
use lettre::transport::smtp::client::{Tls, TlsParametersBuilder};
use lettre::transport::smtp::extension::ClientId;
use lettre::{SmtpTransport, Transport};
use regex::Regex;

use crate::converter::output::create_output_text_filename;
use crate::converter::settings::NovelSettings;
use crate::db::NovelRecord;
use crate::db::inventory::Inventory;
use crate::downloader::site_setting::effective_site_settings;
use crate::downloader::TocObject;
use crate::error::Result;

pub const MAIL_SETTING_FILE: &str = "mail_setting.yaml";
pub const MAIL_INTERRUPTED_MESSAGE: &str = "メール送信を中断しました";
const MAIL_ATTACHMENT_PATTERN_SETTING: &str = "mail.attachment-filename-pattern";
const MAIL_ATTACHMENT_REPLACEMENT_SETTING: &str = "mail.attachment-filename-replacement";
const MAIL_ATTACHMENT_PATTERN_KEYS: &[&str] = &[
    "attachment_filename_pattern",
    "attachment-filename-pattern",
    MAIL_ATTACHMENT_PATTERN_SETTING,
];
const MAIL_ATTACHMENT_REPLACEMENT_KEYS: &[&str] = &[
    "attachment_filename_replacement",
    "attachment-filename-replacement",
    MAIL_ATTACHMENT_REPLACEMENT_SETTING,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmtpTlsMode {
    None,
    Opportunistic,
    Required,
    Wrapper,
}

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
    let novels = crate::native::novel_repository::NativeNovelRepository::new();
    let ids = novels
        .scan_ids_sync(&crate::platform::NovelFilter::all(), None, usize::MAX)?;
    let now = chrono::Utc::now();
    for id in ids {
        let Some(record) = novels.get_current_sync(id)? else {
            continue;
        };
        if record.last_mail_date.is_some() {
            continue;
        }
        let write = crate::native::materialization::capture_write_for_record(&record)?;
        novels.update_existing_guarded_sync(write.fence(), id, |record| {
            if record.last_mail_date.is_none() {
                record.last_mail_date = Some(now);
            }
            Ok(())
        })?;
    }
    Ok(())
}

pub fn load_mail_setting() -> std::result::Result<MailSetting, MailSettingLoadError> {
    let path = mail_setting_path()
        .map_err(|e| MailSettingLoadError::Io(std::io::Error::other(e.to_string())))?;
    if !path.exists() {
        return Err(MailSettingLoadError::NotFound(path));
    }

    let raw = std::fs::read_to_string(&path)?;
    let map = parse_symbolic_yaml_map(&raw)?;

    if !map.get("complete").and_then(crate::setting_core::setting_bool_value).unwrap_or(false) {
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
    let target = alias_to_target(target)?;

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

    if send_all && !force
        && let (Some(new_arrivals_date), Some(last_mail_date)) =
            (record.new_arrivals_date, record.last_mail_date)
            && new_arrivals_date < last_mail_date {
                return Ok(false);
            }

    let write = crate::native::materialization::capture_write_for_record(&record)
        .map_err(|error| error.to_string())?;
    let ext = current_device_ext().unwrap_or_else(|| ".epub".to_string());
    let ebook_paths = get_ebook_file_paths(&record, &write, &ext)?;
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

    println!(
        "{}",
        green_bold(&format!("ID:{}　{}", record.id, record.title))
    );

    for ebook_path in &ebook_paths {
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
            ebook_path,
            interrupted,
        )?;
        println!(
            "{} をメールで送信しました",
            ebook_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
        );
        update_last_mail_date(&write, record.id)?;
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

    let message = build_message(setting, id, body, attachment_path)?;
    send_built_message(setting, &message)
}

fn send_built_message(setting: &MailSetting, message: &Message) -> std::result::Result<(), String> {
    let transport = build_transport(setting)?;
    transport.send(message).map_err(|error| error.to_string())?;
    Ok(())
}

fn build_message(
    setting: &MailSetting,
    id: &str,
    body: &str,
    attachment_path: &Path,
) -> std::result::Result<Message, String> {
    crate::application::novel_access::require_private_path(attachment_path).map_err(|error| error.to_string())?;
    let from = parse_mailbox(&setting.from)?;
    let to = parse_mailboxes(&setting.to)?;
    let attachment_name = attachment_name(setting, id, attachment_path)?;
    let attachment_bytes = std::fs::read(attachment_path).map_err(|e| e.to_string())?;

    let mut builder = Message::builder()
        .from(from)
        .subject(setting.subject.clone());
    for mailbox in to {
        builder = builder.to(mailbox);
    }
    for mailbox in yaml_mailboxes(setting.extras.get("reply_to"))? {
        builder = builder.reply_to(mailbox);
    }
    for mailbox in yaml_mailboxes(setting.extras.get("cc"))? {
        builder = builder.cc(mailbox);
    }
    for mailbox in yaml_mailboxes(setting.extras.get("bcc"))? {
        builder = builder.bcc(mailbox);
    }

    builder
        .multipart(
            MultiPart::mixed()
                .singlepart(SinglePart::plain(body.to_string()))
                .singlepart(Attachment::new(attachment_name).body(
                    Body::new(attachment_bytes),
                    guess_attachment_content_type(attachment_path),
                )),
        )
        .map_err(|e| e.to_string())
}

fn send_mail_with_progress(
    setting: &MailSetting,
    id: &str,
    body: &str,
    attachment_path: &Path,
    interrupted: Option<&AtomicBool>,
) -> std::result::Result<(), String> {
    if interrupted.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err(MAIL_INTERRUPTED_MESSAGE.to_string());
    }
    if !setting.via.eq_ignore_ascii_case("smtp") {
        return Err(format!("unsupported mail via: {}", setting.via));
    }
    // Consume the private attachment before spawning. An interrupted caller
    // may drop its workspace immediately; the SMTP thread owns bytes, never
    // another operation's soon-to-be-cleaned temporary path.
    let message = build_message(setting, id, body, attachment_path)?;
    if interrupted.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err(MAIL_INTERRUPTED_MESSAGE.to_string());
    }
    let setting = setting.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().spawn(move || {
        let _ = tx.send(send_built_message(&setting, &message));
    }).map_err(|error| error.to_string())?;

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
    let authentication = smtp_auth_mechanisms(via_options.get("authentication"));
    let domain = yaml_string(via_options.get("domain")).filter(|value| !value.is_empty());
    let tls_mode = smtp_tls_mode(via_options, port)?;

    let mut builder = SmtpTransport::builder_dangerous(&host).port(port);
    if let Some(tls) = smtp_tls(via_options, tls_mode, domain.as_deref().unwrap_or(&host))? {
        builder = builder.tls(tls);
    }

    if !user_name.is_empty() {
        builder = builder.credentials(Credentials::new(user_name, password));
    }

    if let Some(domain) = domain {
        builder = builder.hello_name(ClientId::Domain(domain));
    }

    if !authentication.is_empty() {
        builder = builder.authentication(authentication);
    }

    if tls_mode == SmtpTlsMode::None {
        eprintln!("WARN: insecure plain SMTP is enabled by explicit opt-in");
    }

    Ok(builder.build())
}

fn smtp_tls_mode(
    via_options: &HashMap<String, serde_yaml::Value>,
    port: u16,
) -> std::result::Result<SmtpTlsMode, String> {
    if via_options.get("ssl").and_then(crate::setting_core::setting_bool_value).unwrap_or(false)
        || via_options.get("tls").and_then(crate::setting_core::setting_bool_value).unwrap_or(false)
        || port == 465
    {
        return Ok(SmtpTlsMode::Wrapper);
    }
    if via_options
        .get("enable_starttls")
        .and_then(crate::setting_core::setting_bool_value)
        .unwrap_or(false)
    {
        return Ok(SmtpTlsMode::Required);
    }
    if via_options
        .get("enable_starttls_auto")
        .and_then(crate::setting_core::setting_bool_value)
        .unwrap_or(true)
    {
        if smtp_allow_insecure(via_options) {
            eprintln!("WARN: opportunistic STARTTLS is enabled by explicit opt-in");
            return Ok(SmtpTlsMode::Opportunistic);
        }
        return Ok(SmtpTlsMode::Required);
    }
    if smtp_allow_insecure(via_options) {
        return Ok(SmtpTlsMode::None);
    }

    eprintln!(
        "WARN: plain SMTP / STARTTLS disabled in mail_setting.yaml without explicit opt-in"
    );
    Err(
        "安全でない SMTP 設定です。via_options.allow_insecure: true または local_setting の mail.smtp.allow_insecure: true を明示してください"
            .to_string(),
    )
}

fn smtp_tls(
    via_options: &HashMap<String, serde_yaml::Value>,
    mode: SmtpTlsMode,
    tls_domain: &str,
) -> std::result::Result<Option<Tls>, String> {
    if mode == SmtpTlsMode::None {
        return Ok(None);
    }

    let mut builder = TlsParametersBuilder::new(tls_domain.to_string());
    if smtp_accept_invalid_certs(via_options) {
        eprintln!("WARN: SMTP certificate verification is disabled by explicit opt-in");
        builder = builder
            .dangerous_accept_invalid_certs(true)
            .dangerous_accept_invalid_hostnames(true);
    }
    let params = builder.build().map_err(|e| e.to_string())?;
    let tls = match mode {
        SmtpTlsMode::None => unreachable!(),
        SmtpTlsMode::Opportunistic => Tls::Opportunistic(params),
        SmtpTlsMode::Required => Tls::Required(params),
        SmtpTlsMode::Wrapper => Tls::Wrapper(params),
    };
    Ok(Some(tls))
}

fn smtp_accept_invalid_certs(via_options: &HashMap<String, serde_yaml::Value>) -> bool {
    via_options.get("tls_skip_verify").and_then(crate::setting_core::setting_bool_value).unwrap_or(false)
        || crate::compat::load_local_setting_bool("mail.smtp.tls_skip_verify")
        || yaml_string(via_options.get("openssl_verify_mode"))
            .map(|mode| {
                matches!(
                    mode.trim_start_matches(':').to_ascii_lowercase().as_str(),
                    "none" | "verify_none"
                )
            })
            .unwrap_or(false)
}

fn smtp_allow_insecure(via_options: &HashMap<String, serde_yaml::Value>) -> bool {
    via_options.get("allow_insecure").and_then(crate::setting_core::setting_bool_value).unwrap_or(false)
        || crate::compat::load_local_setting_bool("mail.smtp.allow_insecure")
}

/// `narou mail` のレコード解決。探索順 (id → URL → ncode → title-only、
/// 一致した定義ごとに TOC URL を試す) は
/// `application::target_lookup::TargetLookupPlan::MAIL` が唯一の定義。
fn resolve_record(target: &str) -> std::result::Result<Option<NovelRecord>, String> {
    let lookup = crate::native::novel_repository::NativeNovelRepository::new();
    let site_settings = effective_site_settings();
    crate::application::target_lookup::resolve_target_record_sync(
        crate::application::target_lookup::TargetLookupPlan::MAIL,
        &HashMap::new(),
        crate::application::target_lookup::UrlSource::Settings(&site_settings),
        target,
        &lookup,
    )
    .map_err(|e| e.to_string())
}

/// Private materialized copies of a novel's generated ebooks. Paths live
/// only while the workspace does; callers hold the set, never clone paths
/// past its drop.
pub struct EbookFileSet {
    _workspace: tempfile::TempDir,
    paths: Vec<PathBuf>,
}

impl EbookFileSet {
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

impl std::ops::Deref for EbookFileSet {
    type Target = [PathBuf];

    fn deref(&self) -> &[PathBuf] {
        &self.paths
    }
}

impl<'a> IntoIterator for &'a EbookFileSet {
    type Item = &'a PathBuf;
    type IntoIter = std::slice::Iter<'a, PathBuf>;

    fn into_iter(self) -> Self::IntoIter {
        self.paths.iter()
    }
}

/// The output basename shared by every ebook naming rule (record → toc
/// fallback) so CLI and Web enumerate identical candidates.
fn ebook_base_name(
    record: &NovelRecord,
    settings: &NovelSettings,
) -> std::result::Result<String, String> {
    let toc = TocObject {
        title: record.title.clone(),
        author: record.author.clone(),
        toc_url: record.toc_url.clone(),
        story: None,
        subtitles: Vec::new(),
        novel_type: Some(record.novel_type),
    };
    let txt_name = create_output_text_filename(settings, record.id, &toc, Some(record));
    PathBuf::from(txt_name)
        .file_stem()
        .and_then(|stem| stem.to_str().map(|stem| stem.to_string()))
        .ok_or_else(|| "Invalid converted filename".to_string())
}

/// Ordered candidate names mirroring the legacy directory walk: the primary
/// name first (present or not), then `base_N.ext` while N is contiguous from
/// 2 and the name actually exists in storage.
fn planned_ebook_names(
    base: &str,
    ext: &str,
    present: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut names = vec![format!("{base}{ext}")];
    let mut index = 2usize;
    loop {
        let next = format!("{base}_{index}{ext}");
        if !present(&next) {
            break;
        }
        names.push(next);
        index += 1;
    }
    names
}

/// Select canonical objects that may be one of this novel's ebooks.
fn ebook_name_matches(base: &str, ext: &str, name: &str) -> bool {
    name.strip_prefix(base).is_some_and(|rest| {
        rest == ext || rest.strip_suffix(ext)
            .and_then(|rest| rest.strip_prefix('_'))
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

fn current_ebook_record(record: &NovelRecord, prefix: &crate::platform::ObjectPrefix) -> Result<NovelRecord> {
    let current = crate::native::novel_repository::NativeNovelRepository::new().get_current_sync(record.id.into())?
        .ok_or_else(|| crate::error::NarouError::NotFound(format!("ID: {}", record.id)))?;
    let keys = crate::platform::NovelObjectKeys::new(&current.sitename, &current.file_title, current.use_subdirectory)?;
    if keys.prefix().as_ref() != prefix.as_ref().trim_end_matches('/') {
        return Err(crate::error::NarouError::Platform("plan_stale: ebook metadata moved outside its original admission".into()));
    }
    Ok(current)
}

/// Ebook candidate `(name, present)` pairs under the caller's admitted read
/// scope. Settings come from a private metadata snapshot admitted inside the
/// same lease — no fresh epoch is captured and canonical paths are never
/// touched. `present` reports object existence on the canonical store, never
/// a filesystem mirror.
pub async fn ebook_file_candidates(
    scope: &mut crate::application::novel_access::NovelReadScope,
    record: &NovelRecord,
    ext: &str,
) -> Result<Vec<(String, bool)>> {
    let keys = crate::platform::NovelObjectKeys::new(&record.sitename, &record.file_title, record.use_subdirectory)?;
    let prefix = crate::platform::ObjectPrefix::from(keys.prefix().clone());
    let inventory = Inventory::with_default_root()?;
    let objects = crate::native::materialization::read_store(inventory.root_dir())?;
    let mut lease = scope.lease_for(&prefix).await?;
    let current = current_ebook_record(record, lease.prefix())?;
    lease.validate().await?;
    let record = &current;
    let inputs = crate::native::materialization::ConversionInputs::begin(
        &objects, &mut lease, inventory.root_dir(),
    ).await?;
    let settings = NovelSettings::load_for_novel(record.id, &record.title, &record.author, inputs.path())?;
    let base = ebook_base_name(record, &settings).map_err(crate::error::NarouError::Conversion)?;

    let prefix_text = prefix.as_ref().to_owned();
    let mut present = std::collections::BTreeSet::new();
    let mut request = crate::platform::ObjectListRequest::new(
        prefix, std::num::NonZeroUsize::new(100).expect("nonzero ebook listing page"),
    );
    loop {
        lease.validate().await?;
        let page = objects.list_page(&request).await?;
        for object in page.objects {
            let Some(name) = object.key.as_ref()
                .strip_prefix(&prefix_text)
                .and_then(|rest| rest.strip_prefix('/'))
            else {
                return Err(crate::error::NarouError::Platform("ebook listing key escaped its novel".to_string()));
            };
            if crate::platform::is_reserved_key(&object.key) || name.contains('/') {
                continue;
            }
            if ebook_name_matches(&base, ext, name) {
                present.insert(name.to_owned());
            }
        }
        match page.next_cursor {
            Some(cursor) => request = request.after(cursor),
            None => break,
        }
    }
    inputs.populate(&objects, &mut lease, false, None, crate::native::materialization::ConversionInputSelection {
        illustrations: false,
        cover: false,
    }).await?;
    Ok(planned_ebook_names(&base, ext, |name| present.contains(name))
        .into_iter()
        .map(|name| {
            let exists = present.contains(&name);
            (name, exists)
        })
        .collect())
}

/// Materialize generated ebooks under the caller's original writer epoch.
/// Settings and outputs are read privately; no canonical raw-path fallback.
pub fn get_ebook_file_paths(
    record: &NovelRecord,
    write: &crate::application::novel_access::NovelWriteGuard,
    ext: &str,
) -> std::result::Result<EbookFileSet, String> {
    crate::native::materialization::block_on(async {
        let inventory = Inventory::with_default_root()?;
        let objects = crate::native::materialization::read_store(inventory.root_dir())?;
        let mut lease = crate::application::novel_access::NovelReadLease::admit(
            &objects, write.prefix(), "native-ebook-files",
        ).await?;
        let result = async {
            if lease.epoch() != write.epoch() {
                return Err(crate::error::NarouError::Platform("novel_busy: ebook origin changed".into()));
            }
            let current = current_ebook_record(record, lease.prefix())?;
            lease.validate().await?;
            let record = &current;
            let inputs = crate::native::materialization::ConversionInputs::begin(&objects, &mut lease, inventory.root_dir()).await?;
            let settings = NovelSettings::load_for_novel(record.id, &record.title, &record.author, inputs.path())?;
            let base = ebook_base_name(record, &settings).map_err(crate::error::NarouError::Conversion)?;
            let (workspace, copied) = crate::native::materialization::selected_files_snapshot_leased(
                &objects, &mut lease, inventory.root_dir(), |name| ebook_name_matches(&base, ext, name),
            ).await?;
            inputs.populate(&objects, &mut lease, false, None, crate::native::materialization::ConversionInputSelection {
                illustrations: false,
                cover: false,
            }).await?;
            let names = planned_ebook_names(&base, ext, |name| copied.contains(name));
            let paths = names.iter().map(|name| workspace.path().join(name)).collect();
            Ok(EbookFileSet { _workspace: workspace, paths })
        }.await;
        let released = lease.release().await;
        match result { Ok(value) => { released?; Ok(value) }, Err(error) => Err(error) }
    }).map_err(|error| error.to_string())
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

fn update_last_mail_date(
    write: &crate::application::novel_access::NovelWriteGuard,
    id: i64,
) -> std::result::Result<(), String> {
    let found = crate::native::novel_repository::NativeNovelRepository::new()
        .update_existing_guarded_sync(write.fence(), id.into(), |record| {
            record.last_mail_date = Some(chrono::Utc::now());
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    if !found { return Err(format!("ID: {id}")); }
    Ok(())
}

fn current_device_ext() -> Option<String> {
    crate::compat::current_device().map(|device| device.ebook_file_ext().to_string())
}

/// 別名表 (alias.yaml) の完全一致解決を共有実装で行う。未登録の名前は
/// そのまま返す (小文字化しない) — `hotentry` 判定・`resolve_record` の前に
/// 適用する順序も従来どおり。
fn alias_to_target(target: &str) -> std::result::Result<String, String> {
    let aliases = crate::db::with_database(|db| -> Result<HashMap<String, String>> {
        let values: HashMap<String, serde_yaml::Value> = db
            .inventory()
            .load("alias", crate::db::inventory::InventoryScope::Local)?;
        Ok(crate::application::aliases::alias_map_from_values(values))
    })
    .map_err(|error| error.to_string())?;
    Ok(crate::application::aliases::resolve_alias_target(&aliases, target).into_owned())
}

fn attachment_name(
    setting: &MailSetting,
    id: &str,
    path: &Path,
) -> std::result::Result<String, String> {
    let original = original_attachment_name(id, path);
    let Some(pattern) = mail_attachment_pattern(setting) else {
        return Ok(original);
    };
    let replacement = mail_attachment_replacement(setting).unwrap_or_default();
    let regex = Regex::new(&pattern).map_err(|e| {
        format!(
            "{} の正規表現が不正です: {}",
            MAIL_ATTACHMENT_PATTERN_SETTING, e
        )
    })?;
    let replaced = regex.replace_all(&original, replacement.as_str()).to_string();
    if replaced.is_empty() {
        Ok(original)
    } else {
        Ok(replaced)
    }
}

fn original_attachment_name(id: &str, path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .unwrap_or_else(|| id.to_string())
}

fn mail_attachment_pattern(setting: &MailSetting) -> Option<String> {
    mail_extra_string(setting, MAIL_ATTACHMENT_PATTERN_KEYS)
        .or_else(|| crate::compat::load_local_setting_string(MAIL_ATTACHMENT_PATTERN_SETTING))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn mail_attachment_replacement(setting: &MailSetting) -> Option<String> {
    mail_extra_string(setting, MAIL_ATTACHMENT_REPLACEMENT_KEYS)
        .or_else(|| crate::compat::load_local_setting_string(MAIL_ATTACHMENT_REPLACEMENT_SETTING))
}

fn mail_extra_string(setting: &MailSetting, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| yaml_string(setting.extras.get(*key)))
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

fn parse_mailboxes(raw: &str) -> std::result::Result<Vec<Mailbox>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(parse_mailbox)
        .collect()
}

fn yaml_mailboxes(value: Option<&serde_yaml::Value>) -> std::result::Result<Vec<Mailbox>, String> {
    let mut mailboxes = Vec::new();
    for raw in yaml_string_list(value) {
        mailboxes.extend(parse_mailboxes(&raw)?);
    }
    Ok(mailboxes)
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

fn yaml_string_list(value: Option<&serde_yaml::Value>) -> Vec<String> {
    match value {
        Some(serde_yaml::Value::Sequence(values)) => values
            .iter()
            .filter_map(|v| yaml_value_to_string(Some(v)))
            .collect(),
        Some(value) => yaml_string(Some(value))
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(|v| v.to_string())
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

fn smtp_auth_mechanisms(value: Option<&serde_yaml::Value>) -> Vec<Mechanism> {
    yaml_string_list(value)
        .into_iter()
        .filter_map(
            |raw| match raw.trim_start_matches(':').to_ascii_lowercase().as_str() {
                "plain" => Some(Mechanism::Plain),
                "login" => Some(Mechanism::Login),
                "xoauth2" => Some(Mechanism::Xoauth2),
                _ => None,
            },
        )
        .collect()
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
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent() {
            candidates.push(parent.join("preset"));
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
    use super::{
        MAIL_INTERRUPTED_MESSAGE, MailSetting, SmtpTlsMode, attachment_name, build_message,
        build_transport, send_mail_with_progress, smtp_accept_invalid_certs, smtp_allow_insecure,
        smtp_auth_mechanisms, smtp_tls_mode,
    };
    use lettre::transport::smtp::authentication::Mechanism;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    fn with_ebook_library(run: impl FnOnce(&TempDir, &crate::db::NovelRecord, &crate::application::novel_access::NovelWriteGuard)) {
        struct DatabaseRestore(Option<crate::db::Database>);
        impl Drop for DatabaseRestore {
            fn drop(&mut self) { *crate::db::DATABASE.lock() = self.0.take(); }
        }
        let root = TempDir::new().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(root.path());
        let _database = DatabaseRestore(crate::db::DATABASE.lock().take());
        let _legacy = crate::test_support::legacy_yaml_guard();
        std::fs::create_dir(root.path().join(".narou")).unwrap();
        crate::db::init_database().unwrap();
        let record = serde_yaml::from_str("id: 1\ntitle: book\nauthor: author\nfile_title: book\nsitename: site\ntoc_url: https://example.test/book\nnovel_type: 2\nlast_update: 2026-01-01T00:00:00Z\n").unwrap();
        let (record, write) = crate::native::materialization::create_test_record(record).unwrap();
        run(&root, &record, &write);
    }

    #[test]
    fn ebook_files_use_private_copies_and_preserve_source_files() {
        with_ebook_library(|root, record, write| {
            let settings = crate::converter::settings::NovelSettings::load_for_novel(record.id, &record.title, &record.author, root.path()).unwrap();
            let base = super::ebook_base_name(record, &settings).unwrap();
            let key = crate::platform::ObjectKey::try_new(write.prefix().as_ref()).unwrap();
            crate::native::materialization::block_on(async {
                write.write_small(&key.join(format!("{base}.epub"))?, b"first ebook".to_vec()).await?;
                write.write_small(&key.join(format!("{base}_2.epub"))?, b"second ebook".to_vec()).await?;
                write.write_small(&key.join("user-notes.txt")?, b"keep notes".to_vec()).await?;
                write.write_small(&key.join("other-output.epub")?, b"keep output".to_vec()).await
            }).unwrap();
            let directory = crate::native::materialization::directory_for_origin(write).unwrap();
            let files = super::get_ebook_file_paths(record, write, ".epub").unwrap();
            assert_eq!(files.len(), 2);
            assert_eq!(std::fs::read(&files[0]).unwrap(), b"first ebook");
            assert_eq!(std::fs::read(&files[1]).unwrap(), b"second ebook");
            assert!(!files[0].starts_with(&directory));
            assert_eq!(std::fs::metadata(&files[0]).unwrap().modified().unwrap(),
                std::fs::metadata(directory.join(format!("{base}.epub"))).unwrap().modified().unwrap());
            let private = files[0].clone();
            drop(files);
            assert!(!private.exists());
            assert_eq!(std::fs::read(directory.join("user-notes.txt")).unwrap(), b"keep notes");
            assert_eq!(std::fs::read(directory.join("other-output.epub")).unwrap(), b"keep output");
            assert_eq!(std::fs::read(directory.join(format!("{base}.epub"))).unwrap(), b"first ebook");
        });
    }

    #[test]
    fn ebook_and_mail_date_never_upgrade_stale_original_epochs() {
        use crate::platform::NovelAccessFence;
        with_ebook_library(|root, record, write| {
            let stores = crate::native::object_store::NativeStores::for_narou_root(root.path()).unwrap();
            let mut reads = crate::application::novel_access::NovelReadScope::new(&stores.objects, "ebook-stale-test");
            crate::native::materialization::block_on(async {
                drop(reads.lease_for(write.prefix()).await?);
                let epoch = stores.primary.close_gate(write.prefix(), "ebook-new-publisher").await?;
                stores.primary.open_gate(write.prefix(), "ebook-new-publisher", epoch).await
            }).unwrap();
            assert!(super::get_ebook_file_paths(record, write, ".epub").is_err());
            assert!(super::update_last_mail_date(write, record.id).is_err());
            crate::native::materialization::block_on(async {
                assert!(super::ebook_file_candidates(&mut reads, record, ".epub").await.is_err());
                reads.release_all().await
            }).unwrap();
            assert!(crate::native::novel_repository::NativeNovelRepository::new()
                .get_current_sync(record.id.into()).unwrap().unwrap().last_mail_date.is_none());
        });
    }

    #[test]
    fn ebook_settings_io_failure_is_not_a_missing_file_success() {
        with_ebook_library(|root, record, write| {
            let objects = crate::native::materialization::read_store(root.path()).unwrap();
            let mut reads = crate::application::novel_access::NovelReadScope::new(&objects, "ebook-settings-error");
            let setting = root.path().join(".narou/local_setting.yaml");
            if setting.try_exists().unwrap() { std::fs::remove_file(&setting).unwrap(); }
            std::fs::create_dir(&setting).unwrap();
            assert!(super::get_ebook_file_paths(record, write, ".epub").is_err());
            crate::native::materialization::block_on(async {
                assert!(super::ebook_file_candidates(&mut reads, record, ".epub").await.is_err());
                reads.release_all().await
            }).unwrap();
        });
    }

    fn test_mail_setting(extras: HashMap<String, serde_yaml::Value>) -> MailSetting {
        MailSetting {
            from: "sender@example.com".to_string(),
            to: "receiver@example.com".to_string(),
            subject: "subject".to_string(),
            via: "smtp".to_string(),
            via_options: HashMap::new(),
            extras,
        }
    }

    #[test]
    fn attachment_name_keeps_original_filename() {
        let setting = test_mail_setting(HashMap::new());
        let path = std::path::Path::new("example.kepub.epub");
        assert_eq!(
            attachment_name(&setting, "hotentry", path).unwrap(),
            "example.kepub.epub"
        );
    }

    #[test]
    fn attachment_name_falls_back_to_id_when_path_has_no_name() {
        let setting = test_mail_setting(HashMap::new());
        let path = std::path::Path::new("");
        assert_eq!(attachment_name(&setting, "1", path).unwrap(), "1");
    }

    #[test]
    fn attachment_name_applies_regex_replacement() {
        let mut extras = HashMap::new();
        extras.insert(
            "attachment_filename_pattern".to_string(),
            serde_yaml::Value::String(r"^\[[^\]]+\](.*)$".to_string()),
        );
        extras.insert(
            "attachment_filename_replacement".to_string(),
            serde_yaml::Value::String("$1".to_string()),
        );
        let setting = test_mail_setting(extras);
        let path = std::path::Path::new("[very long author]ReadableTitle.epub");

        assert_eq!(
            attachment_name(&setting, "1", path).unwrap(),
            "ReadableTitle.epub"
        );
    }

    #[test]
    fn attachment_name_reports_invalid_regex() {
        let mut extras = HashMap::new();
        extras.insert(
            "attachment_filename_pattern".to_string(),
            serde_yaml::Value::String("[".to_string()),
        );
        let setting = test_mail_setting(extras);
        let path = std::path::Path::new("example.epub");

        let err = attachment_name(&setting, "1", path).unwrap_err();
        assert!(err.contains("mail.attachment-filename-pattern"));
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

        let err = send_mail_with_progress(&setting, "1", "body", &attachment, Some(&interrupted))
            .unwrap_err();

        assert_eq!(err, MAIL_INTERRUPTED_MESSAGE);
    }

    #[test]
    fn smtp_auth_mechanisms_accept_ruby_symbols_and_lists() {
        assert_eq!(
            smtp_auth_mechanisms(Some(&serde_yaml::Value::String(":login".to_string()))),
            vec![Mechanism::Login]
        );
        assert_eq!(
            smtp_auth_mechanisms(Some(&serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String(":plain".to_string()),
                serde_yaml::Value::String("xoauth2".to_string()),
            ]))),
            vec![Mechanism::Plain, Mechanism::Xoauth2]
        );
    }

    #[test]
    fn build_transport_accepts_domain_and_login_authentication() {
        let mut via_options = HashMap::new();
        via_options.insert(
            "address".to_string(),
            serde_yaml::Value::String("smtp.example.com".to_string()),
        );
        via_options.insert(
            "port".to_string(),
            serde_yaml::Value::Number(serde_yaml::Number::from(587)),
        );
        via_options.insert(
            "enable_starttls".to_string(),
            serde_yaml::Value::Bool(false),
        );
        via_options.insert(
            "user_name".to_string(),
            serde_yaml::Value::String("user".to_string()),
        );
        via_options.insert(
            "password".to_string(),
            serde_yaml::Value::String("secret".to_string()),
        );
        via_options.insert(
            "authentication".to_string(),
            serde_yaml::Value::String(":login".to_string()),
        );
        via_options.insert(
            "domain".to_string(),
            serde_yaml::Value::String("example.com".to_string()),
        );

        let setting = MailSetting {
            from: "sender@example.com".to_string(),
            to: "receiver@example.com".to_string(),
            subject: "subject".to_string(),
            via: "smtp".to_string(),
            via_options,
            extras: HashMap::new(),
        };

        build_transport(&setting).unwrap();
    }

    #[test]
    fn smtp_tls_mode_matches_pony_style_flags() {
        let mut via_options = HashMap::new();
        assert_eq!(smtp_tls_mode(&via_options, 587).unwrap(), SmtpTlsMode::Required);

        via_options.insert("enable_starttls".to_string(), serde_yaml::Value::Bool(true));
        assert_eq!(smtp_tls_mode(&via_options, 587).unwrap(), SmtpTlsMode::Required);

        via_options.remove("enable_starttls");
        via_options.insert("ssl".to_string(), serde_yaml::Value::Bool(true));
        assert_eq!(smtp_tls_mode(&via_options, 587).unwrap(), SmtpTlsMode::Wrapper);

        assert_eq!(smtp_tls_mode(&HashMap::new(), 465).unwrap(), SmtpTlsMode::Wrapper);
    }

    #[test]
    fn smtp_accept_invalid_certs_recognizes_verify_none() {
        let mut via_options = HashMap::new();
        via_options.insert(
            "openssl_verify_mode".to_string(),
            serde_yaml::Value::String(":none".to_string()),
        );

        assert!(smtp_accept_invalid_certs(&via_options));
    }

    #[test]
    fn smtp_tls_mode_rejects_plain_without_opt_in() {
        let mut via_options = HashMap::new();
        via_options.insert(
            "enable_starttls_auto".to_string(),
            serde_yaml::Value::Bool(false),
        );

        assert!(smtp_tls_mode(&via_options, 587).is_err());
    }

    #[test]
    fn smtp_allow_insecure_accepts_explicit_opt_in() {
        let mut via_options = HashMap::new();
        via_options.insert("allow_insecure".to_string(), serde_yaml::Value::Bool(true));

        assert!(smtp_allow_insecure(&via_options));
        assert_eq!(
            smtp_tls_mode(&via_options, 587).unwrap(),
            SmtpTlsMode::Opportunistic
        );

        via_options.insert(
            "enable_starttls_auto".to_string(),
            serde_yaml::Value::Bool(false),
        );
        assert_eq!(smtp_tls_mode(&via_options, 587).unwrap(), SmtpTlsMode::None);
    }

    #[test]
    fn build_message_applies_reply_to_cc_and_bcc_headers() {
        let tmp = TempDir::new().unwrap();
        let attachment = tmp.path().join("sample.epub");
        std::fs::write(&attachment, "dummy").unwrap();

        let mut extras = HashMap::new();
        extras.insert(
            "reply_to".to_string(),
            serde_yaml::Value::String("reply@example.com".to_string()),
        );
        extras.insert(
            "cc".to_string(),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("cc1@example.com".to_string()),
                serde_yaml::Value::String("cc2@example.com".to_string()),
            ]),
        );
        extras.insert(
            "bcc".to_string(),
            serde_yaml::Value::String("bcc@example.com".to_string()),
        );

        let setting = MailSetting {
            from: "sender@example.com".to_string(),
            to: "to1@example.com, to2@example.com".to_string(),
            subject: "subject".to_string(),
            via: "smtp".to_string(),
            via_options: HashMap::new(),
            extras,
        };

        let message = build_message(&setting, "1", "body", &attachment).unwrap();
        let formatted = String::from_utf8(message.formatted()).unwrap();

        assert!(formatted.contains("To: to1@example.com"));
        assert!(formatted.contains("to2@example.com"));
        assert!(formatted.contains("Reply-To: reply@example.com"));
        assert!(formatted.contains("Cc: cc1@example.com"));
        assert!(formatted.contains("cc2@example.com"));
        assert!(formatted.contains("Content-Type: application/epub+zip"));
    }
}
