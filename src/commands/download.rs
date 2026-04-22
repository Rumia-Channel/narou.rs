use std::io::{self, BufRead, IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use indicatif::MultiProgress;

use narou_rs::converter::NovelConverter;
use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::downloader::{Downloader, TargetType, UpdateStatus};
use narou_rs::mail::{
    MailSettingLoadError, ensure_mail_setting_file, load_mail_setting, send_target_with_setting,
};
use narou_rs::progress::{CliProgress, WebProgress, is_web_mode};

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

pub fn cmd_download(opts: DownloadOptions) -> i32 {
    match std::thread::spawn(move || cmd_download_inner(opts)).join() {
        Ok(code) => code,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn cmd_download_inner(opts: DownloadOptions) -> i32 {
    if let Err(e) = narou_rs::db::init_database() {
        eprintln!("Error initializing database: {}", e);
        return 127;
    }

    let mut downloader = match Downloader::with_user_agent(opts.user_agent.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error creating downloader: {}", e);
            return 127;
        }
    };

    let mut targets = opts.targets.clone();

    if targets.is_empty() {
        targets = interactive_mode(&downloader);
        if targets.is_empty() {
            return 0;
        }
    }

    targets = tagname_to_ids(&targets);

    let multi = CliProgress::multi();
    let multi_clone = multi.clone();
    let mut mistook = 0usize;

    for (i, target) in targets.iter().enumerate() {
        if i > 0 {
            println!("{}", "\u{2015}".repeat(35));
        }

        let mut download_target = target.clone();
        loop {
            let data = get_data_by_target(&download_target);

            if is_novel_frozen(&download_target) {
                if let Some(ref rec) = data {
                    println!(
                        "{} は凍結中です\nダウンロードを中止しました",
                        rec.title
                    );
                }
                mistook += 1;
                break;
            }

            if !opts.force {
                if let Some(existing) = inspect_existing_download(&download_target) {
                    match existing {
                        ExistingDownloadState::Present(rec) => {
                            println!(
                                "{} はダウンロード済みです。\nID: {}\ntitle: {}",
                                download_target, rec.id, rec.title
                            );
                            mistook += 1;
                            break;
                        }
                        ExistingDownloadState::Missing { record, path } => {
                            eprintln!(
                                "{} が見つかりません。\n保存フォルダが消去されていたため、データベースのインデックスを削除しました。",
                                path.display()
                            );
                            if confirm("再ダウンロードしますか", false, true) {
                                download_target = record.toc_url;
                                continue;
                            }
                            mistook += 1;
                            break;
                        }
                    }
                }
            }

            let progress: Box<dyn narou_rs::progress::ProgressReporter> = if is_web_mode() {
                Box::new(WebProgress::new("download"))
            } else {
                Box::new(CliProgress::with_multi(&format!("DL {}", download_target), multi_clone.clone()))
            };
            downloader.set_progress(progress);

            match downloader.download_novel_with_force(&download_target, opts.force) {
                Ok(dl) => {
                    print_download_status(&dl);

                    match dl.status {
                        UpdateStatus::Ok => {}
                        UpdateStatus::None | UpdateStatus::Failed | UpdateStatus::Canceled => {
                            mistook += 1;
                            break;
                        }
                    }

                    if opts.no_convert {
                        after_process(&download_target, &opts);
                    } else {
                        if let Err(e) = auto_convert(&multi_clone, &dl) {
                            println!("  Convert error: {}", e);
                        }
                        after_process(&download_target, &opts);
                    }
                }
                Err(e) => {
                    if matches!(e, narou_rs::error::NarouError::SuspendDownload(_)) {
                        std::panic::resume_unwind(Box::new(e.to_string()));
                    }
                    println!("  Error: {}", e);
                    mistook += 1;
                }
            }
            break;
        }
    }

    drop(multi);

    if mistook > 0 {
        mistook.min(127) as i32
    } else {
        0
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
                println!("入力済みです");
            } else {
                targets.push(input.to_string());
            }
        } else {
            println!("対応外の小説です");
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

pub(crate) fn tagname_to_ids(targets: &[String]) -> Vec<String> {
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
                expanded.push(tag_name.to_string());
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
            let mut all_ids = narou_rs::db::with_database(|db| Ok(db.ids())).unwrap_or_default();
            all_ids.sort_unstable();
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

pub(crate) struct RecordInfo {
    pub(crate) id: i64,
    pub(crate) title: String,
    pub(crate) toc_url: String,
}

enum ExistingDownloadState {
    Present(RecordInfo),
    Missing {
        record: RecordInfo,
        path: std::path::PathBuf,
    },
}

pub(crate) fn get_data_by_target(target: &str) -> Option<RecordInfo> {
    let target = super::resolve_alias_target(target);
    let target_type = Downloader::get_target_type(&target);
    match target_type {
        TargetType::Id => {
            if let Ok(id) = target.parse::<i64>() {
                narou_rs::db::with_database(|db| {
                    Ok(db.get(id).map(|r| RecordInfo {
                        id: r.id,
                        title: r.title.clone(),
                        toc_url: r.toc_url.clone(),
                    }))
                })
                .ok()
                .flatten()
            } else {
                None
            }
        }
        TargetType::Url => {
            let toc_url = resolve_toc_url_from_url(&target)?;
            narou_rs::db::with_database(|db| {
                Ok(db.get_by_toc_url(&toc_url).map(|r| RecordInfo {
                    id: r.id,
                    title: r.title.clone(),
                    toc_url: r.toc_url.clone(),
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
                            toc_url: r.toc_url.clone(),
                        }));
                    }
                }
                Ok::<Option<RecordInfo>, narou_rs::error::NarouError>(None)
            })
            .ok()
            .flatten()
        }
        _ => narou_rs::db::with_database(|db| {
            Ok(db.find_by_title(&target).map(|r| RecordInfo {
                id: r.id,
                title: r.title.clone(),
                toc_url: r.toc_url.clone(),
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

fn inspect_existing_download(target: &str) -> Option<ExistingDownloadState> {
    let record = get_record_for_target(target)?;
    let info = RecordInfo {
        id: record.id,
        title: record.title.clone(),
        toc_url: record.toc_url.clone(),
    };
    let archive_root = narou_rs::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
        .unwrap_or_else(|_| std::path::PathBuf::from(narou_rs::downloader::ARCHIVE_ROOT_DIR));
    let novel_dir = narou_rs::db::existing_novel_dir_for_record(&archive_root, &record);
    if novel_dir.exists() {
        return Some(ExistingDownloadState::Present(info));
    }

    if let Err(err) = narou_rs::db::with_database_mut(|db| {
        db.remove(record.id);
        db.save()
    }) {
        eprintln!("Warning: stale database index cleanup failed: {}", err);
    }

    Some(ExistingDownloadState::Missing {
        record: info,
        path: novel_dir,
    })
}

fn get_record_for_target(target: &str) -> Option<narou_rs::db::NovelRecord> {
    let target_type = Downloader::get_target_type(target);
    match target_type {
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
    }
}

fn confirm(message: &str, default: bool, nontty_default: bool) -> bool {
    if !std::io::stdin().is_terminal() {
        return nontty_default;
    }

    loop {
        print!("{} (y/n)?: ", message);
        let _ = io::stdout().flush();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).unwrap_or(0) == 0 {
            return nontty_default;
        }
        if let Some(answer) = parse_confirm_input(&input, default) {
            return answer;
        }
    }
}

fn parse_confirm_input(input: &str, default: bool) -> Option<bool> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Some(default);
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

fn is_novel_frozen(target: &str) -> bool {
    get_data_by_target(target)
        .map(|rec| narou_rs::compat::is_frozen_id(rec.id))
        .unwrap_or(false)
}

fn print_download_status(dl: &narou_rs::downloader::DownloadResult) {
    match dl.status {
        UpdateStatus::Ok => {
            if dl.new_novel {
                println!(
                    "{} のDL完了 (ID:{}, {}セクション)",
                    dl.title, dl.id, dl.total_count
                );
            } else if dl.updated_count > 0 {
                println!(
                    "{} の更新完了 (ID:{}, {}/{}話更新)",
                    dl.title, dl.id, dl.updated_count, dl.total_count
                );
            } else if dl.title_changed {
                println!(
                    "ID:{} {} のタイトルが更新されています",
                    dl.id, dl.title
                );
            } else if dl.story_changed {
                println!(
                    "ID:{} {} のあらすじが更新されています",
                    dl.id, dl.title
                );
            } else if dl.author_changed {
                println!(
                    "ID:{} {} の作者名が更新されています",
                    dl.id, dl.title
                );
            }
        }
        UpdateStatus::None => {
            println!("{} に更新はありません", dl.title);
        }
        UpdateStatus::Canceled => {
            println!(
                "ID:{} {} の更新はキャンセルされました",
                dl.id, dl.title
            );
        }
        UpdateStatus::Failed => {}
    }
}

fn after_process(target: &str, opts: &DownloadOptions) {
    if opts.mail {
        match load_mail_setting() {
            Ok(setting) => {
                if let Err(e) = send_target_with_setting(&setting, target, false, true) {
                    eprintln!("{}", e);
                }
            }
            Err(MailSettingLoadError::NotFound(_)) => {
                if let Ok(path) = ensure_mail_setting_file() {
                    println!("created {}", path.display());
                    println!("メールの設定用ファイルを作成しました。設定ファイルを書き換えることで mail コマンドが有効になります。");
                    println!(
                        "注意：次回以降のupdateで新着があった場合に送信可能フラグが立ちます",
                    );
                }
            }
            Err(MailSettingLoadError::Incomplete(path)) => {
                eprintln!(
                    "設定ファイルの書き換えが終了していないようです。\n設定ファイルは {} にあります",
                    path.display()
                );
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    }
    if opts.freeze {
        println!("凍結: {}", target);
        super::manage::freeze_by_target(target);
    } else if opts.remove {
        println!("削除: {}", target);
        super::manage::remove_by_target(target);
    }
}

fn auto_convert(
    multi: &Arc<MultiProgress>,
    dl: &narou_rs::downloader::DownloadResult,
) -> Result<(), String> {
    if is_web_mode() {
        return auto_convert_via_web_subprocess(dl.id);
    }

    let settings = NovelSettings::load_for_novel(dl.id, &dl.title, &dl.author, &dl.novel_dir);
    let mut converter = if let Some(uc) = UserConverter::load_with_title(&dl.novel_dir, &dl.title) {
        NovelConverter::with_user_converter(settings, uc)
    } else {
        NovelConverter::new(settings)
    };

    let progress: Box<dyn narou_rs::progress::ProgressReporter> = if is_web_mode() {
        Box::new(WebProgress::new("convert"))
    } else {
        Box::new(CliProgress::with_multi(&format!("Convert {}", dl.title), multi.clone()))
    };
    converter.set_progress(progress);

    let _lock = narou_rs::compat::NovelLockGuard::acquire(Some(dl.id))
        .map_err(|e| e.to_string())?;
    match converter.convert_novel_by_id(dl.id, &dl.novel_dir) {
        Ok(path) => {
            println!("  Converted: {}", path);
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn auto_convert_via_web_subprocess(id: i64) -> Result<(), String> {
    let exe_path = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(exe_path);
    command.arg("convert").arg("--no-open").arg(id.to_string());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "convert stdout を取得できません".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "convert stderr を取得できません".to_string())?;

    let stdout_thread =
        std::thread::spawn(move || narou_rs::compat::relay_web_stream_to_console(stdout, "stdout2"));
    let stderr_thread =
        std::thread::spawn(move || narou_rs::compat::relay_web_stream_to_console(stderr, "stdout2"));

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

#[cfg(test)]
mod tests {
    use super::parse_confirm_input;

    #[test]
    fn parse_confirm_input_accepts_yes_and_no() {
        assert_eq!(parse_confirm_input("y", false), Some(true));
        assert_eq!(parse_confirm_input("yes", false), Some(true));
        assert_eq!(parse_confirm_input("n", true), Some(false));
        assert_eq!(parse_confirm_input("no", true), Some(false));
    }

    #[test]
    fn parse_confirm_input_uses_default_on_empty() {
        assert_eq!(parse_confirm_input("", false), Some(false));
        assert_eq!(parse_confirm_input("\n", true), Some(true));
    }

    #[test]
    fn parse_confirm_input_rejects_invalid_values() {
        assert_eq!(parse_confirm_input("maybe", false), None);
    }
}
