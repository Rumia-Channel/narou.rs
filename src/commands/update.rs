use std::sync::Arc;

use indicatif::MultiProgress;

use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::converter::NovelConverter;
use narou_rs::downloader::{DownloadResult, Downloader, UpdateStatus};
use narou_rs::progress::{CliProgress, ProgressReporter};

pub struct UpdateOptions {
    pub ids: Option<Vec<String>>,
    pub all: bool,
    pub force: bool,
    pub no_convert: bool,
    pub sort_by: Option<String>,
    pub user_agent: Option<String>,
}

pub fn cmd_update(opts: UpdateOptions) {
    let result = std::thread::spawn(move || {
        if let Err(e) = narou_rs::db::init_database() {
            eprintln!("Error initializing database: {}", e);
            std::process::exit(1);
        }

        let mut downloader = match Downloader::with_user_agent(opts.user_agent.as_deref()) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Error creating downloader: {}", e);
                std::process::exit(1);
            }
        };

        let target_ids = resolve_targets(&opts);
        if target_ids.is_empty() {
            return;
        }

        let total = target_ids.len();
        let mut mistook = 0usize;

        let multi = CliProgress::multi();
        let multi_clone = multi.clone();

        for (i, &id) in target_ids.iter().enumerate() {
            if i > 0 {
                let _ = multi_clone.println(format!("{}", "\u{2500}".repeat(35)));
            }

            let is_bulk = opts.ids.is_none() || opts.all;

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

            let progress = CliProgress::with_multi(&format!("DL {}", id), multi_clone.clone());
            downloader.set_progress(Box::new(progress));

            match downloader.download_novel(&id.to_string()) {
                Ok(dl) => {
                    print_status_messages(&multi_clone, &dl);

                    let should_convert = !opts.no_convert && (dl.status == UpdateStatus::Ok);

                    if should_convert {
                        if let Err(e) = auto_convert(&multi_clone, &dl) {
                            let _ = multi_clone.println(format!("  Convert error: {}", e));
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

            if i + 1 < total {
                std::thread::sleep(std::time::Duration::from_millis(2500));
            }
        }

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
        eprintln!("Thread panicked: {:?}", e);
    }
}

fn resolve_targets(opts: &UpdateOptions) -> Vec<i64> {
    if opts.all {
        return narou_rs::db::with_database(|db| {
            let mut ids = db.ids();
            if let Some(ref key) = opts.sort_by {
                let reverse = is_time_sort_key(key);
                let records = db.sort_by(key, reverse);
                ids = records.iter().map(|r| r.id).collect();
            }
            Ok(ids)
        })
        .unwrap_or_default();
    }

    if let Some(ref targets) = opts.ids {
        let mut resolved = Vec::new();
        for target in targets {
            if let Some(id) = resolve_target_to_id(target) {
                if !resolved.contains(&id) {
                    resolved.push(id);
                }
            } else if target.starts_with("tag:") {
                let tag_name = &target[4..];
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
            } else if target.starts_with("^tag:") {
                let tag_name = &target[5..];
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

        if let Some(ref key) = opts.sort_by {
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

    narou_rs::db::with_database(|db| Ok(db.ids())).unwrap_or_default()
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
        let record = db.get(id);
        Ok(record
            .map(|r| r.tags.contains(&"frozen".to_string()))
            .unwrap_or(false))
    })
    .unwrap_or(false)
}

fn get_novel_title(id: i64) -> String {
    narou_rs::db::with_database(|db| Ok(db.get(id).map(|r| r.title.clone()).unwrap_or_default()))
        .unwrap_or_default()
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
                let _ = multi.println(format!(
                    "{} の更新完了 (ID:{}, {}/{}話更新)",
                    dl.title, dl.id, dl.updated_count, dl.total_count
                ));
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
