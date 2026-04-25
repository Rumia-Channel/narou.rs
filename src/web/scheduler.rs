use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::process::{ChildStderr, ChildStdout, Stdio};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, LocalResult, NaiveDate, TimeZone, Utc};
use serde_yaml::{Number, Value};
use tokio::task::JoinHandle;

use crate::compat::{
    configure_web_subprocess_command, load_frozen_ids_from_inventory, load_local_setting_bool,
    load_local_setting_string, load_local_setting_value, record_is_frozen,
};
use crate::db;
use crate::db::inventory::{Inventory, InventoryScope};
use crate::downloader::site_setting::SiteSetting;
use crate::progress::{WEB_PROGRESS_SCOPE_ENV, WS_LINE_PREFIX};
use crate::queue::{JobType, PersistentQueue, QueueJob};
use crate::termcolor::colored;

use super::push::PushServer;
use super::sort_state::{current_sort_from_server_setting, normalize_sort_key, sort_column_key};

const AUTO_UPDATE_LAST_RUN_KEY: &str = "update.auto-schedule.last-run";
pub fn start_auto_update_scheduler(
    queue: Arc<PersistentQueue>,
    running_jobs: Arc<parking_lot::Mutex<Vec<QueueJob>>>,
    push_server: Arc<PushServer>,
) -> Option<JoinHandle<()>> {
    let enabled = load_local_setting_bool("update.auto-schedule.enable");
    let schedule_string = load_local_setting_string("update.auto-schedule")?;
    if !enabled || schedule_string.trim().is_empty() {
        return None;
    }

    let times = parse_schedule_times(&schedule_string);
    if times.is_empty() {
        eprintln!(
            "自動アップデートスケジューラーの時刻指定が不正です: {}",
            schedule_string
        );
        push_server.broadcast_echo(
            &format!(
                "自動アップデートスケジューラーの時刻指定が不正です: {}",
                schedule_string
            ),
            "stdout",
        );
        return None;
    }

    let catch_up_run = missed_run_time(&times, load_last_auto_update_run(), Local::now());
    Some(tokio::spawn(async move {
        if let Some(missed_run) = catch_up_run {
            run_scheduled_auto_update(
                queue.as_ref(),
                &running_jobs,
                push_server.as_ref(),
                missed_run,
                true,
            );
        }

        loop {
            let Some(next_run) = calculate_next_run_time(&times) else {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                continue;
            };

            sleep_until(next_run).await;
            run_scheduled_auto_update(
                queue.as_ref(),
                &running_jobs,
                push_server.as_ref(),
                next_run,
                false,
            );
        }
    }))
}

pub fn start_or_restart_auto_update_scheduler(
    queue: Arc<PersistentQueue>,
    running_jobs: Arc<parking_lot::Mutex<Vec<QueueJob>>>,
    push_server: Arc<PushServer>,
    scheduler_task: &parking_lot::Mutex<Option<JoinHandle<()>>>,
) -> bool {
    if let Some(task) = scheduler_task.lock().take() {
        task.abort();
    }

    let task = start_auto_update_scheduler(queue, running_jobs, push_server);
    let started = task.is_some();
    *scheduler_task.lock() = task;
    started
}

pub fn restart_auto_update_scheduler(
    queue: Arc<PersistentQueue>,
    running_jobs: Arc<parking_lot::Mutex<Vec<QueueJob>>>,
    push_server: Arc<PushServer>,
    scheduler_task: &parking_lot::Mutex<Option<JoinHandle<()>>>,
) -> bool {
    start_or_restart_auto_update_scheduler(queue, running_jobs, push_server, scheduler_task)
}

