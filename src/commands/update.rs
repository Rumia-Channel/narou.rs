use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

use narou_rs::compat::{
    convert_existing_novel, current_device, load_local_setting_bool, load_local_setting_string,
    load_local_setting_value, relay_web_stream_to_console, yaml_value_to_string,
};
use narou_rs::converter::NovelConverter;
use narou_rs::converter::device::{Device, OutputManager};
use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::db::NovelRecord;
use narou_rs::db::inventory::InventoryScope;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::{
    DownloadResult, Downloader, SubtitleInfo, TargetType, TocFile, TocObject, UpdateStatus,
};
use narou_rs::mail::{
    MailSettingLoadError, ensure_mail_setting_file, load_mail_setting, send_target_with_setting,
};
use narou_rs::progress::{CliProgress, WebProgress, is_web_mode};
use narou_rs::termcolor::{bold_colored, colored};

const MODIFIED_TAG: &str = "modified";
const INTERVAL_MIN_SECS: f64 = 2.5;
const FORCE_WAIT_SECS: f64 = 2.0;
const HOTENTRY_DIR_NAME: &str = "hotentry";
const HOTENTRY_FOOTER: &str = "［＃ここから地付き］［＃小書き］（本を読み終わりました）［＃小書き終わり］［＃ここで地付き終わり］";

static UPDATE_INTERRUPT_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
struct UpdateInterrupted;

const UPDATE_SORT_KEYS: &[(&str, &str)] = &[
    ("id", "ID"),
    ("last_update", "更新日"),
    ("title", "タイトル"),
    ("author", "作者名"),
    ("new_arrivals_date", "新着日"),
    ("general_lastup", "最新話掲載日"),
];

pub struct UpdateOptions {
    pub ids: Option<Vec<String>>,
    pub force: bool,
    pub no_convert: bool,
    pub convert_only_new_arrival: bool,
    pub gl: Option<Option<String>>,
    pub sort_by: Option<String>,
    pub ignore_all: bool,
    pub user_agent: Option<String>,
}

pub fn cmd_update(opts: UpdateOptions) {
    let result = std::thread::spawn(move || -> std::result::Result<(), UpdateInterrupted> {
        if let Err(e) = narou_rs::db::init_database() {
            eprintln!("Error initializing database: {}", e);
            std::process::exit(1);
        }

        let interrupted = match update_interrupt_flag() {
            Ok(flag) => flag,
            Err(code) => std::process::exit(code),
        };
        interrupted.store(false, AtomicOrdering::SeqCst);

        repair_empty_titles();

        if let Some(ref gl_opt) = opts.gl {
            update_general_lastup(gl_opt.as_deref(), opts.user_agent.as_deref());
            return Ok(());
        }

        let setting_sort_by = load_local_setting_string("update.sort-by");
        let sort_by = resolve_sort_key(opts.sort_by.as_deref().or(setting_sort_by.as_deref()));

        let convert_only_new_arrival = opts.convert_only_new_arrival
            || load_local_setting_bool("update.convert-only-new-arrival");
        let interval_secs = load_setting_float("update.interval", INTERVAL_MIN_SECS);

        let stdin_targets = read_targets_from_stdin();
        let merged_ids = merge_cli_and_stdin_targets(opts.ids, stdin_targets);
        let is_bulk = merged_ids.is_none();
        let (target_ids, unresolved_count) =
            resolve_targets(merged_ids.as_deref(), opts.ignore_all, sort_by.as_deref());
        if target_ids.is_empty() {
            if unresolved_count > 0 {
                std::process::exit(unresolved_count.min(127) as i32);
            }
            return Ok(());
        }

        let _total = target_ids.len();
        let mut mistook = unresolved_count;

        let mut downloader = match Downloader::with_user_agent(opts.user_agent.as_deref()) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Error creating downloader: {}", e);
                std::process::exit(1);
            }
        };

        let multi = CliProgress::multi();
        let multi_clone = multi.clone();
        let hotentry_enabled = load_local_setting_bool("hotentry");
        let mut hotentries: HashMap<i64, Vec<SubtitleInfo>> = HashMap::new();

        let mut last_time = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs_f64(interval_secs))
            .unwrap_or_else(std::time::Instant::now);

        for (i, &id) in target_ids.iter().enumerate() {
            abort_if_interrupted(interrupted.as_ref())?;

            if i > 0 {
                println!("{}", "\u{2015}".repeat(35));
            }

            let frozen = !opts.force && is_novel_frozen(id);
            if frozen {
                if is_bulk {
                    continue;
                }
                let title = get_novel_title(id);
                println!("ID:{}　{} は凍結中です", id, title);
                mistook += 1;
                continue;
            }

            let elapsed = last_time.elapsed().as_secs_f64();
            if elapsed < interval_secs {
                sleep_with_interrupt(interval_secs - elapsed, interrupted.as_ref())?;
            }
            last_time = std::time::Instant::now();

            let progress: Box<dyn narou_rs::progress::ProgressReporter> = if is_web_mode() {
                Box::new(WebProgress::new("update"))
            } else {
                Box::new(CliProgress::with_multi(&format!("DL {}", id), multi_clone.clone()))
            };
            downloader.set_progress(progress);

            match downloader.download_novel(&id.to_string()) {
                Ok(dl) => {
                    print_status_messages(&dl);

                    if hotentry_enabled && !dl.new_arrival_subtitles.is_empty() {
                        hotentries
                            .entry(dl.id)
                            .or_default()
                            .extend(dl.new_arrival_subtitles.clone());
                    }

                    let new_arrivals = dl.new_arrivals;
                    let has_convert_failure = narou_rs::db::with_database(|db| {
                        Ok(db.get(dl.id).map(|r| r.convert_failure).unwrap_or(false))
                    })
                    .unwrap_or(false);

                    match dl.status {
                        UpdateStatus::Ok => {
                            remove_modified_tag(dl.id);
                            update_last_check_date(dl.id);
                            sync_end_tag(dl.id);
                            if opts.no_convert {
                                std::thread::sleep(std::time::Duration::from_secs_f64(
                                    FORCE_WAIT_SECS,
                                ));
                                continue;
                            }

                            if convert_only_new_arrival && !new_arrivals {
                                std::thread::sleep(std::time::Duration::from_secs_f64(
                                    FORCE_WAIT_SECS,
                                ));
                                continue;
                            }
                        }
                        UpdateStatus::None => {
                            remove_modified_tag(dl.id);
                            update_last_check_date(dl.id);
                            sync_end_tag(dl.id);
                            if !has_convert_failure {
                                continue;
                            }
                        }
                        UpdateStatus::Failed => {
                            mistook += 1;
                            continue;
                        }
                        UpdateStatus::Canceled => {
                            let title = if dl.title.is_empty() {
                                get_novel_title(id)
                            } else {
                                dl.title.clone()
                            };
                            println!(
                                "ID:{}　{} の更新はキャンセルされました",
                                id, title
                            );
                            mistook += 1;
                            continue;
                        }
                    }

                    if has_convert_failure {
                        println!("{}", colored("前回変換できなかったので再変換します", "yellow"));
                    }

                    match auto_convert(&dl, is_bulk) {
                        Ok(()) => {
                            clear_convert_failure(dl.id);
                        }
                        Err(e) => {
                            println!("  Convert error: {}", e);
                            set_convert_failure(dl.id);
                            mistook += 1;
                        }
                    }
                }
                Err(e) => {
                    if matches!(e, narou_rs::error::NarouError::SuspendDownload(_)) {
                        return Err(UpdateInterrupted);
                    }
                    let title = get_novel_title(id);
                    println!("ID:{}　{} の更新は失敗しました", id, title);
                    mistook += 1;
                }
            }
        }

        let _ = narou_rs::db::with_database_mut(|db| db.save());

        if hotentry_enabled {
            abort_if_interrupted(interrupted.as_ref())?;
            if let Err(e) = process_hotentry(&hotentries) {
                println!("hotentry の処理に失敗しました\n  {}", e);
                mistook += 1;
            }
        }

        if mistook > 0 {
            println!("\n{} 件のエラーが発生しました", mistook);
        }
        drop(multi);

        if mistook > 0 {
            std::process::exit(mistook.min(127) as i32);
        }
        Ok(())
    })
    .join();

    match result {
        Ok(Ok(())) => {}
        Ok(Err(UpdateInterrupted)) => {
            println!("アップデートを中断しました");
            std::process::exit(126);
        }
        Err(e) => {
            if let Some(s) = e.downcast_ref::<String>() {
                println!("アップデートを中断しました: {}", s);
            } else {
                println!("アップデートを中断しました");
            }
            std::process::exit(126);
        }
    }
}

