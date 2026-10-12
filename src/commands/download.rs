use std::io::{self, BufRead, IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use narou_rs::application::messages::{self, MessageSink, Stream};
use narou_rs::compat::{configure_web_subprocess_command, convert_existing_novel};
use narou_rs::downloader::{DownloadResult, Downloader, TargetType, UpdateStatus};
use narou_rs::mail::{
    MailSettingLoadError, ensure_mail_setting_file, load_mail_setting, send_target_with_setting,
};
use narou_rs::progress::{CliProgress, WebProgress, is_web_mode};

const EXIT_INTERRUPT: i32 = 126;

#[derive(Debug, Clone, Copy)]
struct DownloadInterrupted;

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

pub async fn cmd_download(opts: DownloadOptions, sink: &Arc<dyn MessageSink>) -> i32 {
    match cmd_download_inner(opts, sink).await {
        Ok(code) => code,
        Err(DownloadInterrupted) => {
            sink.emit(Stream::Stdout, messages::download::download_interrupted());
            EXIT_INTERRUPT
        }
    }
}

async fn cmd_download_inner(
    opts: DownloadOptions,
    sink: &Arc<dyn MessageSink>,
) -> std::result::Result<i32, DownloadInterrupted> {
    if let Err(e) = narou_rs::db::init_database() {
        sink.emit(Stream::Stderr, &messages::init_db_error(e));
        return Ok(127);
    }

    let mut downloader = match Downloader::with_user_agent(opts.user_agent.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            sink.emit(Stream::Stderr, &messages::downloader_create_error(e));
            return Ok(127);
        }
    };
    // レポート行もログに残す (narou.rb と同じ)。
    downloader.set_message_sink(narou_rs::progress::console_sink());

    let mut targets = opts.targets.clone();

    if targets.is_empty() {
        targets = interactive_mode(&downloader, sink);
        if targets.is_empty() {
            return Ok(0);
        }
    }

    targets = tagname_to_ids(&targets);

    let multi = CliProgress::multi();
    let multi_clone = multi.clone();
    let mut mistook = 0usize;
    let (expanded_targets, series_mistook) =
        expand_series_targets(&downloader, &targets, sink).await;
    targets = expanded_targets;
    mistook += series_mistook;

    for (i, target) in targets.iter().enumerate() {
        if i > 0 {
            sink.emit(Stream::Stdout, &messages::separator());
        }

        let mut download_target = target.clone();
        loop {
            let data = get_data_by_target(&download_target);

            if is_novel_frozen(&download_target) {
                if let Some(ref rec) = data {
                    sink.emit(
                        Stream::Stdout,
                        &messages::download::frozen_abort(&rec.title),
                    );
                }
                mistook += 1;
                break;
            }

            if !opts.force
                && let Some(existing) = inspect_existing_download(&download_target, sink)
            {
                match existing {
                    ExistingDownloadState::Present(rec) => {
                        sink.emit(
                            Stream::Stdout,
                            &messages::download::already_downloaded(
                                &download_target,
                                rec.id,
                                &rec.title,
                            ),
                        );
                        mistook += 1;
                        break;
                    }
                    ExistingDownloadState::Missing { record, path } => {
                        sink.emit(
                            Stream::Stderr,
                            &messages::download::missing_dir_index_removed(&path),
                        );
                        if confirm(
                            &messages::download::confirm_yes_no("再ダウンロードしますか"),
                            false,
                            true,
                            sink,
                        ) {
                            download_target = record.toc_url;
                            continue;
                        }
                        mistook += 1;
                        break;
                    }
                }
            }

            let progress: Box<dyn narou_rs::progress::ProgressReporter> = if is_web_mode() {
                Box::new(WebProgress::new("download"))
            } else {
                Box::new(CliProgress::with_multi(
                    &format!("DL {}", download_target),
                    multi_clone.clone(),
                ))
            };
            downloader.set_progress(progress);

            // 数値 ID が分かっているときはダウンロード中ロックする
            // (別レーンの変換と同時に触らない)。
            let locked_id = download_target.trim().parse::<i64>().ok();
            let download_result = {
                let _lock = narou_rs::compat::NovelLockGuard::acquire(locked_id);
                downloader
                    .download_novel_with_force(&download_target, opts.force)
                    .await
            };
            match download_result {
                Ok(dl) => {
                    print_download_status(&dl, sink);

                    match dl.status {
                        UpdateStatus::Ok => {}
                        UpdateStatus::None | UpdateStatus::Failed | UpdateStatus::Canceled => {
                            mistook += 1;
                            break;
                        }
                    }

                    if opts.no_convert {
                        after_process(&download_target, &opts, sink);
                    } else {
                        if let Err(e) = auto_convert(&dl) {
                            sink.emit(Stream::Stdout, &messages::download::convert_error_line(e));
                        }
                        after_process(&download_target, &opts, sink);
                    }
                }
                Err(e) => {
                    if matches!(e, narou_rs::error::NarouError::SuspendDownload(_)) {
                        return Err(DownloadInterrupted);
                    }
                    sink.emit(Stream::Stdout, &messages::indented_error(e));
                    mistook += 1;
                }
            }
            break;
        }
    }

    drop(multi);

    Ok(if mistook > 0 {
        mistook.min(127) as i32
    } else {
        0
    })
}