fn run_scheduled_auto_update(
    queue: &PersistentQueue,
    running_jobs: &parking_lot::Mutex<Vec<QueueJob>>,
    push_server: &PushServer,
    scheduled_time: chrono::DateTime<Local>,
    catch_up: bool,
) {
    let now = Local::now();
    if let Err(message) = persist_last_auto_update_run(now) {
        push_server.broadcast_echo(
            &format!("自動アップデート最終実行時刻の保存に失敗しました: {}", message),
            "stdout",
        );
    }

    let heading = if catch_up {
        format!(
            "自動アップデートを catch-up 実行します: {}",
            scheduled_time.format("%Y/%m/%d %H:%M:%S")
        )
    } else {
        format!(
            "自動アップデートが予定されています: {}",
            scheduled_time.format("%Y/%m/%d %H:%M:%S")
        )
    };
    push_server.broadcast_echo(&heading, "stdout");

    match queue_auto_update_job_if_needed(queue, running_jobs) {
        Ok((job_id, true)) => {
            push_server.broadcast_echo(
                &format!("自動アップデートをキューに追加しました ({})", job_id),
                "stdout",
            );
            push_server.broadcast_event("notification.queue", "");
        }
        Ok((_, false)) => {
            push_server.broadcast_echo(
                "自動アップデートは既にキューまたは実行中に存在します",
                "stdout",
            );
        }
        Err(message) => {
            push_server.broadcast_echo(
                &format!("自動アップデートのキュー追加に失敗しました: {}", message),
                "stdout",
            );
        }
    }
}

pub fn stop_auto_update_scheduler(scheduler_task: &parking_lot::Mutex<Option<JoinHandle<()>>>) {
    if let Some(task) = scheduler_task.lock().take() {
        task.abort();
    }
}

fn parse_schedule_times(schedule_string: &str) -> Vec<(u32, u32)> {
    let mut times: Vec<(u32, u32)> = schedule_string
        .split(',')
        .filter_map(|value| {
            let trimmed = value.trim();
            if trimmed.len() != 4 || !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
                return None;
            }

            let hour = trimmed[0..2].parse::<u32>().ok()?;
            let minute = trimmed[2..4].parse::<u32>().ok()?;
            (hour < 24 && minute < 60).then_some((hour, minute))
        })
        .collect();
    times.sort_unstable();
    times.dedup();
    times
}

fn calculate_next_run_time(times: &[(u32, u32)]) -> Option<chrono::DateTime<Local>> {
    calculate_next_run_time_after(times, Local::now())
}

fn calculate_next_run_time_after<Tz>(
    times: &[(u32, u32)],
    after: DateTime<Tz>,
) -> Option<DateTime<Tz>>
where
    Tz: TimeZone,
    Tz::Offset: Copy,
{
    for &(hour, minute) in times {
        let candidate = local_datetime_in_timezone(
            &after.timezone(),
            after.year(),
            after.month(),
            after.day(),
            hour,
            minute,
        )?;
        if candidate > after {
            return Some(candidate);
        }
    }

    let tomorrow = after.date_naive().succ_opt()?;
    let (hour, minute) = *times.first()?;
    local_datetime_in_timezone(
        &after.timezone(),
        tomorrow.year(),
        tomorrow.month(),
        tomorrow.day(),
        hour,
        minute,
    )
}

fn local_datetime_in_timezone<Tz>(
    timezone: &Tz,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Option<DateTime<Tz>>
where
    Tz: TimeZone,
    Tz::Offset: Copy,
{
    let mut candidate = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)?;
    let start_date = candidate.date();
    let max_date = start_date.succ_opt()?;

    loop {
        match timezone.from_local_datetime(&candidate) {
            LocalResult::Single(value) => return Some(value),
            LocalResult::Ambiguous(first, _) => return Some(first),
            LocalResult::None => {
                candidate = candidate.checked_add_signed(ChronoDuration::minutes(1))?;
                if candidate.date() > max_date {
                    return None;
                }
            }
        }
    }
}

fn missed_run_time(
    times: &[(u32, u32)],
    last_run: Option<DateTime<Local>>,
    now: DateTime<Local>,
) -> Option<DateTime<Local>> {
    let last_run = last_run?;
    if last_run >= now {
        return None;
    }
    let missed = calculate_next_run_time_after(times, last_run)?;
    (missed < now).then_some(missed)
}

fn load_last_auto_update_run() -> Option<DateTime<Local>> {
    let timestamp = match load_local_setting_value(AUTO_UPDATE_LAST_RUN_KEY)? {
        Value::Number(value) => value.as_i64()?,
        Value::String(text) => text.parse::<i64>().ok()?,
        _ => return None,
    };
    Utc.timestamp_opt(timestamp, 0).single().map(|value| value.with_timezone(&Local))
}