fn update_interrupt_flag() -> Result<Arc<AtomicBool>, i32> {
    if let Some(flag) = UPDATE_INTERRUPT_FLAG.get() {
        return Ok(flag.clone());
    }

    let flag = Arc::new(AtomicBool::new(false));
    let handler_flag = flag.clone();
    ctrlc::set_handler(move || {
        handler_flag.store(true, AtomicOrdering::SeqCst);
    })
    .map_err(|e| {
        eprintln!("Error: {}", e);
        1
    })?;

    let _ = UPDATE_INTERRUPT_FLAG.set(flag.clone());
    Ok(flag)
}

fn abort_if_interrupted(interrupted: &AtomicBool) -> std::result::Result<(), UpdateInterrupted> {
    if interrupted.load(AtomicOrdering::SeqCst) {
        Err(UpdateInterrupted)
    } else {
        Ok(())
    }
}

fn sleep_with_interrupt(
    wait_secs: f64,
    interrupted: &AtomicBool,
) -> std::result::Result<(), UpdateInterrupted> {
    let total = std::time::Duration::from_secs_f64(wait_secs.max(0.0));
    let deadline = std::time::Instant::now() + total;
    loop {
        abort_if_interrupted(interrupted)?;
        let now = std::time::Instant::now();
        if now >= deadline {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(now);
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(100)));
    }
}

fn resolve_targets(
    ids: Option<&[String]>,
    ignore_all: bool,
    sort_by: Option<&str>,
) -> (Vec<i64>, usize) {
    if let Some(targets) = ids {
        let mut resolved = Vec::new();
        let mut unresolved_count = 0usize;
        let site_settings = SiteSetting::load_all().unwrap_or_default();
        for target in expand_tag_targets(targets) {
            if let Some(id) = resolve_target_to_id(&target, &site_settings) {
                if !resolved.contains(&id) {
                    resolved.push(id);
                }
            } else {
                eprintln!("{} {} は管理小説の中に存在しません", bold_colored("[ERROR]", "red"), target);
                unresolved_count += 1;
            }
        }

        if let Some(key) = sort_by {
            sort_update_ids_by_key(&mut resolved, key);
        }

        return (resolved, unresolved_count);
    }

    if ignore_all {
        return (Vec::new(), 0);
    }

    let mut all_ids = narou_rs::db::with_database(|db| Ok(db.ids())).unwrap_or_default();
    if let Some(key) = sort_by {
        sort_update_ids_by_key(&mut all_ids, key);
    }
    (all_ids, 0)
}