async fn expand_series_targets(
    downloader: &Downloader,
    targets: &[String],
    sink: &Arc<dyn MessageSink>,
) -> (Vec<String>, usize) {
    let mut expanded = Vec::new();
    let mut mistook = 0usize;
    for target in targets {
        match downloader.download_target_expander().expand(target).await {
            Ok(Some(items)) => {
                sink.emit(
                    Stream::Stdout,
                    &messages::download::series_expanded(target, items.len()),
                );
                expanded.extend(items);
            }
            Ok(None) => expanded.push(target.clone()),
            Err(err) => {
                sink.emit(Stream::Stdout, &messages::indented_error(err));
                mistook += 1;
            }
        }
    }
    (expanded, mistook)
}

fn interactive_mode(downloader: &Downloader, sink: &Arc<dyn MessageSink>) -> Vec<String> {
    if !std::io::stdin().is_terminal() {
        return Vec::new();
    }

    for line in messages::download::interactive_banner() {
        sink.emit(Stream::Stdout, line);
    }

    let mut targets = Vec::new();
    let stdin = io::stdin();
    print_prompt(targets.len(), sink);

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
                sink.emit(Stream::Stdout, messages::download::already_entered());
            } else {
                targets.push(input.to_string());
            }
        } else {
            sink.emit(Stream::Stdout, messages::download::unsupported_novel());
        }
        print_prompt(targets.len(), sink);
    }

    targets
}

fn print_prompt(count: usize, sink: &Arc<dyn MessageSink>) {
    sink.emit_fragment(
        Stream::Stdout,
        &messages::download::interactive_prompt(count),
    );
    let _ = io::stdout().flush();
}

fn valid_target(downloader: &Downloader, target: &str) -> bool {
    let target_type = Downloader::get_target_type(target);
    match target_type {
        TargetType::Ncode => true,
        TargetType::Url => downloader.site_setting_matches_url(target)
            || downloader.download_target_expander().recognizes(target),
        _ => false,
    }
}