fn persist_last_auto_update_run(timestamp: DateTime<Local>) -> Result<(), String> {
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let mut settings: HashMap<String, Value> = inventory
        .load("local_setting", InventoryScope::Local)
        .unwrap_or_default();
    settings.insert(
        AUTO_UPDATE_LAST_RUN_KEY.to_string(),
        Value::Number(Number::from(timestamp.timestamp())),
    );
    inventory
        .save("local_setting", InventoryScope::Local, &settings)
        .map_err(|e| e.to_string())
}

async fn sleep_until(target_time: chrono::DateTime<Local>) {
    loop {
        let now = Local::now();
        if now >= target_time {
            break;
        }

        let remaining = (target_time - now).num_seconds().max(1) as u64;
        tokio::time::sleep(Duration::from_secs(remaining.min(60))).await;
    }
}

pub fn execute_auto_update(
    root_dir: &Path,
    push_server: Arc<PushServer>,
    job_id: &str,
    running_pids: Arc<parking_lot::Mutex<HashMap<String, u32>>>,
) -> bool {
    auto_update_echo(
        push_server.as_ref(),
        &format!(
            "自動アップデートを実行中... ({})",
            Local::now().format("%Y/%m/%d %H:%M:%S")
        ),
    );

    let sort_args = build_auto_update_sort_args(push_server.as_ref());
    if !run_update_phase(
        root_dir,
        &["--gl", "narou"],
        "なろうAPIによる更新確認",
        &push_server,
        job_id,
        &running_pids,
    ) {
        auto_update_echo(
            push_server.as_ref(),
            "自動アップデート失敗: なろうAPI更新確認",
        );
        return false;
    }

    let (modified_ids, other_ids) = collect_auto_update_target_ids();

    if modified_ids.is_empty() {
        auto_update_echo(
            push_server.as_ref(),
            "自動アップデート: modified タグの付いた小説はありません",
        );
    } else {
        auto_update_echo(
            push_server.as_ref(),
            &colored("modified タグの付いた小説を更新します", "yellow"),
        );
        auto_update_echo(
            push_server.as_ref(),
            &format!(
                "自動アップデート: modified タグの付いた小説を更新します ({}件)",
                modified_ids.len()
            ),
        );
        let mut args = sort_args.clone();
        args.extend(modified_ids.iter().map(String::as_str));
        if !run_update_phase(
            root_dir,
            &args,
            "modified タグ更新",
            &push_server,
            job_id,
            &running_pids,
        ) {
            auto_update_echo(
                push_server.as_ref(),
                "自動アップデート失敗: modified タグ更新",
            );
            return false;
        }
    }

    if other_ids.is_empty() {
        auto_update_echo(
            push_server.as_ref(),
            "自動アップデート: 通常更新の対象となるその他小説はありません",
        );
    } else {
        auto_update_echo(
            push_server.as_ref(),
            &format!(
                "自動アップデート: その他小説を通常更新します ({}件)",
                other_ids.len()
            ),
        );
        let mut args = sort_args;
        args.extend(other_ids.iter().map(String::as_str));
        if !run_update_phase(
            root_dir,
            &args,
            "その他小説更新",
            &push_server,
            job_id,
            &running_pids,
        ) {
            auto_update_echo(push_server.as_ref(), "自動アップデート失敗: その他小説更新");
            return false;
        }
    }

    auto_update_echo(push_server.as_ref(), "自動アップデートが正常に完了しました");
    push_server.broadcast_event("table.reload", "");
    push_server.broadcast_event("tag.updateCanvas", "");
    true
}

fn build_auto_update_sort_args(push_server: &PushServer) -> Vec<&'static str> {
    let Some(sort_key) = read_auto_update_sort_key() else {
        auto_update_echo(push_server, "自動アップデート: デフォルトソート順序で実行");
        return Vec::new();
    };

    auto_update_echo(
        push_server,
        &format!("自動アップデート: WebUIソート設定を適用 ({})", sort_key),
    );
    vec!["--sort-by", sort_key]
}

