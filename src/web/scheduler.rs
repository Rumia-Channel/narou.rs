use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, Local, LocalResult, TimeZone};
use serde_yaml::Value;
use tokio::task::JoinHandle;

use crate::compat::{load_local_setting_bool, load_local_setting_string};
use crate::termcolor::colored;
use crate::db;
use crate::db::inventory::{Inventory, InventoryScope};
use crate::downloader::site_setting::SiteSetting;
use crate::queue::{JobType, PersistentQueue, QueueJob};

use super::push::PushServer;

const AUTO_UPDATE_SORT_COLUMNS: &[&str] = &["id", "last_update", "general_lastup", "last_check_date"];
const SORT_COLUMN_KEYS: &[&str] = &[
    "id",
    "last_update",
    "general_lastup",
    "last_check_date",
    "title",
    "author",
    "sitename",
    "novel_type",
    "tags",
    "general_all_no",
    "length",
    "status",
    "toc_url",
];

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
        eprintln!("自動アップデートスケジューラーの時刻指定が不正です: {}", schedule_string);
        push_server.broadcast_echo(
            &format!("自動アップデートスケジューラーの時刻指定が不正です: {}", schedule_string),
            "stdout",
        );
        return None;
    }

    Some(tokio::spawn(async move {
        loop {
            let Some(next_run) = calculate_next_run_time(&times) else {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                continue;
            };

            sleep_until(next_run).await;
            push_server.broadcast_echo(
                &format!("自動アップデートが予定されています: {}", next_run.format("%Y/%m/%d %H:%M:%S")),
                "stdout",
            );

            match queue_auto_update_job_if_needed(queue.as_ref(), &running_jobs) {
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
    }))
}

pub fn restart_auto_update_scheduler(
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

pub fn stop_auto_update_scheduler(
    scheduler_task: &parking_lot::Mutex<Option<JoinHandle<()>>>,
) {
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
    let now = Local::now();

    for &(hour, minute) in times {
        let candidate = local_datetime(now.year(), now.month(), now.day(), hour, minute)?;
        if candidate > now {
            return Some(candidate);
        }
    }

    let tomorrow = now.date_naive().succ_opt()?;
    let (hour, minute) = *times.first()?;
    local_datetime(
        tomorrow.year(),
        tomorrow.month(),
        tomorrow.day(),
        hour,
        minute,
    )
}

fn local_datetime(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Option<chrono::DateTime<Local>> {
    match Local.with_ymd_and_hms(year, month, day, hour, minute, 0) {
        LocalResult::Single(value) => Some(value),
        LocalResult::Ambiguous(first, _) => Some(first),
        LocalResult::None => None,
    }
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

pub fn execute_auto_update(root_dir: &Path, push_server: &PushServer) -> bool {
    println!(
        "自動アップデートを実行中... ({})",
        Local::now().format("%Y/%m/%d %H:%M:%S")
    );
    push_server.broadcast_echo("自動アップデートを開始します", "stdout");

    let sort_args = build_auto_update_sort_args();
    if !run_update_phase(root_dir, &["--gl", "narou"], "なろうAPIによる更新確認") {
        push_server.broadcast_echo("自動アップデート失敗: なろうAPI更新確認", "stdout");
        return false;
    }

    let (modified_ids, other_ids) = collect_auto_update_target_ids();

    if modified_ids.is_empty() {
        println!("自動アップデート: modified タグの付いた小説はありません");
    } else {
        push_server.broadcast_echo(
            &colored("modified タグの付いた小説を更新します", "yellow"),
            "stdout",
        );
        println!(
            "自動アップデート: modified タグの付いた小説を更新します ({}件)",
            modified_ids.len()
        );
        let mut args = sort_args.clone();
        args.extend(modified_ids.iter().map(String::as_str));
        if !run_update_phase(root_dir, &args, "modified タグ更新") {
            push_server.broadcast_echo("自動アップデート失敗: modified タグ更新", "stdout");
            return false;
        }
    }

    if other_ids.is_empty() {
        println!("自動アップデート: 通常更新の対象となるその他小説はありません");
    } else {
        println!(
            "自動アップデート: その他小説を通常更新します ({}件)",
            other_ids.len()
        );
        let mut args = sort_args;
        args.extend(other_ids.iter().map(String::as_str));
        if !run_update_phase(root_dir, &args, "その他小説更新") {
            push_server.broadcast_echo("自動アップデート失敗: その他小説更新", "stdout");
            return false;
        }
    }

    println!("自動アップデートが正常に完了しました");
    push_server.broadcast_echo("自動アップデートが正常に完了しました", "stdout");
    push_server.broadcast_event("table.reload", "");
    push_server.broadcast_event("tag.updateCanvas", "");
    true
}

fn build_auto_update_sort_args() -> Vec<&'static str> {
    let Some(sort_key) = read_auto_update_sort_key() else {
        println!("自動アップデート: デフォルトソート順序で実行");
        return Vec::new();
    };

    println!("自動アップデート: WebUIソート設定を適用 ({})", sort_key);
    match sort_key {
        "id" => vec!["--sort-by", "id"],
        "last_update" => vec!["--sort-by", "last_update"],
        "general_lastup" => vec!["--sort-by", "general_lastup"],
        "last_check_date" => vec!["--sort-by", "last_check_date"],
        _ => Vec::new(),
    }
}

fn read_auto_update_sort_key() -> Option<&'static str> {
    let inventory = Inventory::with_default_root().ok()?;
    let server_setting: Value = inventory.load("server_setting", InventoryScope::Global).ok()?;
    auto_update_sort_key_from_value(&server_setting)
}

fn auto_update_sort_key_from_value(server_setting: &Value) -> Option<&'static str> {
    let current_sort = server_setting
        .as_mapping()?
        .get(Value::String("current_sort".to_string()))?;
    let current_sort = current_sort.as_mapping()?;
    let column_index = current_sort
        .get(Value::String("column".to_string()))
        .and_then(value_as_usize)
        .or_else(|| {
            current_sort
                .get(Value::String(":column".to_string()))
                .and_then(value_as_usize)
        })?;
    let direction = current_sort
        .get(Value::String("dir".to_string()))
        .and_then(Value::as_str)
        .or_else(|| {
            current_sort
                .get(Value::String(":dir".to_string()))
                .and_then(Value::as_str)
        })?;
    if !matches!(direction, "asc" | "desc") {
        return None;
    }

    let key = *SORT_COLUMN_KEYS.get(column_index)?;
    AUTO_UPDATE_SORT_COLUMNS.contains(&key).then_some(key)
}

fn value_as_usize(value: &Value) -> Option<usize> {
    match value {
        Value::Number(number) => number.as_u64().map(|value| value as usize),
        Value::String(text) => text.parse::<usize>().ok(),
        _ => None,
    }
}

fn collect_auto_update_target_ids() -> (Vec<String>, Vec<String>) {
    let tag_index =
        db::with_database(|db| Ok::<_, crate::error::NarouError>(db.tag_index())).unwrap_or_default();
    let modified_ids: Vec<String> = tag_index
        .get("modified")
        .into_iter()
        .flat_map(|ids| ids.iter().copied())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|id| id.to_string())
        .collect();

    let modified_set: std::collections::HashSet<i64> =
        modified_ids.iter().filter_map(|id| id.parse::<i64>().ok()).collect();
    let site_settings = SiteSetting::load_all().unwrap_or_default();
    let other_ids = db::with_database(|db| {
        Ok::<_, crate::error::NarouError>(
            db.all_records()
                .values()
                .filter(|record| !modified_set.contains(&record.id))
                .filter(|record| {
                    !site_settings
                        .iter()
                        .find(|setting| setting.matches_url(&record.toc_url))
                        .and_then(|setting| setting.narou_api_url.as_ref())
                        .is_some()
                })
                .map(|record| record.id.to_string())
                .collect::<Vec<_>>(),
        )
    })
    .unwrap_or_default();

    (modified_ids, other_ids)
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

fn run_update_phase(root_dir: &Path, args: &[&str], label: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        println!("{} で重大なエラーが発生しました（実行ファイルを取得できません）", label);
        return false;
    };

    let status = std::process::Command::new(exe)
        .current_dir(root_dir)
        .stdin(Stdio::null())
        .arg("update")
        .args(args)
        .status();

    let Ok(status) = status else {
        println!("{} で重大なエラーが発生しました（update を起動できません）", label);
        return false;
    };

    let Some(code) = status.code() else {
        println!("{} で重大なエラーが発生しました（終了コード不明）", label);
        return false;
    };

    match code {
        0 => {
            println!("{} が完了しました", label);
            refresh_database_after_phase(label)
        }
        1..=9 => {
            println!("{} が完了しました（{}件の小説でエラーがありました）", label, code);
            refresh_database_after_phase(label)
        }
        _ => {
            println!("{} で重大なエラーが発生しました（終了コード: {}）", label, code);
            false
        }
    }
}

fn refresh_database_after_phase(label: &str) -> bool {
    match db::with_database_mut(|db| db.refresh()) {
        Ok(()) => true,
        Err(e) => {
            println!("{} 後のDB再読み込みに失敗しました: {}", label, e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        auto_update_sort_key_from_value, calculate_next_run_time, parse_schedule_times,
        queue_auto_update_job_if_needed,
    };
    use parking_lot::Mutex;
    use crate::queue::{JobType, PersistentQueue, QueueJob};

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
    fn auto_update_sort_key_accepts_supported_current_sort() {
        let server_setting: serde_yaml::Value = serde_yaml::from_str(
            "current_sort:\n  column: 3\n  dir: asc\n",
        )
        .unwrap();

        assert_eq!(auto_update_sort_key_from_value(&server_setting), Some("last_check_date"));
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
}