fn read_targets_from_stdin() -> Vec<String> {
    if std::io::stdin().is_terminal() {
        return Vec::new();
    }

    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        return Vec::new();
    }

    input
        .split_whitespace()
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect()
}

fn merge_cli_and_stdin_targets(
    ids: Option<Vec<String>>,
    stdin_targets: Vec<String>,
) -> Option<Vec<String>> {
    match (ids, stdin_targets.is_empty()) {
        (Some(mut ids), false) => {
            ids.extend(stdin_targets);
            Some(ids)
        }
        (Some(ids), true) => Some(ids),
        (None, false) => Some(stdin_targets),
        (None, true) => None,
    }
}

fn resolve_sort_key(key: Option<&str>) -> Option<String> {
    let key = key?;
    let key_lower = key.to_lowercase();
    if UPDATE_SORT_KEYS.iter().any(|(k, _)| *k == key_lower) {
        return Some(key_lower);
    }
    let summaries = UPDATE_SORT_KEYS
        .iter()
        .map(|(k, v)| format!("  {:>20}   {}", k, v))
        .collect::<Vec<_>>()
        .join("\n");
    eprintln!(
        "{} は正しいキーではありません。次の中から選択して下さい\n{}",
        key, summaries
    );
    std::process::exit(127);
}

fn expand_tag_targets(targets: &[String]) -> Vec<String> {
    let (tag_index, mut all_ids) = narou_rs::db::with_database(|db| {
        Ok::<_, narou_rs::error::NarouError>((db.tag_index(), db.ids()))
    })
    .unwrap_or_default();
    all_ids.sort_unstable();

    let mut expanded = Vec::new();
    for target in targets {
        if let Ok(id) = target.parse::<i64>() {
            let exists =
                narou_rs::db::with_database(|db| Ok(db.get(id).is_some())).unwrap_or(false);
            if exists {
                expanded.push(id.to_string());
                continue;
            }
        }

        if let Some(tag_name) = target.strip_prefix("^tag:") {
            if let Some(exclude_ids) = tag_index.get(tag_name) {
                let exclude: HashSet<i64> = exclude_ids.iter().copied().collect();
                expanded.extend(
                    all_ids
                        .iter()
                        .filter(|id| !exclude.contains(id))
                        .map(|id| id.to_string()),
                );
            } else {
                expanded.push(tag_name.to_string());
            }
        } else if let Some(tag_name) = target.strip_prefix("tag:") {
            if let Some(ids) = tag_index.get(tag_name) {
                expanded.extend(ids.iter().map(|id| id.to_string()));
            } else {
                expanded.push(tag_name.to_string());
            }
        } else if let Some(ids) = tag_index.get(target) {
            expanded.extend(ids.iter().map(|id| id.to_string()));
        } else {
            expanded.push(target.clone());
        }
    }

    let mut seen = HashSet::new();
    expanded
        .into_iter()
        .filter(|target| seen.insert(target.clone()))
        .collect()
}

fn resolve_target_to_id(target: &str, site_settings: &[SiteSetting]) -> Option<i64> {
    let target = alias_to_target(target);
    if let Ok(i) = target.parse::<i64>() {
        let exists = narou_rs::db::with_database(|db| Ok(db.get(i).is_some())).unwrap_or(false);
        if exists {
            return Some(i);
        }
    }

    match Downloader::get_target_type(&target) {
        TargetType::Url => resolve_url_to_id(&target, site_settings),
        TargetType::Ncode => resolve_ncode_to_id(&target),
        TargetType::Id => None,
        TargetType::Other => {
            narou_rs::db::with_database(|db| Ok(db.find_by_title(&target).map(|r| r.id)))
                .ok()
                .flatten()
        }
    }
}

fn resolve_url_to_id(target: &str, site_settings: &[SiteSetting]) -> Option<i64> {
    let setting = site_settings.iter().find(|s| s.matches_url(target))?;
    let toc_url = setting.toc_url_with_url_captures(target)?;
    narou_rs::db::with_database(|db| Ok(db.get_by_toc_url(&toc_url).map(|r| r.id)))
        .ok()
        .flatten()
}

fn resolve_ncode_to_id(target: &str) -> Option<i64> {
    let ncode = target.to_lowercase();
    narou_rs::db::with_database(|db| {
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
            .map(|r| r.id))
    })
    .ok()
    .flatten()
}

fn alias_to_target(target: &str) -> String {
    let alias = narou_rs::db::with_database(|db| {
        let aliases: HashMap<String, serde_yaml::Value> =
            db.inventory().load("alias", InventoryScope::Local)?;
        Ok(aliases.get(target).and_then(yaml_value_to_string))
    })
    .ok()
    .flatten();
    alias.unwrap_or_else(|| {
        if target.chars().all(|c| c.is_ascii_digit()) {
            target.to_string()
        } else {
            target.to_lowercase()
        }
    })
}

