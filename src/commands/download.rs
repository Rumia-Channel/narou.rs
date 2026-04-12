use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;

use indicatif::MultiProgress;

use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::converter::NovelConverter;
use narou_rs::downloader::{Downloader, TargetType, UpdateStatus};
use narou_rs::progress::CliProgress;

pub struct DownloadOptions {
    pub targets: Vec<String>,
    pub force: bool,
    pub no_convert: bool,
    pub freeze: bool,
    pub remove: bool,
    #[allow(dead_code)]
    pub mail: bool,
    pub user_agent: Option<String>,
}

pub fn cmd_download(opts: DownloadOptions) {
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

        let mut targets = opts.targets.clone();

        if targets.is_empty() {
            targets = interactive_mode(&downloader);
            if targets.is_empty() {
                return;
            }
        }

        targets = tagname_to_ids(&targets);

        let multi = CliProgress::multi();
        let multi_clone = multi.clone();
        let mut mistook = 0usize;

        for (i, target) in targets.iter().enumerate() {
            if i > 0 {
                let _ = multi_clone.println(format!("{}", "\u{2500}".repeat(35)));
            }

            let data = get_data_by_target(target);

            if is_novel_frozen(target) {
                if let Some(ref rec) = data {
                    let _ = multi_clone.println(format!(
                        "{} は凍結中です\nダウンロードを中止しました",
                        rec.title
                    ));
                }
                mistook += 1;
                continue;
            }

            if !opts.force {
                if let Some(ref rec) = data {
                    if has_novel_data_dir(target) {
                        let _ = multi_clone.println(format!(
                            "{} はダウンロード済みです。\nID: {}\ntitle: {}",
                            target, rec.id, rec.title
                        ));
                        mistook += 1;
                        continue;
                    }
                }
            }

            let progress = CliProgress::with_multi(&format!("DL {}", target), multi_clone.clone());
            downloader.set_progress(Box::new(progress));

            match downloader.download_novel_with_force(target, opts.force) {
                Ok(dl) => {
                    print_download_status(&multi_clone, &dl);

                    match dl.status {
                        UpdateStatus::Ok => {}
                        UpdateStatus::None | UpdateStatus::Failed | UpdateStatus::Canceled => {
                            mistook += 1;
                            continue;
                        }
                    }

                    if opts.no_convert {
                        after_process(&multi_clone, target, &opts);
                    } else {
                        if let Err(e) = auto_convert(&multi_clone, &dl) {
                            let _ = multi_clone.println(format!("  Convert error: {}", e));
                        }
                        after_process(&multi_clone, target, &opts);
                    }
                }
                Err(e) => {
                    if matches!(e, narou_rs::error::NarouError::SuspendDownload(_)) {
                        std::panic::resume_unwind(Box::new(e.to_string()));
                    }
                    let _ = multi_clone.println(format!("  Error: {}", e));
                    mistook += 1;
                }
            }
        }

        drop(multi);

        if mistook > 0 {
            std::process::exit(mistook.min(127) as i32);
        }
    })
    .join();

    if let Err(_) = result {
        std::process::exit(127);
    }
}

fn interactive_mode(downloader: &Downloader) -> Vec<String> {
    if !std::io::stdin().is_terminal() {
        return Vec::new();
    }

    println!("【対話モード】");
    println!("ダウンロードしたい小説のNコードもしくはURLを入力して下さい。(1行に1つ)");
    println!("連続して複数の小説を入力していきます。");
    println!(
        "対応サイトは小説家になろう(小説を読もう)、ノクターンノベルズ、ムーンライトノベルズ、Arcadia、ハーメルン、暁、カクヨムです。"
    );
    println!("入力を終了してダウンロードを開始するには未入力のままエンターを押して下さい。");
    println!();

    let mut targets = Vec::new();
    let stdin = io::stdin();
    print_prompt(targets.len());

    loop {
        let mut input = String::new();
        if stdin.lock().read_line(&mut input).unwrap_or(0) == 0 {
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            break;
        }

        if valid_target(downloader, input) {
            if targets.contains(&input.to_string()) {
                eprintln!("入力済みです");
            } else {
                targets.push(input.to_string());
            }
        } else {
            eprintln!("対応外の小説です");
        }
        print_prompt(targets.len());
    }

    targets
}

fn print_prompt(count: usize) {
    print!("{}件をダウンロードしますか？ [Y/n]> ", count);
    let _ = io::stdout().flush();
}

fn valid_target(downloader: &Downloader, target: &str) -> bool {
    let target_type = Downloader::get_target_type(target);
    match target_type {
        TargetType::Ncode => true,
        TargetType::Url => downloader.site_setting_matches_url(target),
        _ => false,
    }
}

