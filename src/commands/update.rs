use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use indicatif::MultiProgress;

use narou_rs::converter::NovelConverter;
use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::{DownloadResult, Downloader, UpdateStatus};
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
    pub all: bool,
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

        let setting_sort_by = load_setting("update.sort-by");
        let sort_by = resolve_sort_key(opts.sort_by.as_deref().or(setting_sort_by.as_deref()));

        let convert_only_new_arrival =
            opts.convert_only_new_arrival || load_setting_bool("update.convert-only-new-arrival");
        let interval_secs = load_setting_float("update.interval", INTERVAL_MIN_SECS);

        let target_ids = resolve_targets(
            opts.ids.as_deref(),
            opts.all,
            opts.ignore_all,
            sort_by.as_deref(),
        );
        if target_ids.is_empty() {
            return;
        }

        let is_bulk = opts.ids.is_none() || opts.all;
        let _total = target_ids.len();
        let mut mistook = 0usize;

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

                    print_status_messages(&multi_clone, &dl);

                    let new_arrivals = dl.updated_count > 0 || dl.new_novel;

                    if opts.no_convert {
                        std::thread::sleep(std::time::Duration::from_secs_f64(FORCE_WAIT_SECS));
                        continue;
                    }

                    if convert_only_new_arrival && !new_arrivals {
                        std::thread::sleep(std::time::Duration::from_secs_f64(FORCE_WAIT_SECS));
                        continue;
                    }

                    let should_convert = dl.status == UpdateStatus::Ok;
                    let has_convert_failure = narou_rs::db::with_database(|db| {
                        Ok(db.get(dl.id).map(|r| r.convert_failure).unwrap_or(false))
                    })
                    .unwrap_or(false);

                    if !should_convert && !has_convert_failure {
                        continue;
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
    all: bool,
    ignore_all: bool,
    sort_by: Option<&str>,
) -> Vec<i64> {
    if all {
        return narou_rs::db::with_database(|db| {
            let ids = db.ids();
            Ok(if let Some(key) = sort_by {
                let reverse = is_time_sort_key(key);
                let records = db.sort_by(key, reverse);
                records.iter().map(|r| r.id).collect()
            } else {
                ids
            })
        })
        .unwrap_or_default();
    }

    if let Some(targets) = ids {
        let mut resolved = Vec::new();
        for target in targets {
            if let Some(id) = resolve_target_to_id(target) {
                if !resolved.contains(&id) {
                    resolved.push(id);
                }
            } else if let Some(tag_name) = target.strip_prefix("tag:") {
                let tag_ids = narou_rs::db::with_database(|db| {
                    let index = db.tag_index();
                    Ok(index.get(tag_name).cloned().unwrap_or_default())
                })
                .unwrap_or_default();
                for id in tag_ids {
                    if !resolved.contains(&id) {
                        resolved.push(id);
                    }
                }
            } else if let Some(tag_name) = target.strip_prefix("^tag:") {
                let exclude_ids = narou_rs::db::with_database(|db| {
                    let index = db.tag_index();
                    Ok(index.get(tag_name).cloned().unwrap_or_default())
                })
                .unwrap_or_default();
                let all_ids = narou_rs::db::with_database(|db| Ok(db.ids())).unwrap_or_default();
                for id in all_ids {
                    if !exclude_ids.contains(&id) && !resolved.contains(&id) {
                        resolved.push(id);
                    }
                }
            } else {
                eprintln!("[ERROR] {} は管理小説の中に存在しません", target);
            }
        }

        if let Some(key) = sort_by {
            let reverse = is_time_sort_key(key);
            let all_records = narou_rs::db::with_database(|db| {
                Ok(db
                    .sort_by(key, reverse)
                    .iter()
                    .map(|r| r.id)
                    .collect::<Vec<_>>())
            })
            .unwrap_or_default();
            resolved.sort_by_key(|id| {
                all_records
                    .iter()
                    .position(|&rid| rid == *id)
                    .unwrap_or(usize::MAX)
            });
        }

        return resolved;
    }

    if ignore_all {
        return Vec::new();
    }

    narou_rs::db::with_database(|db| {
        let ids = db.ids();
        Ok(if let Some(key) = sort_by {
            let reverse = is_time_sort_key(key);
            let records = db.sort_by(key, reverse);
            records.iter().map(|r| r.id).collect()
        } else {
            ids
        })
    })
    .unwrap_or_default()
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

fn resolve_target_to_id(target: &str) -> Option<i64> {
    if let Ok(i) = target.parse::<i64>() {
        let exists = narou_rs::db::with_database(|db| Ok(db.get(i).is_some())).unwrap_or(false);
        if exists {
            return Some(i);
        }
    }
    narou_rs::db::with_database(|db| Ok(db.find_by_title(target).map(|r| r.id)))
        .ok()
        .flatten()
}

fn is_time_sort_key(key: &str) -> bool {
    matches!(key, "last_update" | "general_lastup" | "new_arrivals_date")
}

fn is_novel_frozen(id: i64) -> bool {
    narou_rs::db::with_database(|db| {
        Ok(db
            .get(id)
            .map(|r| r.tags.contains(&"frozen".to_string()))
            .unwrap_or(false))
    })
    .unwrap_or(false)
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
                    if let Ok(toc) = serde_yaml::from_str::<narou_rs::downloader::TocFile>(&toc_content) {
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

fn load_setting(key: &str) -> Option<String> {
    narou_rs::db::with_database(|db| {
        let settings: HashMap<String, serde_yaml::Value> = db.inventory().load(
            "local_setting",
            narou_rs::db::inventory::InventoryScope::Local,
        )?;
        Ok(settings
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()))
    })
    .ok()
    .flatten()
}

fn load_setting_bool(key: &str) -> bool {
    load_setting(key)
        .as_deref()
        .map(|v| v == "true" || v == "yes")
        .unwrap_or(false)
}

fn load_setting_float(key: &str, default: f64) -> f64 {
    load_setting(key)
        .as_deref()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| if v >= default { v } else { default })
        .unwrap_or(default)
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
) -> (Vec<(i64, String, String)>, Vec<i64>) {
    let records: Vec<(i64, String, String, bool)> = narou_rs::db::with_database(|db| {
        Ok(db
            .all_records()
            .values()
            .filter(|r| !r.tags.contains(&"frozen".to_string()))
            .map(|r| {
                let has_api = site_settings
                    .iter()
                    .find(|s| s.matches_url(&r.toc_url))
                    .map(|s| s.narou_api_url.is_some())
                    .unwrap_or(false);
                (
                    r.id,
                    r.ncode.clone().unwrap_or_default(),
                    r.toc_url.clone(),
                    has_api,
                )
            })
            .collect())
    })
    .unwrap_or_default();

    let mut narou: Vec<(i64, String, String)> = Vec::new();
    let mut other: Vec<i64> = Vec::new();

    for (id, ncode, _toc_url, has_api) in records {
        if has_api && !ncode.is_empty() {
            narou.push((id, ncode, _toc_url));
        } else {
            other.push(id);
        }
    }

    (narou, other)
}

fn update_general_lastup_narou(novels: &[(i64, String, String)], user_agent: Option<&str>) {
    if novels.is_empty() {
        return;
    }

    let ua = user_agent
        .map(|s| s.to_string())
        .unwrap_or_else(|| ua_generator::ua::spoof_firefox_ua().to_string());
    let fetcher = match narou_rs::downloader::fetch::HttpFetcher::new(&ua) {
        Ok(f) => f,
        Err(_) => return,
    };

    let api_url = "https://api.syosetu.com/novelapi/api/";

    for chunk in novels.chunks(50) {
        let ncodes: Vec<&str> = chunk.iter().map(|(_, nc, _)| nc.as_str()).collect();
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

        let api_result: narou_rs::downloader::NarouApiResult = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(_) => continue,
        };

        for entry in &api_result.data {
            if let Some((id, _, _)) = chunk.iter().find(|(_, nc, _)| nc == &entry.ncode) {
                let _ = narou_rs::db::with_database_mut(|db| {
                    if let Some(record) = db.get(*id).cloned() {
                        let mut r = record;

                        let novelupdated_at =
                            chrono::DateTime::parse_from_rfc3339(&entry.novelupdated_at)
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc));

                        let general_lastup =
                            chrono::DateTime::parse_from_rfc3339(&entry.general_lastup)
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc));

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

fn update_general_lastup_other(novels: &[i64], user_agent: Option<&str>) {
    if novels.is_empty() {
        return;
    }

    let mut downloader = match Downloader::with_user_agent(user_agent) {
        Ok(d) => d,
        Err(_) => return,
    };

    for &id in novels {
        let _ = narou_rs::db::with_database_mut(|db| {
            if let Some(record) = db.get(id).cloned() {
                let mut r = record;
                r.last_check_date = Some(Utc::now());
                db.insert(r);
            }
            Ok::<(), narou_rs::error::NarouError>(())
        });

        std::thread::sleep(std::time::Duration::from_secs_f64(INTERVAL_MIN_SECS));

        if let Ok(dl) = downloader.download_novel(&id.to_string()) {
            remove_modified_tag(dl.id);
        }
    }
}