fn read_auto_update_sort_key() -> Option<&'static str> {
    let inventory = Inventory::with_default_root().ok()?;
    let server_setting: Value = inventory
        .load("server_setting", InventoryScope::Global)
        .ok()?;
    auto_update_sort_key_from_value(&server_setting)
}

fn auto_update_sort_key_from_value(server_setting: &Value) -> Option<&'static str> {
    let current_sort = current_sort_from_server_setting(server_setting)?;
    let key = sort_column_key(&current_sort)?;
    normalize_sort_key(key)
}

fn collect_auto_update_target_ids() -> (Vec<String>, Vec<String>) {
    let site_settings = SiteSetting::load_all().unwrap_or_default();
    db::with_database(|db| {
        let frozen_ids = load_frozen_ids_from_inventory(db.inventory()).unwrap_or_default();
        let modified_ids = db
            .tag_index()
            .get("modified")
            .into_iter()
            .flat_map(|ids| ids.iter().copied())
            .collect::<std::collections::BTreeSet<_>>();
        Ok::<_, crate::error::NarouError>(split_auto_update_target_ids(
            db.all_records().values(),
            &modified_ids,
            &frozen_ids,
            |record| {
                site_settings
                    .iter()
                    .find(|setting| setting.matches_url(&record.toc_url))
                    .and_then(|setting| setting.narou_api_url.as_ref())
                    .is_some()
            },
        ))
    })
    .unwrap_or_default()
}

fn split_auto_update_target_ids<'a, I, F>(
    records: I,
    modified_ids: &std::collections::BTreeSet<i64>,
    frozen_ids: &std::collections::HashSet<i64>,
    mut api_supported: F,
) -> (Vec<String>, Vec<String>)
where
    I: IntoIterator<Item = &'a crate::db::NovelRecord>,
    F: FnMut(&crate::db::NovelRecord) -> bool,
{
    let mut modified = Vec::new();
    let mut other = Vec::new();
    for record in records {
        if record_is_frozen(record, frozen_ids) {
            continue;
        }
        if modified_ids.contains(&record.id) {
            modified.push(record.id.to_string());
            continue;
        }
        if api_supported(record) {
            continue;
        }
        other.push(record.id.to_string());
    }
    (modified, other)
}

fn queue_auto_update_job_if_needed(
    queue: &PersistentQueue,
    running_jobs: &parking_lot::Mutex<Vec<QueueJob>>,
) -> std::result::Result<(String, bool), String> {
    if let Some(existing_id) = running_jobs
        .lock()
        .iter()
        .find(|job| matches!(job.job_type, JobType::AutoUpdate))
        .map(|job| job.id.clone())
    {
        return Ok((existing_id, false));
    }

    if let Some(existing_id) = queue
        .get_pending_tasks()
        .into_iter()
        .find(|job| matches!(job.job_type, JobType::AutoUpdate))
        .map(|job| job.id)
    {
        return Ok((existing_id, false));
    }

    queue
        .push(JobType::AutoUpdate, "")
        .map(|id| (id, true))
        .map_err(|e| e.to_string())
}

