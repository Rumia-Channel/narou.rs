use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use indicatif::MultiProgress;

use narou_rs::converter::NovelConverter;
use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::db::inventory::InventoryScope;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::{DownloadResult, Downloader, TargetType, UpdateStatus};
use narou_rs::progress::CliProgress;

const MODIFIED_TAG: &str = "modified";
const INTERVAL_MIN_SECS: f64 = 2.5;
const FORCE_WAIT_SECS: f64 = 2.0;

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
    let result = std::thread::spawn(move || {
        if let Err(e) = narou_rs::db::init_database() {
            eprintln!("Error initializing database: {}", e);
            std::process::exit(1);
        }

        repair_empty_titles();

        if let Some(ref gl_opt) = opts.gl {
            update_general_lastup(gl_opt.as_deref(), opts.user_agent.as_deref());
            return;
        }

        let setting_sort_by = load_setting_string("update.sort-by");
        let sort_by = resolve_sort_key(opts.sort_by.as_deref().or(setting_sort_by.as_deref()));

        let convert_only_new_arrival =
            opts.convert_only_new_arrival || load_setting_bool("update.convert-only-new-arrival");
        let interval_secs = load_setting_float("update.interval", INTERVAL_MIN_SECS);

        let (target_ids, unresolved_count) =
            resolve_targets(opts.ids.as_deref(), opts.ignore_all, sort_by.as_deref());
        if target_ids.is_empty() {
            if unresolved_count > 0 {
                std::process::exit(unresolved_count.min(127) as i32);
            }
            return;
        }

        let is_bulk = opts.ids.is_none();
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

        let mut last_time = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs_f64(interval_secs))
            .unwrap_or_else(std::time::Instant::now);

        for (i, &id) in target_ids.iter().enumerate() {
            if i > 0 {
                let _ = multi_clone.println("\u{2500}".repeat(35));
            }

            let frozen = !opts.force && is_novel_frozen(id);
            if frozen {
                if is_bulk {
                    continue;
                }
                let title = get_novel_title(id);
                let _ = multi_clone.println(format!("ID:{}　{} は凍結中です", id, title));
                mistook += 1;
                continue;
            }

            let elapsed = last_time.elapsed().as_secs_f64();
            if elapsed < interval_secs {
                std::thread::sleep(std::time::Duration::from_secs_f64(interval_secs - elapsed));
            }
            last_time = std::time::Instant::now();

            let progress = CliProgress::with_multi(&format!("DL {}", id), multi_clone.clone());
            downloader.set_progress(Box::new(progress));

            match downloader.download_novel(&id.to_string()) {
                Ok(dl) => {
                    remove_modified_tag(dl.id);
                    update_last_check_date(dl.id);
                    sync_end_tag(dl.id);

                    print_status_messages(&multi_clone, &dl);

                    let new_arrivals = dl.updated_count > 0 || dl.new_novel;
                    let has_convert_failure = narou_rs::db::with_database(|db| {
                        Ok(db.get(dl.id).map(|r| r.convert_failure).unwrap_or(false))
                    })
                    .unwrap_or(false);

                    match dl.status {
                        UpdateStatus::Ok => {
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
                            if !has_convert_failure {
                                continue;
                            }
                        }
                        UpdateStatus::Failed => {
                            mistook += 1;
                            continue;
                        }
                    }

                    if has_convert_failure {
                        let _ = multi_clone.println("前回変換できなかったので再変換します");
                    }

                    match auto_convert(&multi_clone, &dl) {
                        Ok(()) => {
                            clear_convert_failure(dl.id);
                        }
                        Err(e) => {
                            let _ = multi_clone.println(format!("  Convert error: {}", e));
                            set_convert_failure(dl.id);
                            mistook += 1;
                        }
                    }
                }
                Err(e) => {
                    let title = get_novel_title(id);
                    let _ = multi_clone
                        .println(format!("ID:{} {} の更新は失敗しました\n  {}", id, title, e));
                    mistook += 1;
                }
            }
        }

        let _ = narou_rs::db::with_database_mut(|db| db.save());

        if mistook > 0 {
            let _ = multi_clone.println(format!("\n{} 件のエラーが発生しました", mistook));
        }
        drop(multi);

        if mistook > 0 {
            std::process::exit(mistook.min(127) as i32);
        }
    })
    .join();

    if let Err(e) = result {
        if let Some(s) = e.downcast_ref::<String>() {
            eprintln!("アップデートを中断しました: {}", s);
        } else {
            eprintln!("アップデートを中断しました");
        }
        std::process::exit(126);
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
                eprintln!("[ERROR] {} は管理小説の中に存在しません", target);
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

fn print_status_messages(multi: &Arc<MultiProgress>, dl: &DownloadResult) {
    match dl.status {
        UpdateStatus::Ok => {
            if dl.new_novel {
                let _ = multi.println(format!(
                    "{} のDL完了 (ID:{}, {}セクション)",
                    dl.title, dl.id, dl.total_count
                ));
            } else if dl.sections_deleted {
                let _ = multi.println(format!(
                    "ID:{} {} は一部の話が削除されています",
                    dl.id, dl.title
                ));
            } else if dl.updated_count > 0 {
                let _ = multi.println(format!("{} の更新が完了しました", dl.title));
            } else if dl.title_changed {
                let _ = multi.println(format!(
                    "ID:{} {} のタイトルが更新されています",
                    dl.id, dl.title
                ));
            } else if dl.story_changed {
                let _ = multi.println(format!(
                    "ID:{} {} のあらすじが更新されています",
                    dl.id, dl.title
                ));
            } else if dl.author_changed {
                let _ = multi.println(format!(
                    "ID:{} {} の作者名が更新されています",
                    dl.id, dl.title
                ));
            }
        }
        UpdateStatus::None => {
            let _ = multi.println(format!("{} に更新はありません", dl.title));
        }
        UpdateStatus::Failed => {}
    }
}

fn auto_convert(multi: &Arc<MultiProgress>, dl: &DownloadResult) -> Result<(), String> {
    let settings = NovelSettings::load_for_novel(dl.id, &dl.title, &dl.author, &dl.novel_dir);
    let mut converter = if let Some(uc) = UserConverter::load_with_title(&dl.novel_dir, &dl.title) {
        NovelConverter::with_user_converter(settings, uc)
    } else {
        NovelConverter::new(settings)
    };

    let progress = CliProgress::with_multi(&format!("Convert {}", dl.title), multi.clone());
    converter.set_progress(Box::new(progress));

    match converter.convert_novel_by_id(dl.id, &dl.novel_dir) {
        Ok(path) => {
            let _ = multi.println(format!("  Converted: {}", path));
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn load_setting_value(key: &str) -> Option<serde_yaml::Value> {
    narou_rs::db::with_database(|db| {
        let settings: HashMap<String, serde_yaml::Value> = db
            .inventory()
            .load("local_setting", InventoryScope::Local)?;
        Ok(settings.get(key).cloned())
    })
    .ok()
    .flatten()
}

fn load_setting_string(key: &str) -> Option<String> {
    load_setting_value(key).and_then(|v| yaml_value_to_string(&v))
}

fn load_setting_bool(key: &str) -> bool {
    load_setting_value(key)
        .and_then(|v| match v {
            serde_yaml::Value::Bool(b) => Some(b),
            serde_yaml::Value::String(s) => Some(matches!(s.as_str(), "true" | "yes" | "on" | "1")),
            serde_yaml::Value::Number(n) => Some(n.as_i64().unwrap_or(0) != 0),
            _ => None,
        })
        .unwrap_or(false)
}

fn load_setting_float(key: &str, default: f64) -> f64 {
    load_setting_value(key)
        .and_then(|v| match v {
            serde_yaml::Value::Number(n) => n.as_f64(),
            serde_yaml::Value::String(s) => s.parse::<f64>().ok(),
            _ => None,
        })
        .map(|v| if v >= default { v } else { default })
        .unwrap_or(default)
}

fn yaml_value_to_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
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
                "{}?of=nu-gl-l-ncode&out=json&ncode={}",
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

                            let novelupdated_at = parse_api_datetime(&entry.novelupdated_at);
                            let general_lastup = parse_api_datetime(&entry.general_lastup);

                            let last_check = r.last_check_date.or(Some(r.last_update));

                            if let Some(nu) = novelupdated_at {
                                if let Some(lc) = last_check
                                    && nu > lc
                                    && !r.tags.contains(&MODIFIED_TAG.to_string())
                                {
                                    r.tags.push(MODIFIED_TAG.to_string());
                                }
                                r.novelupdated_at = Some(nu);
                            }

                            if let Some(gl) = general_lastup {
                                r.general_lastup = Some(gl);
                            }

                            r.length = Some(entry.length);
                            r.last_check_date = Some(Utc::now());

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
                let last_check = r.last_check_date.or(Some(r.last_update));
                if let Some(nu) = novelupdated_at {
                    if let Some(lc) = last_check
                        && nu > lc
                        && !r.tags.contains(&MODIFIED_TAG.to_string())
                    {
                        r.tags.push(MODIFIED_TAG.to_string());
                    }
                    r.novelupdated_at = Some(nu);
                }
                if let Some(gl) = general_lastup {
                    r.general_lastup = Some(gl);
                }
                if let Some(len) = length {
                    r.length = Some(len);
                }
                if let Some(end) = is_end {
                    r.end = end;
                }
                r.last_check_date = Some(Utc::now());
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