fn tagname_to_ids(targets: &[String]) -> Vec<String> {
    let mut expanded = Vec::new();

    for target in targets {
        if target.starts_with("tag:") {
            let tag_name = &target[4..];
            let tag_ids = narou_rs::db::with_database(|db| {
                let index = db.tag_index();
                Ok(index.get(tag_name).cloned().unwrap_or_default())
            })
            .unwrap_or_default();
            if tag_ids.is_empty() {
                expanded.push(target.clone());
            } else {
                for id in tag_ids {
                    let s = id.to_string();
                    if !expanded.contains(&s) {
                        expanded.push(s);
                    }
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
                if !exclude_ids.contains(&id) {
                    let s = id.to_string();
                    if !expanded.contains(&s) {
                        expanded.push(s);
                    }
                }
            }
        } else if let Ok(id) = target.parse::<i64>() {
            let exists =
                narou_rs::db::with_database(|db| Ok(db.get(id).is_some())).unwrap_or(false);
            if exists {
                if !expanded.contains(&target.clone()) {
                    expanded.push(target.clone());
                }
            } else {
                let tag_ids = narou_rs::db::with_database(|db| {
                    let index = db.tag_index();
                    Ok(index.get(target).cloned().unwrap_or_default())
                })
                .unwrap_or_default();
                if tag_ids.is_empty() {
                    expanded.push(target.clone());
                } else {
                    for id in tag_ids {
                        let s = id.to_string();
                        if !expanded.contains(&s) {
                            expanded.push(s);
                        }
                    }
                }
            }
        } else {
            let tag_ids = narou_rs::db::with_database(|db| {
                let index = db.tag_index();
                Ok(index.get(target).cloned().unwrap_or_default())
            })
            .unwrap_or_default();
            if tag_ids.is_empty() {
                expanded.push(target.clone());
            } else {
                for id in tag_ids {
                    let s = id.to_string();
                    if !expanded.contains(&s) {
                        expanded.push(s);
                    }
                }
            }
        }
    }

    expanded
}

struct RecordInfo {
    id: i64,
    title: String,
}

fn get_data_by_target(target: &str) -> Option<RecordInfo> {
    let target_type = Downloader::get_target_type(target);
    match target_type {
        TargetType::Id => {
            if let Ok(id) = target.parse::<i64>() {
                narou_rs::db::with_database(|db| {
                    Ok(db.get(id).map(|r| RecordInfo {
                        id: r.id,
                        title: r.title.clone(),
                    }))
                })
                .ok()
                .flatten()
            } else {
                None
            }
        }
        TargetType::Url => {
            let toc_url = resolve_toc_url_from_url(target)?;
            narou_rs::db::with_database(|db| {
                Ok(db.get_by_toc_url(&toc_url).map(|r| RecordInfo {
                    id: r.id,
                    title: r.title.clone(),
                }))
            })
            .ok()
            .flatten()
        }
        TargetType::Ncode => {
            let ncode = target.to_lowercase();
            narou_rs::db::with_database(|db| {
                for r in db.all_records().values() {
                    if r.ncode.as_deref() == Some(ncode.as_str()) {
                        return Ok(Some(RecordInfo {
                            id: r.id,
                            title: r.title.clone(),
                        }));
                    }
                }
                Ok::<Option<RecordInfo>, narou_rs::error::NarouError>(None)
            })
            .ok()
            .flatten()
        }
        _ => narou_rs::db::with_database(|db| {
            Ok(db.find_by_title(target).map(|r| RecordInfo {
                id: r.id,
                title: r.title.clone(),
            }))
        })
        .ok()
        .flatten(),
    }
}

fn resolve_toc_url_from_url(target: &str) -> Option<String> {
    let settings = narou_rs::downloader::site_setting::SiteSetting::load_all().ok()?;
    for setting in &settings {
        if setting.matches_url(target) {
            return Some(
                setting
                    .toc_url_with_url_captures(target)
                    .unwrap_or_else(|| setting.toc_url()),
            );
        }
    }
    None
}

fn has_novel_data_dir(target: &str) -> bool {
    let target_type = Downloader::get_target_type(target);
    let record = match target_type {
        TargetType::Id => {
            if let Ok(id) = target.parse::<i64>() {
                narou_rs::db::with_database(|db| Ok(db.get(id).cloned()))
                    .ok()
                    .flatten()
            } else {
                None
            }
        }
        _ => get_data_by_target(target).and_then(|info| {
            narou_rs::db::with_database(|db| Ok(db.get(info.id).cloned()))
                .ok()
                .flatten()
        }),
    };

    if let Some(rec) = record {
        let novel_dir = narou_rs::db::novel_dir_for_record(
            &std::path::PathBuf::from(narou_rs::downloader::ARCHIVE_ROOT_DIR),
            &rec,
        );
        novel_dir
            .join(narou_rs::downloader::SECTION_SAVE_DIR)
            .exists()
    } else {
        false
    }
}

fn is_novel_frozen(target: &str) -> bool {
    let data = get_data_by_target(target);
    if let Some(rec) = data {
        narou_rs::db::with_database(|db| {
            Ok(db
                .get(rec.id)
                .map(|r| r.tags.contains(&"frozen".to_string()))
                .unwrap_or(false))
        })
        .unwrap_or(false)
    } else {
        false
    }
}

fn print_download_status(multi: &Arc<MultiProgress>, dl: &narou_rs::downloader::DownloadResult) {
    match dl.status {
        UpdateStatus::Ok => {
            if dl.new_novel {
                let _ = multi.println(format!(
                    "{} のDL完了 (ID:{}, {}セクション)",
                    dl.title, dl.id, dl.total_count
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
        UpdateStatus::Canceled => {
            let _ = multi.println(format!(
                "ID:{} {} の更新はキャンセルされました",
                dl.id, dl.title
            ));
        }
        UpdateStatus::Failed => {}
    }
}

fn after_process(multi: &Arc<MultiProgress>, target: &str, opts: &DownloadOptions) {
    if opts.freeze {
        let _ = multi.println(format!("凍結: {}", target));
        super::manage::freeze_by_target(target);
    } else if opts.remove {
        let _ = multi.println(format!("削除: {}", target));
        super::manage::remove_by_target(target);
    }
}

fn auto_convert(
    multi: &Arc<MultiProgress>,
    dl: &narou_rs::downloader::DownloadResult,
) -> Result<(), String> {
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