fn run_update_phase(
    root_dir: &Path,
    args: &[&str],
    label: &str,
    push_server: &Arc<PushServer>,
    job_id: &str,
    running_pids: &Arc<parking_lot::Mutex<HashMap<String, u32>>>,
) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        auto_update_echo(
            push_server.as_ref(),
            &format!(
                "{} で重大なエラーが発生しました（実行ファイルを取得できません）",
                label
            ),
        );
        return false;
    };

    let mut command = std::process::Command::new(exe);
    command
        .current_dir(root_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("update")
        .args(args);
    configure_web_subprocess_command(&mut command);
    command.env(WEB_PROGRESS_SCOPE_ENV, job_id);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            auto_update_echo(
                push_server.as_ref(),
                &format!(
                    "{} で重大なエラーが発生しました（update を起動できません: {}）",
                    label, e
                ),
            );
            return false;
        }
    };

    running_pids.lock().insert(job_id.to_string(), child.id());
    let stdout_thread = relay_child_stdout(child.stdout.take(), Arc::clone(push_server));
    let stderr_thread = relay_child_stderr(child.stderr.take(), Arc::clone(push_server));

    let status = child.wait();
    running_pids.lock().remove(job_id);
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    let Ok(status) = status else {
        auto_update_echo(
            push_server.as_ref(),
            &format!(
                "{} で重大なエラーが発生しました（update の終了待機に失敗しました）",
                label
            ),
        );
        return false;
    };

    let Some(code) = status.code() else {
        auto_update_echo(
            push_server.as_ref(),
            &format!("{} で重大なエラーが発生しました（終了コード不明）", label),
        );
        return false;
    };

    match code {
        0 => {
            auto_update_echo(push_server.as_ref(), &format!("{} が完了しました", label));
            refresh_database_after_phase(label, push_server.as_ref())
        }
        1..=127 => {
            auto_update_echo(
                push_server.as_ref(),
                &format!(
                    "{} が完了しました（{}件の小説でエラーがありました）",
                    label, code
                ),
            );
            refresh_database_after_phase(label, push_server.as_ref())
        }
        _ => {
            auto_update_echo(
                push_server.as_ref(),
                &format!(
                    "{} で重大なエラーが発生しました（終了コード: {}）",
                    label, code
                ),
            );
            false
        }
    }
}

fn refresh_database_after_phase(label: &str, push_server: &PushServer) -> bool {
    match db::with_database_mut(|db| db.refresh()) {
        Ok(()) => true,
        Err(e) => {
            auto_update_echo(
                push_server,
                &format!("{} 後のDB再読み込みに失敗しました: {}", label, e),
            );
            false
        }
    }
}

fn relay_child_stdout(
    stdout: Option<ChildStdout>,
    push_server: Arc<PushServer>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(out) = stdout else {
            return;
        };

        let reader = std::io::BufReader::new(out);
        for line in reader.lines() {
            let Ok(text) = line else {
                break;
            };
            relay_stdout_line(push_server.as_ref(), &text);
        }
    })
}

fn relay_child_stderr(
    stderr: Option<ChildStderr>,
    push_server: Arc<PushServer>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(err) = stderr else {
            return;
        };

        let reader = std::io::BufReader::new(err);
        for line in reader.lines() {
            match line {
                Ok(text) => auto_update_echo(push_server.as_ref(), &text),
                Err(_) => break,
            }
        }
    })
}

fn relay_stdout_line(push_server: &PushServer, text: &str) {
    let Some(json_str) = text.strip_prefix(WS_LINE_PREFIX) else {
        auto_update_echo(push_server, text);
        return;
    };

    match serde_json::from_str::<serde_json::Value>(json_str) {
        Ok(message) => push_server.broadcast_raw(&message),
        Err(_) => auto_update_echo(push_server, text),
    }
}

fn auto_update_echo(push_server: &PushServer, body: &str) {
    push_server.broadcast_echo(body, "stdout");
}

#[cfg(test)]
mod tests {
    use super::{
        auto_update_sort_key_from_value, calculate_next_run_time, calculate_next_run_time_after,
        local_datetime_in_timezone, missed_run_time, parse_schedule_times,
        queue_auto_update_job_if_needed, split_auto_update_target_ids,
    };
    use crate::web::sort_state::SORT_COLUMN_KEYS;
    use crate::db::NovelRecord;
    use crate::queue::{JobType, PersistentQueue, QueueJob};
    use chrono::{Local, TimeZone, Timelike, Utc};
    use chrono_tz::America::New_York;
    use parking_lot::Mutex;
    use std::collections::{BTreeMap, BTreeSet, HashSet};

    fn sample_record(id: i64, toc_url: &str, tags: &[&str]) -> NovelRecord {
        NovelRecord {
            id,
            author: "author".to_string(),
            title: format!("title-{id}"),
            file_title: format!("{id} title-{id}"),
            toc_url: toc_url.to_string(),
            sitename: "site".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            extra_fields: BTreeMap::new(),
        }
    }

    #[test]
    fn parse_schedule_times_accepts_four_digit_times() {
        assert_eq!(parse_schedule_times("0930, 2215"), vec![(9, 30), (22, 15)]);
        assert!(parse_schedule_times("9999").is_empty());
    }