fn sort_update_ids_by_key(ids: &mut [i64], key: &str) {
    let records = narou_rs::db::with_database(|db| {
        Ok(ids
            .iter()
            .filter_map(|id| db.get(*id).cloned())
            .map(|r| (r.id, r))
            .collect::<HashMap<_, _>>())
    })
    .unwrap_or_default();

    ids.sort_by(|a, b| {
        let Some(a_record) = records.get(a) else {
            return Ordering::Greater;
        };
        let Some(b_record) = records.get(b) else {
            return Ordering::Less;
        };
        match key {
            "id" => a_record.id.cmp(&b_record.id),
            "title" => a_record
                .title
                .to_lowercase()
                .cmp(&b_record.title.to_lowercase()),
            "author" => a_record
                .author
                .to_lowercase()
                .cmp(&b_record.author.to_lowercase()),
            "last_update" => b_record.last_update.cmp(&a_record.last_update),
            "new_arrivals_date" => b_record.new_arrivals_date.cmp(&a_record.new_arrivals_date),
            "general_lastup" => b_record.general_lastup.cmp(&a_record.general_lastup),
            _ => Ordering::Equal,
        }
    });
}

fn is_novel_frozen(id: i64) -> bool {
    if load_frozen_ids().contains(&id) {
        return true;
    }

    narou_rs::db::with_database(|db| {
        Ok(db
            .get(id)
            .map(|r| r.tags.contains(&"frozen".to_string()))
            .unwrap_or(false))
    })
    .unwrap_or(false)
}

fn load_frozen_ids() -> HashSet<i64> {
    narou_rs::db::with_database(|db| {
        let frozen: HashMap<i64, serde_yaml::Value> =
            db.inventory().load("freeze", InventoryScope::Local)?;
        Ok(frozen.keys().copied().collect())
    })
    .unwrap_or_default()
}

fn get_novel_title(id: i64) -> String {
    narou_rs::db::with_database(|db| Ok(db.get(id).map(|r| r.title.clone()).unwrap_or_default()))
        .unwrap_or_default()
}

fn repair_empty_titles() {
    let _ = narou_rs::db::with_database_mut(|db| {
        let records: Vec<(i64, String, String)> = db
            .all_records()
            .values()
            .filter(|r| r.title.is_empty() || r.author.is_empty())
            .filter_map(|r| {
                let novel_dir = narou_rs::db::novel_dir_for_record(db.archive_root(), r);
                let toc_path = novel_dir.join("toc.yaml");
                if let Ok(toc_content) = std::fs::read_to_string(&toc_path) {
                    if let Ok(toc) =
                        serde_yaml::from_str::<narou_rs::downloader::TocFile>(&toc_content)
                    {
                        if !toc.title.is_empty() || !toc.author.is_empty() {
                            return Some((r.id, toc.title, toc.author));
                        }
                    }
                }
                let title = extract_title_from_file_title(&r.file_title);
                if !title.is_empty() {
                    return Some((r.id, title, String::new()));
                }
                None
            })
            .collect();
        let mut fixed = false;
        for (id, title, author) in &records {
            if let Some(record) = db.get(*id).cloned() {
                let mut r = record;
                let mut changed = false;
                if r.title.is_empty() && !title.is_empty() {
                    r.title = title.clone();
                    changed = true;
                }
                if r.author.is_empty() && !author.is_empty() {
                    r.author = author.clone();
                    changed = true;
                }
                if changed {
                    db.insert(r);
                    fixed = true;
                }
            }
        }
        if fixed {
            let _ = db.save();
        }
        Ok::<(), narou_rs::error::NarouError>(())
    });
}

fn extract_title_from_file_title(file_title: &str) -> String {
    if let Some(space_pos) = file_title.find(' ') {
        file_title[space_pos + 1..].to_string()
    } else {
        String::new()
    }
}

fn remove_modified_tag(id: i64) {
    let _ = narou_rs::db::with_database_mut(|db| {
        if let Some(record) = db.get(id).cloned() {
            let mut r = record;
            if r.tags.contains(&MODIFIED_TAG.to_string()) {
                r.tags.retain(|t| t != MODIFIED_TAG);
                db.insert(r);
            }
        }
        Ok(())
    });
}

fn update_last_check_date(id: i64) {
    let _ = narou_rs::db::with_database_mut(|db| {
        if let Some(record) = db.get(id).cloned() {
            let mut r = record;
            r.last_check_date = Some(Utc::now());
            db.insert(r);
        }
        Ok(())
    });
}

fn apply_general_lastup_check_result(
    record: &mut NovelRecord,
    novelupdated_at: Option<DateTime<Utc>>,
    general_lastup: Option<DateTime<Utc>>,
    length: Option<i64>,
    is_end: Option<bool>,
    now: DateTime<Utc>,
) {
    let last_check = record.last_check_date.or(Some(record.last_update));

    if let Some(nu) = novelupdated_at {
        if let Some(lc) = last_check
            && nu > lc
            && !record.tags.iter().any(|tag| tag == MODIFIED_TAG)
        {
            record.tags.push(MODIFIED_TAG.to_string());
        }
        record.novelupdated_at = Some(nu);
    }

    if let Some(gl) = general_lastup {
        record.general_lastup = Some(gl);
    }

    if let Some(len) = length {
        record.length = Some(len);
    }

    if let Some(end) = is_end {
        record.end = end;
    }

    record.last_check_date = Some(now);
}