pub(crate) fn tagname_to_ids(targets: &[String]) -> Vec<String> {
    let mut expanded = Vec::new();
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();

    // tag index 相当: 完全一致タグで絞った ID 一覧 (D1 では novel_tags join)。
    let tag_ids = |tag_name: &str| -> Vec<i64> {
        let filter = narou_rs::platform::NovelFilter {
            tag: Some(tag_name.to_string()),
            ..Default::default()
        };
        novels
            .scan_ids_sync(&filter, None, usize::MAX)
            .unwrap_or_default()
            .into_iter()
            .map(|id| id.0)
            .collect()
    };

    for target in targets {
        if let Some(tag_name) = target.strip_prefix("tag:") {
            let ids = tag_ids(tag_name);
            if ids.is_empty() {
                expanded.push(tag_name.to_string());
            } else {
                for id in ids {
                    let s = id.to_string();
                    if !expanded.contains(&s) {
                        expanded.push(s);
                    }
                }
            }
        } else if let Some(tag_name) = target.strip_prefix("^tag:") {
            let exclude_ids: std::collections::HashSet<i64> =
                tag_ids(tag_name).into_iter().collect();
            let filter = narou_rs::platform::NovelFilter::all();
            let all_ids: Vec<i64> = novels
                .scan_ids_sync(&filter, None, usize::MAX)
                .unwrap_or_default()
                .into_iter()
                .map(|id| id.0)
                .collect();
            for id in all_ids {
                if !exclude_ids.contains(&id) {
                    let s = id.to_string();
                    if !expanded.contains(&s) {
                        expanded.push(s);
                    }
                }
            }
        } else if let Ok(id) = target.parse::<i64>() {
            let exists = novels.get_sync(id.into()).ok().flatten().is_some();
            if exists {
                if !expanded.contains(&target.clone()) {
                    expanded.push(target.clone());
                }
            } else {
                let ids = tag_ids(target);
                if ids.is_empty() {
                    expanded.push(target.clone());
                } else {
                    for id in ids {
                        let s = id.to_string();
                        if !expanded.contains(&s) {
                            expanded.push(s);
                        }
                    }
                }
            }
        } else {
            let ids = tag_ids(target);
            if ids.is_empty() {
                expanded.push(target.clone());
            } else {
                for id in ids {
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
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let target_type = Downloader::get_target_type(&target);
    match target_type {
        TargetType::Id => {
            if let Ok(id) = target.parse::<i64>() {
                novels
                    .get_sync(id.into())
                    .ok()
                    .flatten()
                    .map(|r| RecordInfo {
                        id: r.id,
                        title: r.title,
                        toc_url: r.toc_url,
                    })
            } else {
                None
            }
        }
        TargetType::Url => {
            let toc_url = resolve_toc_url_from_url(&target)?;
            novels
                .find_by_toc_url_sync(&toc_url)
                .ok()
                .flatten()
                .map(|r| RecordInfo {
                    id: r.id,
                    title: r.title,
                    toc_url: r.toc_url,
                })
        }
        TargetType::Ncode => {
            novels
                .find_by_ncode_sync(&target)
                .ok()
                .flatten()
                .map(|r| RecordInfo {
                    id: r.id,
                    title: r.title,
                    toc_url: r.toc_url,
                })
        }
        _ => novels
            .find_by_title_sync(&target)
            .ok()
            .flatten()
            .map(|r| RecordInfo {
                id: r.id,
                title: r.title,
                toc_url: r.toc_url,
            }),
    }
}

/// Whether `target` is already managed.
///
/// Used by the author check to skip works that are already in the library
/// before going through the whole download path.
pub fn target_is_known(target: &str) -> bool {
    if let Some(toc_url) = resolve_toc_url_from_url(target)
        && let Ok(Some(_)) = narou_rs::native::novel_repository::NativeNovelRepository::new()
            .find_by_toc_url_sync(&toc_url)
    {
        return true;
    }
    let settings = narou_rs::downloader::site_setting::effective_site_settings();
    settings
        .iter()
        .filter(|setting| setting.matches_url(target))
        .filter_map(|setting| setting.extract_url_captures(target))
        .filter_map(|captures| captures.get("ncode").cloned())
        .any(|ncode| {
            narou_rs::native::novel_repository::NativeNovelRepository::new()
                .find_by_ncode_sync(&ncode)
                .ok()
                .flatten()
                .is_some()
        })
}

fn resolve_toc_url_from_url(target: &str) -> Option<String> {
    let settings = narou_rs::downloader::site_setting::effective_site_settings();
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

fn inspect_existing_download(
    target: &str,
    sink: &Arc<dyn MessageSink>,
) -> Option<ExistingDownloadState> {
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let record = get_record_for_target(target)?;
    let info = RecordInfo {
        id: record.id,
        title: record.title.clone(),
        toc_url: record.toc_url.clone(),
    };
    // archive_root は Inventory 依存 (Phase 4 対象)。
    let archive_root = narou_rs::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
        .unwrap_or_else(|_| std::path::PathBuf::from(narou_rs::downloader::ARCHIVE_ROOT_DIR));
    let novel_dir = narou_rs::db::existing_novel_dir_for_record(&archive_root, &record);
    if novel_dir.exists() {
        return Some(ExistingDownloadState::Present(info));
    }
    // mirror-files=false の構成では、有効な作品でも保存フォルダが無い。
    // ストアに本文 (toc.yaml) が残っているなら記録を消してはいけない。
    // ストアを確認できないときも消さない (誤削除は再取得不能を招く)。
    match narou_rs::native::object_store::novel_content_stored(&archive_root, &novel_dir) {
        Ok(true) => return Some(ExistingDownloadState::Present(info)),
        Ok(false) => {}
        Err(err) => {
            sink.emit(Stream::Stderr, &messages::indented_error(err));
            return Some(ExistingDownloadState::Present(info));
        }
    }

    if let Err(err) = novels.apply_batch_sync(vec![narou_rs::platform::NovelMutation::Remove(
        record.id.into(),
    )]) {
        sink.emit(
            Stream::Stderr,
            &messages::download::stale_index_cleanup_warn(err),
        );
    }

    Some(ExistingDownloadState::Missing {
        record: info,
        path: novel_dir,
    })
}

fn get_record_for_target(target: &str) -> Option<narou_rs::db::NovelRecord> {
    let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
    let target_type = Downloader::get_target_type(target);
    match target_type {
        TargetType::Id => {
            if let Ok(id) = target.parse::<i64>() {
                novels.get_sync(id.into()).ok().flatten()
            } else {
                None
            }
        }
        _ => get_data_by_target(target)
            .and_then(|info| novels.get_sync(info.id.into()).ok().flatten()),
    }
}

fn confirm(prompt: &str, default: bool, nontty_default: bool, sink: &Arc<dyn MessageSink>) -> bool {
    if !std::io::stdin().is_terminal() {
        return nontty_default;
    }

    loop {
        sink.emit_fragment(Stream::Stdout, prompt);
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

/// `narou download` の 1 件分の結果行。文言・順序・ストリームは Worker の
/// ジョブ経路 (`worker_entry::executor`) と揃える共有実装
/// (`application::emit_download_result_lines`) に寄せる。
fn print_download_status(dl: &narou_rs::downloader::DownloadResult, sink: &Arc<dyn MessageSink>) {
    narou_rs::application::emit_download_result_lines(
        sink.as_ref(),
        narou_rs::application::JobKind::Download,
        dl,
        None,
    );
}

fn after_process(target: &str, opts: &DownloadOptions, sink: &Arc<dyn MessageSink>) {
    if opts.mail {
        match load_mail_setting() {
            Ok(setting) => {
                if let Err(e) = send_target_with_setting(&setting, target, false, true) {
                    sink.emit(Stream::Stderr, &e);
                }
            }
            Err(MailSettingLoadError::NotFound(_)) => {
                if let Ok(path) = ensure_mail_setting_file() {
                    sink.emit(Stream::Stdout, &messages::mail::mail_setting_created(&path));
                    sink.emit(Stream::Stdout, messages::mail::mail_setting_file_notice());
                    sink.emit(
                        Stream::Stdout,
                        messages::mail::mail_setting_next_update_notice(),
                    );
                }
            }
            Err(MailSettingLoadError::Incomplete(path)) => {
                sink.emit(
                    Stream::Stderr,
                    &messages::mail::mail_setting_incomplete(&path),
                );
            }
            Err(e) => {
                sink.emit(Stream::Stderr, &messages::error_line(e));
            }
        }
    }
    if opts.freeze {
        sink.emit(Stream::Stdout, &messages::download::freeze_target(target));
        super::manage::freeze_by_target(target);
    } else if opts.remove {
        sink.emit(Stream::Stdout, &messages::download::remove_target(target));
        super::manage::remove_by_target(target);
    }
}

fn auto_convert(dl: &DownloadResult) -> Result<(), String> {
    if is_web_mode() {
        return auto_convert_via_web_subprocess(dl.id);
    }
    auto_convert_existing_download(dl)
}

fn auto_convert_existing_download(dl: &DownloadResult) -> Result<(), String> {
    convert_existing_novel(dl.id, &dl.title, &dl.author, &dl.novel_dir, true).map(|_| ())
}

fn auto_convert_via_web_subprocess(id: i64) -> Result<(), String> {
    if narou_rs::compat::load_local_setting_bool("concurrency") {
        super::enqueue_web_convert_job(id)?;
        return Ok(());
    }

    let exe_path = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(exe_path);
    command.arg("convert").arg("--no-open").arg(id.to_string());
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_web_subprocess_command(&mut command);

    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| messages::download::convert_stdout_unavailable().to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| messages::download::convert_stderr_unavailable().to_string())?;

    let stdout_thread = std::thread::spawn(move || {
        narou_rs::compat::relay_web_stream_to_console(stdout, "stdout2")
    });
    let stderr_thread = std::thread::spawn(move || {
        narou_rs::compat::relay_web_stream_to_console(stderr, "stdout2")
    });

    let status = child.wait().map_err(|e| e.to_string())?;
    stdout_thread
        .join()
        .map_err(|_| messages::download::convert_stdout_relay_panicked().to_string())??;
    stderr_thread
        .join()
        .map_err(|_| messages::download::convert_stderr_relay_panicked().to_string())??;

    if status.success() {
        Ok(())
    } else {
        Err(match status.code() {
            Some(code) => messages::download::convert_exit_code_failed(code),
            None => messages::download::convert_abnormal_exit().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::commands::download::{ExistingDownloadState, inspect_existing_download};

    use super::{EXIT_INTERRUPT, auto_convert_existing_download, parse_confirm_input};
    use narou_rs::downloader::{DownloadResult, UpdateStatus};
    use narou_rs::queue::{JobType, PersistentQueue};

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

    #[test]
    fn suspend_download_exit_code_matches_ruby() {
        assert_eq!(EXIT_INTERRUPT, 126);
    }

    #[test]
    fn download_auto_convert_uses_shared_auto_convert_device_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        std::fs::create_dir_all(temp.path().join("novel")).unwrap();
        std::fs::write(
            temp.path().join(".narou").join("local_setting.yaml"),
            "convert.multi-device: invalid-device\n",
        )
        .unwrap();
        *narou_rs::db::DATABASE.lock() = None;
        narou_rs::db::init_database().unwrap();

        let result = auto_convert_existing_download(&DownloadResult {
            id: 1,
            title: "title".to_string(),
            author: "author".to_string(),
            novel_dir: temp.path().join("novel"),
            new_novel: true,
            new_arrivals: true,
            new_arrival_subtitles: Vec::new(),
            updated_count: 1,
            total_count: 1,
            status: UpdateStatus::Ok,
            title_changed: false,
            author_changed: false,
            story_changed: false,
            sections_deleted: false,
        });

        assert_eq!(
            result.unwrap_err(),
            "有効な端末名がひとつもありませんでした"
        );
        *narou_rs::db::DATABASE.lock() = None;
    }

    #[test]
    fn enqueue_web_convert_job_pushes_convert_job_when_concurrency_is_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        std::fs::write(
            temp.path().join(".narou").join("local_setting.yaml"),
            "concurrency: true\n",
        )
        .unwrap();
        *narou_rs::db::DATABASE.lock() = None;
        narou_rs::db::init_database().unwrap();

        crate::commands::enqueue_web_convert_job(3062).unwrap();

        let queue = PersistentQueue::new(&temp.path().join(".narou").join("queue.yaml")).unwrap();
        assert_eq!(queue.pending_count(), 1);
        let snapshot = queue.snapshot();
        assert_eq!(
            snapshot.jobs.front().map(|job| job.job_type),
            Some(JobType::Convert)
        );
        assert_eq!(
            snapshot.jobs.front().map(|job| job.target.as_str()),
            Some("3062")
        );
        *narou_rs::db::DATABASE.lock() = None;
    }

    /// mirror-files=false の有効な作品は、保存フォルダが無くても
    /// 「ダウンロード済み」と判定され、記録を消されない。
    #[test]
    fn db_only_novel_is_present_without_folder_and_keeps_record() {
        use narou_rs::application::messages::{MessageSink, Stream};
        use narou_rs::native::sqlite::state;
        use narou_rs::platform::{NovelMutation, NovelObjectKeys, ObjectStore};

        struct TestSink;
        impl MessageSink for TestSink {
            fn emit(&self, _stream: Stream, _text: &str) {}
        }

        let root = tempfile::tempdir().unwrap();
        let narou_dir = root.path().join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        state::write_mode(&narou_dir, state::StorageMode::Sqlite).unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(root.path());
        narou_rs::db::init_database().unwrap();

        let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
        {
            use chrono::{TimeZone, Utc};
            novels
                .apply_batch_sync(vec![NovelMutation::Upsert(narou_rs::db::NovelRecord {
                    id: 1,
                    author: "author".to_string(),
                    title: "DbOnly".to_string(),
                    file_title: "db-only".to_string(),
                    toc_url: "https://example.test/1".to_string(),
                    sitename: "example.test".to_string(),
                    novel_type: 1,
                    end: false,
                    last_update: Utc.with_ymd_and_hms(2026, 4, 20, 0, 0, 0).unwrap(),
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
                    requires_login: false,
                    login_session: None,
                    extra_fields: Default::default(),
                })])
                .unwrap();
        }

        let archive_root = root.path().join("小説データ");
        let keys = NovelObjectKeys::new("example.test", "db-only", false).unwrap();
        let handle = state::active_for(&narou_dir).expect("sqlite state");
        let mirror = narou_rs::native::object_store::NativeObjectStore::from_root(
            archive_root.clone(),
        )
        .unwrap();
        let store = narou_rs::native::sqlite::object_store::SqliteObjectStore::with_mirror_files(
            handle.conn_ref().clone(),
            mirror,
            false,
        );
        futures::executor::block_on(store.write_small(
            &keys.toc(),
            b"title: DbOnly\nauthor: author\ntoc_url: https://example.test/1\nsubtitles: []\n"
                .to_vec(),
        ))
        .unwrap();

        let sink: std::sync::Arc<dyn MessageSink> = std::sync::Arc::new(TestSink);
        let inspected = inspect_existing_download("1", &sink);
        assert!(matches!(
            inspected,
            Some(ExistingDownloadState::Present(_))
        ));
        let kept = novels
            .get_sync(1i64.into())
            .unwrap()
            .expect("DB-only record must not be deleted");
        assert_eq!(kept.id, 1);

        *narou_rs::db::DATABASE.lock() = None;
    }
}