    #[test]
    fn calculate_next_run_time_returns_future_time() {
        let next = calculate_next_run_time(&[(0, 0)]).unwrap();
        assert!(next > chrono::Local::now());
    }

    #[test]
    fn auto_update_sort_key_accepts_all_current_sort_columns() {
        for (index, key) in SORT_COLUMN_KEYS.iter().enumerate() {
            let server_setting: serde_yaml::Value = serde_yaml::from_str(&format!(
                "current_sort:\n  column: \"{}\"\n  dir: asc\n",
                index
            ))
            .unwrap();
            assert_eq!(auto_update_sort_key_from_value(&server_setting), Some(*key));
        }
    }

    #[test]
    fn missed_run_time_triggers_single_catch_up_after_downtime() {
        let last_run = Local.with_ymd_and_hms(2026, 4, 20, 8, 0, 0).single().unwrap();
        let now = Local.with_ymd_and_hms(2026, 4, 20, 12, 0, 0).single().unwrap();
        let missed = missed_run_time(&[(9, 0), (18, 0)], Some(last_run), now).unwrap();

        assert_eq!(missed.hour(), 9);
        assert_eq!(missed.minute(), 0);
    }

    #[test]
    fn local_datetime_in_timezone_rounds_dst_gap_forward() {
        let rounded = local_datetime_in_timezone(&New_York, 2024, 3, 10, 2, 30).unwrap();

        assert_eq!(rounded.hour(), 3);
        assert_eq!(rounded.minute(), 0);
    }

    #[test]
    fn calculate_next_run_time_after_uses_rounded_dst_gap() {
        let before_gap = New_York
            .with_ymd_and_hms(2024, 3, 10, 1, 45, 0)
            .single()
            .unwrap();
        let next = calculate_next_run_time_after(&[(2, 30)], before_gap).unwrap();

        assert_eq!(next.hour(), 3);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn queue_auto_update_job_reuses_pending_job() {
        let temp = tempfile::tempdir().unwrap();
        let queue = PersistentQueue::new(&temp.path().join("queue.yaml")).unwrap();
        let running_jobs = Mutex::new(Vec::new());

        let (first_id, first_queued) =
            queue_auto_update_job_if_needed(&queue, &running_jobs).unwrap();
        let (second_id, second_queued) =
            queue_auto_update_job_if_needed(&queue, &running_jobs).unwrap();

        assert!(first_queued);
        assert!(!second_queued);
        assert_eq!(first_id, second_id);
        assert_eq!(queue.pending_count(), 1);
    }

    #[test]
    fn queue_auto_update_job_reuses_running_job() {
        let temp = tempfile::tempdir().unwrap();
        let queue = PersistentQueue::new(&temp.path().join("queue.yaml")).unwrap();
        let running_jobs = Mutex::new(vec![QueueJob {
            id: "running-auto".to_string(),
            job_type: JobType::AutoUpdate,
            target: String::new(),
            created_at: 0,
            retry_count: 0,
            max_retries: 3,
        }]);

        let (job_id, queued) = queue_auto_update_job_if_needed(&queue, &running_jobs).unwrap();

        assert!(!queued);
        assert_eq!(job_id, "running-auto");
        assert_eq!(queue.pending_count(), 0);
    }

    #[test]
    fn auto_update_target_split_skips_frozen_records() {
        let records = vec![
            sample_record(1, "https://example.com/1", &[]),
            sample_record(2, "https://example.com/2", &[]),
            sample_record(3, "https://example.com/3", &["frozen"]),
            sample_record(4, "https://example.com/4", &[]),
            sample_record(5, "https://example.com/5", &[]),
        ];
        let modified_ids = BTreeSet::from([1, 3]);
        let frozen_ids = HashSet::from([4]);

        let (modified, other) =
            split_auto_update_target_ids(records.iter(), &modified_ids, &frozen_ids, |record| {
                record.id == 2
            });

        assert_eq!(modified, vec!["1".to_string()]);
        assert_eq!(other, vec!["5".to_string()]);
    }
}