fn sync_end_tag(id: i64) {
    let _ = narou_rs::db::with_database_mut(|db| {
        if let Some(record) = db.get(id).cloned() {
            let mut r = record;
            let had_end = r.tags.iter().any(|tag| tag == "end");
            if r.end && !had_end {
                r.tags.push("end".to_string());
                db.insert(r);
            } else if !r.end && had_end {
                r.tags.retain(|tag| tag != "end");
                db.insert(r);
            }
        }
        Ok(())
    });
}

fn set_convert_failure(id: i64) {
    let _ = narou_rs::db::with_database_mut(|db| {
        if let Some(record) = db.get(id).cloned() {
            let mut r = record;
            r.convert_failure = true;
            db.insert(r);
        }
        Ok(())
    });
}

fn clear_convert_failure(id: i64) {
    let _ = narou_rs::db::with_database_mut(|db| {
        if let Some(record) = db.get(id).cloned() {
            let mut r = record;
            r.convert_failure = false;
            db.insert(r);
        }
        Ok(())
    });
}

fn print_status_messages(dl: &DownloadResult) {
    match dl.status {
        UpdateStatus::Ok => {
            if dl.new_novel {
                println!(
                    "{} のDL完了 (ID:{}, {}セクション)",
                    dl.title, dl.id, dl.total_count
                );
            } else if dl.sections_deleted {
                println!(
                    "ID:{}　{} は一部の話が削除されています",
                    dl.id, dl.title
                );
            } else if dl.updated_count > 0 {
                println!("{} の更新が完了しました", dl.title);
            } else if dl.title_changed {
                println!(
                    "ID:{}　{} のタイトルが更新されています",
                    dl.id, dl.title
                );
            } else if dl.story_changed {
                println!(
                    "ID:{}　{} のあらすじが更新されています",
                    dl.id, dl.title
                );
            } else if dl.author_changed {
                println!(
                    "ID:{}　{} の作者名が更新されています",
                    dl.id, dl.title
                );
            }
        }
        UpdateStatus::None => {
            println!("{} に更新はありません", dl.title);
        }
        UpdateStatus::Canceled => {}
        UpdateStatus::Failed => {}
    }
}

fn auto_convert(
    dl: &DownloadResult,
    no_open: bool,
) -> Result<(), String> {
    if is_web_mode() {
        return auto_convert_via_web_subprocess(dl.id, no_open);
    }
    convert_existing_novel(dl.id, &dl.title, &dl.author, &dl.novel_dir, no_open).map(|_| ())
}

fn auto_convert_via_web_subprocess(id: i64, no_open: bool) -> Result<(), String> {
    let exe_path = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(exe_path);
    command.arg("convert");
    if no_open {
        command.arg("--no-open");
    }
    command.arg(id.to_string());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "convert stdout を取得できません".to_string())?;
    let stderr = child.stderr.take().ok_or_else(|| "convert stderr を取得できません".to_string())?;

    let stdout_thread = std::thread::spawn(move || relay_web_convert_stream(stdout));
    let stderr_thread = std::thread::spawn(move || relay_web_convert_stream(stderr));

    let status = child.wait().map_err(|e| e.to_string())?;
    stdout_thread
        .join()
        .map_err(|_| "convert stdout relay thread が panic しました".to_string())??;
    stderr_thread
        .join()
        .map_err(|_| "convert stderr relay thread が panic しました".to_string())??;

    if status.success() {
        Ok(())
    } else {
        Err(match status.code() {
            Some(code) => format!("convert が終了コード {} で失敗しました", code),
            None => "convert が異常終了しました".to_string(),
        })
    }
}

fn relay_web_convert_stream<R: io::Read>(reader: R) -> Result<(), String> {
    relay_web_stream_to_console(reader, "stdout2")
}

fn process_hotentry(
    hotentries: &HashMap<i64, Vec<SubtitleInfo>>,
) -> Result<(), String> {
    if hotentries.is_empty() {
        return Ok(());
    }

    let mut collected: Vec<(i64, PathBuf, TocObject, Vec<SubtitleInfo>)> = Vec::new();
    for (id, subtitles) in hotentries {
        if subtitles.is_empty() {
            continue;
        }

        let (novel_dir, title, author) = narou_rs::db::with_database(|db| {
            let record = db
                .get(*id)
                .cloned()
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
            let novel_dir = narou_rs::db::existing_novel_dir_for_record(db.archive_root(), &record);
            Ok::<_, narou_rs::error::NarouError>((novel_dir, record.title, record.author))
        })
        .map_err(|e| e.to_string())?;

        let toc = load_toc_object(&novel_dir).map_err(|e| e.to_string())?;
        let _ = (title, author);
        collected.push((*id, novel_dir, toc, subtitles.clone()));
    }

    if collected.is_empty() {
        return Ok(());
    }

    println!("{}", "\u{2015}".repeat(35));
    println!("hotentry の変換を開始");

    let mut converted_entries = Vec::new();
    for (id, novel_dir, toc, subtitles) in &collected {
        let settings = NovelSettings::load_for_novel(*id, &toc.title, &toc.author, novel_dir);
        let mut settings = settings;
        settings.enable_illust = false;
        let display_title = settings.novel_title.clone();
        let display_author = settings.novel_author.clone();

        let mut converter = if let Some(uc) = UserConverter::load_with_title(novel_dir, &toc.title)
        {
            NovelConverter::with_user_converter(settings, uc)
        } else {
            NovelConverter::new(settings)
        };

        let text = converter
            .convert_subtitles_for_hotentry(toc, subtitles, novel_dir)
            .map_err(|e| e.to_string())?;
        converted_entries.push(HotentryEntry {
            title: display_title,
            author: display_author,
            text,
        });
    }

    let hotentry_dir = hotentry_dir_path();
    std::fs::create_dir_all(&hotentry_dir).map_err(|e| e.to_string())?;

    let now = chrono::Local::now();
    let hotentry_title = now.format("hotentry %y/%m/%d %H:%M").to_string();
    let hotentry_text = render_hotentry_text(&hotentry_title, &converted_entries, &now);
    let txt_path = hotentry_dir.join(now.format("hotentry_%y-%m-%d_%H%M.txt").to_string());
    std::fs::write(&txt_path, &hotentry_text).map_err(|e| e.to_string())?;

    let device = current_device().unwrap_or(Device::Text);
    let final_path = if device == Device::Text {
        txt_path.clone()
    } else {
        let output_manager = OutputManager::new(device);
        let base_name = txt_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("hotentry");
        output_manager
            .convert_file(&txt_path, &hotentry_dir, base_name, false)
            .map_err(|e| e.to_string())?
    };

    let _ = copy_to_hotentry_output(
        &final_path,
        if device == Device::Text {
            None
        } else {
            Some(device)
        },
    );
    let _ = send_hotentry_output(
        &final_path,
        if device == Device::Text {
            None
        } else {
            Some(device)
        },
    );
    mail_hotentry_if_enabled();

    println!("hotentry を生成しました: {}", final_path.display());
    Ok(())
}

fn mail_hotentry_if_enabled() {
    if !load_local_setting_bool("hotentry.auto-mail") {
        return;
    }

    match load_mail_setting() {
        Ok(setting) => {
            if let Err(e) = send_target_with_setting(&setting, "hotentry", false, false) {
                eprintln!("{}", e);
            }
        }
        Err(MailSettingLoadError::NotFound(_)) => match ensure_mail_setting_file() {
            Ok(path) => {
                println!("created {}", path.display());
                println!(
                    "メールの設定用ファイルを作成しました。設定ファイルを書き換えることで mail コマンドが有効になります。"
                );
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        },
        Err(MailSettingLoadError::Incomplete(_)) => {
            eprintln!(
                "設定ファイルの書き換えが終了していないようです。\n設定ファイルは mail_setting.yaml にあります"
            );
        }
        Err(e) => {
            eprintln!("Error: {}", e);
        }
    }
}

struct HotentryEntry {
    title: String,
    author: String,
    text: String,
}

fn load_toc_object(novel_dir: &Path) -> narou_rs::error::Result<TocObject> {
    let toc_path = novel_dir.join("toc.yaml");
    let toc_content = std::fs::read_to_string(&toc_path)?;
    let toc: TocFile = serde_yaml::from_str(&toc_content)?;
    Ok(TocObject {
        title: toc.title,
        author: toc.author,
        toc_url: toc.toc_url,
        story: toc.story,
        subtitles: toc.subtitles,
        novel_type: toc.novel_type,
    })
}

fn hotentry_dir_path() -> PathBuf {
    PathBuf::from(HOTENTRY_DIR_NAME)
}

fn render_hotentry_text(
    hotentry_title: &str,
    entries: &[HotentryEntry],
    now: &chrono::DateTime<chrono::Local>,
) -> String {
    let mut output = String::new();
    output.push_str(hotentry_title);
    output.push('\n');
    output.push_str("Narou.rb\n\n");
    output.push_str("［＃改ページ］\n");
    output.push_str(&format!(
        "このデータは{}頃作成されました。\n\n",
        now.format("%y年%m月%d日\u{3000}%H時%M分")
    ));
    output.push_str("■収録作品一覧\n");
    for entry in entries {
        output.push_str("［＃１字下げ］");
        output.push_str(&entry.title);
        output.push('\n');
    }
    output.push('\n');

    for entry in entries {
        output.push_str("［＃改ページ］\n");
        output.push_str("［＃ページの左右中央］\n");
        output.push_str("［＃１字下げ］［＃大見出し］");
        output.push_str(&entry.title);
        output.push_str("［＃大見出し終わり］\n");
        output.push_str("［＃ここから地付き］");
        output.push_str(&entry.author);
        output.push_str("［＃ここで地付き終わり］\n");
        output.push_str(entry.text.trim_end_matches('\n'));
        output.push('\n');
    }

    output.push('\n');
    output.push_str(HOTENTRY_FOOTER);
    output.push('\n');
    output
}

fn load_setting_float(key: &str, default: f64) -> f64 {
    load_local_setting_value(key)
        .and_then(|v| match v {
            serde_yaml::Value::Number(n) => n.as_f64(),
            serde_yaml::Value::String(s) => s.parse::<f64>().ok(),
            _ => None,
        })
        .map(|v| if v >= default { v } else { default })
        .unwrap_or(default)
}

fn copy_to_hotentry_output(
    src_path: &Path,
    _device: Option<Device>,
) -> Result<Option<PathBuf>, String> {
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
    let mut dst_dir = base;
    if let Some(device) = _device {
        if narou_rs::compat::load_local_setting_list("convert.copy-to-grouping")
            .iter()
            .any(|value| value.eq_ignore_ascii_case("device"))
        {
            dst_dir.push(device.display_name());
            std::fs::create_dir_all(&dst_dir).map_err(|e| e.to_string())?;
        }
    }
    let dst = dst_dir.join(
        src_path
            .file_name()
            .ok_or_else(|| "Invalid converted filename".to_string())?,
    );
    std::fs::copy(src_path, &dst).map_err(|e| e.to_string())?;
    println!("{} へコピーしました", dst.display());
    Ok(Some(dst))
}

fn send_hotentry_output(
    ebook_file: &Path,
    device: Option<Device>,
) -> Result<(), String> {
    let Some(device) = device else {
        return Ok(());
    };
    let manager = OutputManager::new(device);
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

fn update_general_lastup(gl_opt: Option<&str>, user_agent: Option<&str>) {
    if gl_opt.is_some() && !matches!(gl_opt, Some("narou") | Some("other")) {
        eprintln!("--gl で指定可能なオプションではありません。詳細は narou u -h を参照");
        std::process::exit(127);
    }

    println!("最新話掲載日を確認しています...");

    let site_settings = SiteSetting::load_all().unwrap_or_default();

    let (narou_novels, other_novels) = partition_novels_by_api_support(&site_settings);

    if gl_opt.is_none() || gl_opt == Some("narou") {
        update_general_lastup_narou(&narou_novels, user_agent);
    }

    if gl_opt.is_none() || gl_opt == Some("other") {
        if gl_opt.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
        update_general_lastup_other(&other_novels, user_agent);
    }

    let _ = narou_rs::db::with_database_mut(|db| db.save());

    println!("確認が完了しました");
}

fn partition_novels_by_api_support(
    site_settings: &[SiteSetting],
) -> (HashMap<String, Vec<(i64, String)>>, Vec<i64>) {
    let frozen_ids = load_frozen_ids();
    let records: Vec<(i64, String, Option<String>)> = narou_rs::db::with_database(|db| {
        Ok(db
            .all_records()
            .values()
            .filter(|r| !frozen_ids.contains(&r.id) && !r.tags.contains(&"frozen".to_string()))
            .map(|r| {
                let setting = site_settings.iter().find(|s| s.matches_url(&r.toc_url));
                let ncode = r
                    .ncode
                    .clone()
                    .or_else(|| {
                        setting
                            .and_then(|s| s.extract_url_captures(&r.toc_url))
                            .and_then(|captures| captures.get("ncode").cloned())
                    })
                    .unwrap_or_default();
                let api_url = setting.and_then(|s| s.narou_api_url.clone());
                (r.id, ncode, api_url)
            })
            .collect())
    })
    .unwrap_or_default();

    let mut narou: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    let mut other: Vec<i64> = Vec::new();

    for (id, ncode, api_url) in records {
        match (api_url, ncode.is_empty()) {
            (Some(api_url), false) => narou.entry(api_url).or_default().push((id, ncode)),
            _ => other.push(id),
        }
    }

    (narou, other)
}

fn update_general_lastup_narou(
    novels_by_api: &HashMap<String, Vec<(i64, String)>>,
    user_agent: Option<&str>,
) {
    if novels_by_api.is_empty() {
        return;
    }

    let ua = user_agent
        .map(|s| s.to_string())
        .unwrap_or_else(|| ua_generator::ua::spoof_firefox_ua().to_string());
    let fetcher = match narou_rs::downloader::fetch::HttpFetcher::new(&ua) {
        Ok(f) => f,
        Err(_) => return,
    };

    for (api_url, novels) in novels_by_api {
        for chunk in novels.chunks(50) {
            let ncodes: Vec<&str> = chunk.iter().map(|(_, nc)| nc.as_str()).collect();
            let ncode_param = ncodes.join("-");

            fetcher.rate_limiter.wait();
            let url = format!(
                "{}?of=n-nu-gl-l&out=json&ncode={}",
                api_url, ncode_param
            );

            let response = match fetcher.client.get(&url).send() {
                Ok(r) => r,
                Err(_) => continue,
            };

            if !response.status().is_success() {
                continue;
            }

            let body = match response.text() {
                Ok(b) => b,
                Err(_) => continue,
            };

            let entries = parse_narou_api_entries(&body);

            for entry in &entries {
                if let Some((id, _)) = chunk
                    .iter()
                    .find(|(_, nc)| nc.eq_ignore_ascii_case(&entry.ncode))
                {
                    let _ = narou_rs::db::with_database_mut(|db| {
                        if let Some(record) = db.get(*id).cloned() {
                            let mut r = record;
                            apply_general_lastup_check_result(
                                &mut r,
                                parse_api_datetime(&entry.novelupdated_at),
                                parse_api_datetime(&entry.general_lastup),
                                Some(entry.length),
                                None,
                                Utc::now(),
                            );

                            db.insert(r);
                        }
                        Ok(())
                    });
                }
            }
        }
    }
}

fn update_general_lastup_other(novels: &[i64], user_agent: Option<&str>) {
    if novels.is_empty() {
        return;
    }

    let mut downloader = match Downloader::with_user_agent(user_agent) {
        Ok(d) => d,
        Err(_) => return,
    };

    for &id in novels {
        std::thread::sleep(std::time::Duration::from_secs_f64(INTERVAL_MIN_SECS));

        let Ok((novelupdated_at, general_lastup, length, is_end)) =
            downloader.fetch_latest_status_by_id(id)
        else {
            continue;
        };

        let _ = narou_rs::db::with_database_mut(|db| {
            if let Some(record) = db.get(id).cloned() {
                let mut r = record;
                apply_general_lastup_check_result(
                    &mut r,
                    novelupdated_at,
                    general_lastup,
                    length,
                    is_end,
                    Utc::now(),
                );
                db.insert(r);
            }
            Ok::<(), narou_rs::error::NarouError>(())
        });

        sync_end_tag(id);
    }
}

fn parse_narou_api_entries(body: &str) -> Vec<narou_rs::downloader::NarouApiEntry> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };

    match value {
        serde_json::Value::Array(values) => values
            .into_iter()
            .filter(|v| v.get("ncode").is_some())
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect(),
        serde_json::Value::Object(mut map) => {
            if let Some(data) = map.remove("data") {
                serde_json::from_value(data).unwrap_or_default()
            } else if map.get("ncode").is_some() {
                serde_json::from_value(serde_json::Value::Object(map))
                    .ok()
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn parse_api_datetime(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if let Ok(ts) = value.parse::<i64>() {
        return DateTime::from_timestamp(ts, 0);
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Utc));
    }

    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d",
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d %H:%M",
        "%Y/%m/%d",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(value, fmt) {
            return Some(dt.and_utc());
        }
        if let Ok(date) = NaiveDate::parse_from_str(value, fmt) {
            return date.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{
        MODIFIED_TAG, abort_if_interrupted, apply_general_lastup_check_result,
        sleep_with_interrupt,
    };
    use chrono::{Duration, TimeZone, Utc};
    use narou_rs::db::NovelRecord;
    use narou_rs::compat::reroute_web_line_to_console;
    use narou_rs::progress::WS_LINE_PREFIX;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn abort_if_interrupted_returns_err_when_flag_is_set() {
        let interrupted = AtomicBool::new(true);
        assert!(abort_if_interrupted(&interrupted).is_err());
    }

    #[test]
    fn sleep_with_interrupt_returns_immediately_for_zero_duration() {
        let interrupted = AtomicBool::new(false);
        sleep_with_interrupt(0.0, &interrupted).unwrap();
    }

    #[test]
    fn reroute_web_convert_line_wraps_plain_text_for_stdout2() {
        let routed = reroute_web_line_to_console("Converted: test.epub", "stdout2");
        assert!(routed.starts_with(WS_LINE_PREFIX));
        let json = routed.trim_start_matches(WS_LINE_PREFIX);
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["type"], "echo");
        assert_eq!(value["body"], "Converted: test.epub");
        assert_eq!(value["target_console"], "stdout2");
    }

    #[test]
    fn reroute_web_convert_line_retargets_structured_events_to_stdout2() {
        let source = format!(
            "{}{}",
            WS_LINE_PREFIX,
            serde_json::json!({
                "type": "progressbar.step",
                "data": { "current": 3, "total": 9, "percent": 33.3, "topic": "convert" }
            })
        );
        let routed = reroute_web_line_to_console(&source, "stdout2");
        let json = routed.trim_start_matches(WS_LINE_PREFIX);
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["type"], "progressbar.step");
        assert_eq!(value["target_console"], "stdout2");
        assert_eq!(value["data"]["current"], 3);
    }

    fn sample_record(last_update: chrono::DateTime<Utc>) -> NovelRecord {
        NovelRecord {
            id: 1,
            author: "author".to_string(),
            title: "title".to_string(),
            file_title: "title".to_string(),
            toc_url: "https://example.com".to_string(),
            sitename: "example".to_string(),
            novel_type: 1,
            end: false,
            last_update,
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
        }
    }

    #[test]
    fn general_lastup_check_uses_last_update_fallback_and_preserves_it() {
        let last_update = Utc.with_ymd_and_hms(2026, 4, 17, 12, 0, 0).unwrap();
        let mut record = sample_record(last_update);
        let novelupdated_at = Some(last_update + Duration::minutes(5));
        let general_lastup = Some(last_update + Duration::minutes(10));
        let now = last_update + Duration::minutes(20);

        apply_general_lastup_check_result(
            &mut record,
            novelupdated_at,
            general_lastup,
            Some(12345),
            Some(true),
            now,
        );

        assert_eq!(record.last_update, last_update);
        assert_eq!(record.novelupdated_at, novelupdated_at);
        assert_eq!(record.general_lastup, general_lastup);
        assert_eq!(record.length, Some(12345));
        assert!(record.end);
        assert_eq!(record.last_check_date, Some(now));
        assert!(record.tags.iter().any(|tag| tag == MODIFIED_TAG));
    }

    #[test]
    fn general_lastup_check_respects_existing_last_check_date() {
        let last_update = Utc.with_ymd_and_hms(2026, 4, 17, 12, 0, 0).unwrap();
        let mut record = sample_record(last_update);
        record.last_check_date = Some(last_update + Duration::hours(1));
        let stale_after_update = Some(last_update + Duration::minutes(30));
        let now = last_update + Duration::hours(2);

        apply_general_lastup_check_result(
            &mut record,
            stale_after_update,
            None,
            None,
            None,
            now,
        );

        assert_eq!(record.last_update, last_update);
        assert_eq!(record.novelupdated_at, stale_after_update);
        assert_eq!(record.last_check_date, Some(now));
        assert!(!record.tags.iter().any(|tag| tag == MODIFIED_TAG));
    }
}
