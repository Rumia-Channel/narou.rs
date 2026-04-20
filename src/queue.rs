use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use crate::db::inventory::atomic_write;
use crate::error::{NarouError, Result};

const MAX_PENDING_JOBS: usize = 10_000;
const MAX_JOB_TARGET_CHARS: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueJob {
    pub id: String,
    pub job_type: JobType,
    pub target: String,
    pub created_at: i64,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub max_retries: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobType {
    Download,
    Update,
    AutoUpdate,
    Convert,
    Send,
    Backup,
    Mail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueLane {
    Default,
    Secondary,
}

impl JobType {
    pub fn lane(self) -> QueueLane {
        match self {
            JobType::Download | JobType::Update | JobType::AutoUpdate => QueueLane::Default,
            JobType::Convert | JobType::Send | JobType::Backup | JobType::Mail => {
                QueueLane::Secondary
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueueState {
    pub jobs: VecDeque<QueueJob>,
    pub completed: Vec<String>,
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueueExecutionSpec {
    pub cmd: String,
    pub args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct QueueStateFile {
    #[serde(default)]
    jobs: VecDeque<QueueJob>,
    #[serde(default)]
    completed: Vec<String>,
    #[serde(default)]
    failed: Vec<String>,
    #[serde(default)]
    pending: Vec<LegacyQueueTask>,
    #[serde(default)]
    running: Vec<LegacyQueueTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyQueueFile {
    pending: Vec<LegacyQueueTask>,
    running: Vec<LegacyQueueTask>,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyQueueTask {
    id: String,
    cmd: String,
    #[serde(default)]
    args: Vec<Value>,
    #[serde(default)]
    meta: Mapping,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started_at: Option<Value>,
}

#[derive(Debug, Clone)]
struct StoredQueueJob {
    job: QueueJob,
    legacy: LegacyQueueTask,
}

impl StoredQueueJob {
    fn mark_pending(&mut self) {
        self.legacy.status = Some("pending".to_string());
        self.legacy.started_at = None;
    }

    fn mark_running(&mut self) {
        self.legacy.status = Some("running".to_string());
        if self.legacy.started_at.is_none() {
            self.legacy.started_at = Some(Value::String(now_rfc3339()));
        }
    }

    fn execution_spec(&self) -> QueueExecutionSpec {
        QueueExecutionSpec {
            cmd: self.legacy.cmd.clone(),
            args: flatten_values(&self.legacy.args),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PersistentQueueState {
    active_pending: VecDeque<StoredQueueJob>,
    deferred_pending: VecDeque<StoredQueueJob>,
    active_running: Vec<StoredQueueJob>,
    deferred_running: Vec<StoredQueueJob>,
    completed: Vec<String>,
    failed: Vec<String>,
}

#[derive(Debug)]
pub struct PersistentQueue {
    path: PathBuf,
    state: Mutex<PersistentQueueState>,
}

impl PersistentQueue {
    pub fn new(path: &Path) -> Result<Self> {
        let mut queue = Self {
            path: path.to_path_buf(),
            state: Mutex::new(PersistentQueueState::default()),
        };
        queue.load()?;
        Ok(queue)
    }

    pub fn with_default() -> Result<Self> {
        let path = find_narou_root()?.join(".narou").join("queue.yaml");
        Self::new(&path)
    }

    fn load(&mut self) -> Result<()> {
        if self.path.exists() {
            let content = fs::read_to_string(&self.path)?;
            let state = load_queue_state(&content)?;
            validate_queue_state(&state)?;
            *self.state.lock() = state;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_yaml::to_string(&queue_state_to_legacy_file(&self.state.lock()))?;
        atomic_write(&self.path, &content)?;
        Ok(())
    }

    pub fn push(&self, job_type: JobType, target: &str) -> Result<String> {
        self.push_internal(job_type, target, None)
    }

    pub fn push_with_legacy(
        &self,
        job_type: JobType,
        target: &str,
        legacy_cmd: &str,
        legacy_args: Vec<Value>,
        meta: Mapping,
    ) -> Result<String> {
        self.push_internal(
            job_type,
            target,
            Some((legacy_cmd.to_string(), legacy_args, meta)),
        )
    }

    fn push_internal(
        &self,
        job_type: JobType,
        target: &str,
        legacy_override: Option<(String, Vec<Value>, Mapping)>,
    ) -> Result<String> {
        validate_job_target(target)?;
        let id = generate_job_id(job_type, target);
        let created_at = chrono::Utc::now().timestamp();
        {
            let mut state = self.state.lock();
            ensure_queue_capacity(total_pending_len(&state), 1)?;
            state.active_pending.push_back(build_stored_job(
                id.clone(),
                job_type,
                target.to_string(),
                created_at,
                legacy_override,
            ));
        }
        self.save()?;
        Ok(id)
    }

    pub fn push_batch(&self, jobs: &[(JobType, String)]) -> Result<Vec<String>> {
        for (_, target) in jobs {
            validate_job_target(target)?;
        }
        let mut ids = Vec::new();
        let mut state = self.state.lock();
        ensure_queue_capacity(total_pending_len(&state), jobs.len())?;
        for (job_type, target) in jobs {
            let id = generate_job_id(*job_type, target);
            let created_at = chrono::Utc::now().timestamp();
            state.active_pending.push_back(build_stored_job(
                id.clone(),
                *job_type,
                target.clone(),
                created_at,
                None,
            ));
            ids.push(id);
        }
        drop(state);
        self.save()?;
        Ok(ids)
    }

    pub fn pop(&self) -> Option<QueueJob> {
        let job = {
            let mut state = self.state.lock();
            let mut stored = state.active_pending.pop_front()?;
            stored.mark_running();
            let job = stored.job.clone();
            state.active_running.retain(|running| running.job.id != job.id);
            state.active_running.push(stored);
            job
        };
        let _ = self.save();
        Some(job)
    }

    pub fn pop_for_lane(&self, lane: QueueLane) -> Option<QueueJob> {
        let job = {
            let mut state = self.state.lock();
            let index = state
                .active_pending
                .iter()
                .position(|job| job.job.job_type.lane() == lane)?;
            let mut stored = state.active_pending.remove(index)?;
            stored.mark_running();
            let job = stored.job.clone();
            state.active_running.retain(|running| running.job.id != job.id);
            state.active_running.push(stored);
            job
        };
        let _ = self.save();
        Some(job)
    }

    pub fn complete(&self, job_id: &str) -> Result<()> {
        {
            let mut state = self.state.lock();
            remove_running_job(&mut state.active_running, job_id);
            if !state.completed.iter().any(|id| id == job_id) {
                state.completed.push(job_id.to_string());
            }
            if state.completed.len() > 1000 {
                let drain_count = state.completed.len() - 500;
                state.completed.drain(..drain_count);
            }
        }
        self.save()
    }

    pub fn fail(&self, job_id: &str) -> Result<()> {
        {
            let mut state = self.state.lock();
            remove_running_job(&mut state.active_running, job_id);
            if !state.failed.iter().any(|id| id == job_id) {
                state.failed.push(job_id.to_string());
            }
            if state.failed.len() > 1000 {
                let drain_count = state.failed.len() - 500;
                state.failed.drain(..drain_count);
            }
        }
        self.save()
    }

    pub fn requeue_failed(&self) -> Result<usize> {
        let mut state = self.state.lock();
        ensure_queue_capacity(total_pending_len(&state), state.failed.len())?;
        let failed = std::mem::take(&mut state.failed);
        let count = failed.len();
        for job_id in failed {
            let created_at = chrono::Utc::now().timestamp();
            state.active_pending.push_back(build_stored_job(
                job_id,
                JobType::Update,
                String::new(),
                created_at,
                None,
            ));
        }
        drop(state);
        self.save()?;
        Ok(count)
    }

    pub fn len(&self) -> usize {
        self.pending_count()
    }

    pub fn is_empty(&self) -> bool {
        self.pending_count() == 0
    }

    pub fn pending_count(&self) -> usize {
        let state = self.state.lock();
        total_pending_len(&state)
    }

    pub fn active_pending_count(&self) -> usize {
        self.state.lock().active_pending.len()
    }

    pub fn pending_count_for_lane(&self, lane: QueueLane) -> usize {
        let state = self.state.lock();
        state
            .deferred_pending
            .iter()
            .chain(state.active_pending.iter())
            .filter(|job| job.job.job_type.lane() == lane)
            .count()
    }

    pub fn running_count(&self) -> usize {
        let state = self.state.lock();
        state.active_running.len() + state.deferred_running.len()
    }

    pub fn running_count_for_lane(&self, lane: QueueLane) -> usize {
        let state = self.state.lock();
        state
            .deferred_running
            .iter()
            .chain(state.active_running.iter())
            .filter(|job| job.job.job_type.lane() == lane)
            .count()
    }

    pub fn completed_count(&self) -> usize {
        self.state.lock().completed.len()
    }

    pub fn failed_count(&self) -> usize {
        self.state.lock().failed.len()
    }

    pub fn snapshot(&self) -> QueueState {
        let state = self.state.lock();
        QueueState {
            jobs: state
                .deferred_pending
                .iter()
                .chain(state.active_pending.iter())
                .map(|job| job.job.clone())
                .collect(),
            completed: state.completed.clone(),
            failed: state.failed.clone(),
        }
    }

    pub fn get_pending_tasks(&self) -> Vec<QueueJob> {
        let state = self.state.lock();
        state
            .deferred_pending
            .iter()
            .chain(state.active_pending.iter())
            .map(|job| job.job.clone())
            .collect()
    }

    pub fn get_running_tasks(&self) -> Vec<QueueJob> {
        let state = self.state.lock();
        state
            .deferred_running
            .iter()
            .chain(state.active_running.iter())
            .map(|job| job.job.clone())
            .collect()
    }

    pub fn has_restorable_tasks(&self) -> bool {
        let state = self.state.lock();
        !state.deferred_pending.is_empty() || !state.deferred_running.is_empty()
    }

    pub fn remove_pending(&self, task_id: &str) -> Result<bool> {
        let removed = {
            let mut state = self.state.lock();
            remove_pending_job(&mut state.active_pending, task_id)
                || remove_pending_job(&mut state.deferred_pending, task_id)
        };
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn reorder_pending(&self, task_ids: &[String]) -> Result<bool> {
        let reordered = {
            let mut state = self.state.lock();
            let total_len = total_pending_len(&state);
            if task_ids.len() != total_len {
                return Ok(false);
            }

            let current_ids: Vec<String> = state
                .deferred_pending
                .iter()
                .chain(state.active_pending.iter())
                .map(|job| job.job.id.clone())
                .collect();
            let mut expected = current_ids.clone();
            expected.sort();
            let mut requested = task_ids.to_vec();
            requested.sort();
            if requested != expected {
                return Ok(false);
            }

            let active_ids: std::collections::HashSet<String> = state
                .active_pending
                .iter()
                .map(|job| job.job.id.clone())
                .collect();
            let deferred_jobs: Vec<_> = state.deferred_pending.drain(..).collect();
            let active_jobs: Vec<_> = state.active_pending.drain(..).collect();
            let mut all_jobs = deferred_jobs
                .into_iter()
                .chain(active_jobs)
                .map(|job| (job.job.id.clone(), job))
                .collect::<std::collections::HashMap<_, _>>();

            let mut deferred = VecDeque::new();
            let mut active = VecDeque::new();
            for task_id in task_ids {
                if let Some(job) = all_jobs.remove(task_id) {
                    if active_ids.contains(task_id) {
                        active.push_back(job);
                    } else {
                        deferred.push_back(job);
                    }
                }
            }
            state.deferred_pending = deferred;
            state.active_pending = active;
            true
        };
        if reordered {
            self.save()?;
        }
        Ok(reordered)
    }

    pub fn clear_pending(&self) -> Result<()> {
        {
            let mut state = self.state.lock();
            state.active_pending.clear();
            state.deferred_pending.clear();
        }
        self.save()
    }

    pub fn clear(&self) -> Result<()> {
        {
            let mut state = self.state.lock();
            state.active_pending.clear();
            state.deferred_pending.clear();
            state.active_running.clear();
            state.deferred_running.clear();
            state.completed.clear();
            state.failed.clear();
        }
        self.save()
    }

    pub fn activate_restorable_tasks(&self) -> Result<usize> {
        let count = {
            let mut state = self.state.lock();
            let deferred_running = std::mem::take(&mut state.deferred_running);
            let deferred_pending = std::mem::take(&mut state.deferred_pending);
            let count = deferred_running.len() + deferred_pending.len();
            for mut job in deferred_running {
                job.mark_pending();
                state.active_pending.push_back(job);
            }
            for mut job in deferred_pending {
                job.mark_pending();
                state.active_pending.push_back(job);
            }
            count
        };
        self.save()?;
        Ok(count)
    }

    pub fn defer_restorable_tasks(&self) -> Result<usize> {
        let count = {
            let mut state = self.state.lock();
            let deferred_running = std::mem::take(&mut state.deferred_running);
            let count = deferred_running.len();
            for mut job in deferred_running {
                job.mark_pending();
                state.deferred_pending.push_back(job);
            }
            count
        };
        self.save()?;
        Ok(count)
    }

    pub(crate) fn execution_spec(&self, job_id: &str) -> Option<QueueExecutionSpec> {
        let state = self.state.lock();
        state
            .active_running
            .iter()
            .chain(state.deferred_running.iter())
            .chain(state.active_pending.iter())
            .chain(state.deferred_pending.iter())
            .find(|job| job.job.id == job_id)
            .map(StoredQueueJob::execution_spec)
    }
}

fn remove_running_job(jobs: &mut Vec<StoredQueueJob>, job_id: &str) {
    jobs.retain(|job| job.job.id != job_id);
}

fn remove_pending_job(jobs: &mut VecDeque<StoredQueueJob>, job_id: &str) -> bool {
    let before = jobs.len();
    jobs.retain(|job| job.job.id != job_id);
    jobs.len() < before
}

fn total_pending_len(state: &PersistentQueueState) -> usize {
    state.active_pending.len() + state.deferred_pending.len()
}

fn validate_job_target(target: &str) -> Result<()> {
    if target.chars().count() > MAX_JOB_TARGET_CHARS {
        return Err(NarouError::Database(format!(
            "queue target exceeds {} characters",
            MAX_JOB_TARGET_CHARS
        )));
    }
    Ok(())
}

fn ensure_queue_capacity(current_len: usize, incoming: usize) -> Result<()> {
    if incoming == 0 {
        return Ok(());
    }
    let remaining = MAX_PENDING_JOBS.saturating_sub(current_len);
    if incoming > remaining {
        return Err(NarouError::Database(format!(
            "queue exceeds maximum of {} pending jobs",
            MAX_PENDING_JOBS
        )));
    }
    Ok(())
}

fn validate_queue_state(state: &PersistentQueueState) -> Result<()> {
    let pending_len = total_pending_len(state);
    if pending_len > MAX_PENDING_JOBS {
        return Err(NarouError::Database(format!(
            "queue.yaml contains {} pending jobs, exceeding limit {}",
            pending_len, MAX_PENDING_JOBS
        )));
    }
    for job in state
        .deferred_pending
        .iter()
        .chain(state.active_pending.iter())
        .chain(state.deferred_running.iter())
        .chain(state.active_running.iter())
    {
        validate_job_target(&job.job.target)?;
    }
    Ok(())
}

fn load_queue_state(content: &str) -> Result<PersistentQueueState> {
    let file: QueueStateFile = serde_yaml::from_str(content)?;
    let deferred_pending_jobs = file
        .jobs
        .into_iter()
        .map(stored_job_from_queue_job)
        .collect::<VecDeque<_>>();
    let mut deferred_pending = deferred_pending_jobs;
    deferred_pending.extend(
        file.pending
            .into_iter()
            .filter_map(|task| legacy_task_to_stored_job(task, false)),
    );
    let deferred_running = file
        .running
        .into_iter()
        .filter_map(|task| legacy_task_to_stored_job(task, true))
        .collect();
    Ok(PersistentQueueState {
        active_pending: VecDeque::new(),
        deferred_pending,
        active_running: Vec::new(),
        deferred_running,
        completed: file.completed,
        failed: file.failed,
    })
}

fn queue_state_to_legacy_file(state: &PersistentQueueState) -> LegacyQueueFile {
    let pending = state
        .deferred_pending
        .iter()
        .chain(state.active_pending.iter())
        .map(stored_job_to_pending_legacy_task)
        .collect();
    let running = state
        .deferred_running
        .iter()
        .chain(state.active_running.iter())
        .map(stored_job_to_running_legacy_task)
        .collect();
    LegacyQueueFile {
        pending,
        running,
        updated_at: now_rfc3339(),
    }
}

fn stored_job_from_queue_job(job: QueueJob) -> StoredQueueJob {
    let legacy = build_legacy_task(
        job.id.clone(),
        job.job_type,
        job.target.clone(),
        job.created_at,
        None,
    );
    let mut stored = StoredQueueJob { job, legacy };
    stored.mark_pending();
    stored
}

fn legacy_task_to_stored_job(task: LegacyQueueTask, running: bool) -> Option<StoredQueueJob> {
    let (job_type, target) = legacy_cmd_to_job_type_and_target(&task.cmd, &task.args)?;
    let created_at = legacy_timestamp_to_unix(task.created_at.clone());
    let job = QueueJob {
        id: task.id.clone(),
        job_type,
        target,
        created_at,
        retry_count: 0,
        max_retries: 3,
    };
    let mut stored = StoredQueueJob { job, legacy: task };
    if running {
        stored.mark_running();
    } else {
        stored.mark_pending();
    }
    Some(stored)
}

fn stored_job_to_pending_legacy_task(job: &StoredQueueJob) -> LegacyQueueTask {
    let mut legacy = job.legacy.clone();
    legacy.status = Some("pending".to_string());
    legacy.started_at = None;
    legacy
}

fn stored_job_to_running_legacy_task(job: &StoredQueueJob) -> LegacyQueueTask {
    let mut legacy = job.legacy.clone();
    legacy.status = Some("running".to_string());
    if legacy.started_at.is_none() {
        legacy.started_at = Some(Value::String(now_rfc3339()));
    }
    legacy
}

fn build_stored_job(
    id: String,
    job_type: JobType,
    target: String,
    created_at: i64,
    legacy_override: Option<(String, Vec<Value>, Mapping)>,
) -> StoredQueueJob {
    let job = QueueJob {
        id: id.clone(),
        job_type,
        target: target.clone(),
        created_at,
        retry_count: 0,
        max_retries: 3,
    };
    let mut stored = StoredQueueJob {
        job,
        legacy: build_legacy_task(id, job_type, target, created_at, legacy_override),
    };
    stored.mark_pending();
    stored
}

fn build_legacy_task(
    id: String,
    job_type: JobType,
    target: String,
    created_at: i64,
    legacy_override: Option<(String, Vec<Value>, Mapping)>,
) -> LegacyQueueTask {
    let (cmd, args, meta) = match legacy_override {
        Some((cmd, args, meta)) => (cmd, args, meta),
        None => queue_job_to_legacy_parts(job_type, &target),
    };
    LegacyQueueTask {
        id,
        cmd,
        args,
        meta,
        status: Some("pending".to_string()),
        created_at: Some(Value::String(unix_to_rfc3339(created_at))),
        started_at: None,
    }
}

fn legacy_cmd_to_job_type_and_target(cmd: &str, args: &[Value]) -> Option<(JobType, String)> {
    let flattened = flatten_values(args);
    let target = flattened.join("\t");
    let job_type = match cmd {
        "download" | "download_force" => JobType::Download,
        "update"
        | "update_by_tag"
        | "update_general_lastup"
        | "freeze"
        | "remove"
        | "inspect"
        | "diff"
        | "diff_clean"
        | "setting_burn" => JobType::Update,
        "convert" => JobType::Convert,
        "send" | "backup_bookmark" | "eject" => JobType::Send,
        "backup" => JobType::Backup,
        "mail" => JobType::Mail,
        "auto_update" => JobType::AutoUpdate,
        other => {
            eprintln!(
                "Warning: preserving but not executing unknown legacy queue task '{}'",
                other
            );
            JobType::Update
        }
    };
    Some((job_type, target))
}

fn queue_job_to_legacy_parts(job_type: JobType, target: &str) -> (String, Vec<Value>, Mapping) {
    let parts = split_job_target(target);
    let (cmd, args) = match job_type {
        JobType::Download => {
            if parts.first() == Some(&"--force") && !parts.iter().any(|part| *part == "--mail") {
                (
                    "download_force".to_string(),
                    parts[1..]
                        .iter()
                        .map(|part| Value::String((*part).to_string()))
                        .collect(),
                )
            } else {
                (
                    "download".to_string(),
                    parts.into_iter()
                        .map(|part| Value::String(part.to_string()))
                        .collect(),
                )
            }
        }
        JobType::Update => (
            "update".to_string(),
            parts.into_iter()
                .map(|part| Value::String(part.to_string()))
                .collect(),
        ),
        JobType::Convert => (
            "convert".to_string(),
            parts.into_iter()
                .map(|part| Value::String(part.to_string()))
                .collect(),
        ),
        JobType::Send => {
            if target == "--backup-bookmark" {
                ("backup_bookmark".to_string(), Vec::new())
            } else {
                (
                    "send".to_string(),
                    parts.into_iter()
                        .map(|part| Value::String(part.to_string()))
                        .collect(),
                )
            }
        }
        JobType::Backup => (
            "backup".to_string(),
            parts.into_iter()
                .map(|part| Value::String(part.to_string()))
                .collect(),
        ),
        JobType::Mail => (
            "mail".to_string(),
            parts.into_iter()
                .map(|part| Value::String(part.to_string()))
                .collect(),
        ),
        JobType::AutoUpdate => ("auto_update".to_string(), Vec::new()),
    };
    (cmd, args, Mapping::new())
}

fn split_job_target(target: &str) -> Vec<&str> {
    target.split('\t').filter(|part| !part.is_empty()).collect()
}

fn flatten_values(values: &[Value]) -> Vec<String> {
    let mut flattened = Vec::new();
    for value in values {
        flatten_value_into(value, &mut flattened);
    }
    flattened
}

fn flatten_value_into(value: &Value, flattened: &mut Vec<String>) {
    match value {
        Value::String(s) if !s.is_empty() => flattened.push(s.clone()),
        Value::Number(n) => flattened.push(n.to_string()),
        Value::Bool(b) => flattened.push(b.to_string()),
        Value::Sequence(items) => {
            for item in items {
                flatten_value_into(item, flattened);
            }
        }
        Value::Null => {}
        other => {
            let text = serde_yaml::to_string(other).unwrap_or_default();
            let text = text.trim();
            if !text.is_empty() {
                flattened.push(text.to_string());
            }
        }
    }
}

fn legacy_timestamp_to_unix(value: Option<Value>) -> i64 {
    match value {
        Some(Value::String(s)) => chrono::DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.timestamp())
            .unwrap_or_else(|_| chrono::Utc::now().timestamp()),
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| chrono::Utc::now().timestamp()),
        _ => chrono::Utc::now().timestamp(),
    }
}

fn unix_to_rfc3339(timestamp: i64) -> String {
    use chrono::TimeZone;

    chrono::Utc
        .timestamp_opt(timestamp, 0)
        .single()
        .unwrap_or_else(chrono::Utc::now)
        .with_timezone(&chrono::Local)
        .to_rfc3339()
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

fn generate_job_id(job_type: JobType, target: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    match job_type {
        JobType::Download => "dl".hash(&mut hasher),
        JobType::Update => "up".hash(&mut hasher),
        JobType::AutoUpdate => "au".hash(&mut hasher),
        JobType::Convert => "cv".hash(&mut hasher),
        JobType::Send => "sd".hash(&mut hasher),
        JobType::Backup => "bk".hash(&mut hasher),
        JobType::Mail => "ml".hash(&mut hasher),
    }
    target.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    chrono::Utc::now().timestamp_millis().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn find_narou_root() -> Result<PathBuf> {
    let mut current = std::env::current_dir()?;
    loop {
        if current.join(".narou").exists() {
            return Ok(current);
        }
        if !current.pop() {
            return Err(NarouError::Database(
                ".narou directory not found".to_string(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_yaml::{Mapping, Value};

    use super::{JobType, PersistentQueue, QueueLane};

    #[test]
    fn clear_saves_without_relocking_deadlock() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.yaml");
        let queue = PersistentQueue::new(&queue_path).unwrap();
        queue.push(JobType::Download, "1").unwrap();
        let job = queue.pop().unwrap();
        queue.complete(&job.id).unwrap();
        queue.fail("failed").unwrap();

        queue.clear().unwrap();

        let reloaded = PersistentQueue::new(&queue_path).unwrap();
        assert_eq!(reloaded.pending_count(), 0);
        assert_eq!(reloaded.running_count(), 0);
        assert_eq!(reloaded.completed_count(), 0);
        assert_eq!(reloaded.failed_count(), 0);
    }

    #[test]
    fn pop_for_lane_removes_first_matching_lane_job() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.yaml");
        let queue = PersistentQueue::new(&queue_path).unwrap();
        queue.push(JobType::Download, "1").unwrap();
        queue.push(JobType::Backup, "2").unwrap();
        queue.push(JobType::Update, "3").unwrap();

        let popped = queue.pop_for_lane(QueueLane::Secondary).unwrap();
        assert!(matches!(popped.job_type, JobType::Backup));
        assert_eq!(queue.pending_count_for_lane(QueueLane::Secondary), 0);
        assert_eq!(queue.running_count_for_lane(QueueLane::Secondary), 1);

        let remaining = queue.get_pending_tasks();
        assert_eq!(remaining.len(), 2);
        assert!(matches!(remaining[0].job_type, JobType::Download));
        assert!(matches!(remaining[1].job_type, JobType::Update));
    }

    #[test]
    fn push_rejects_oversized_targets() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.yaml");
        let queue = PersistentQueue::new(&queue_path).unwrap();

        let err = queue
            .push(JobType::Download, &"a".repeat(16 * 1024 + 1))
            .unwrap_err();

        assert!(err.to_string().contains("queue target exceeds"));
        assert_eq!(queue.pending_count(), 0);
    }

    #[test]
    fn load_rejects_tampered_queue_with_too_many_jobs() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.yaml");
        let mut jobs = Vec::new();
        for index in 0..10_001 {
            jobs.push(format!(
                "- id: job-{index}\n  job_type: download\n  target: target-{index}\n  created_at: 0\n  retry_count: 0\n  max_retries: 3"
            ));
        }
        let yaml = format!(
            "jobs:\n{}\ncompleted: []\nfailed: []\n",
            jobs.join("\n")
        );
        std::fs::write(&queue_path, yaml).unwrap();

        let err = PersistentQueue::new(&queue_path).unwrap_err();

        assert!(err.to_string().contains("exceeding limit"));
    }

    #[test]
    fn startup_tasks_stay_deferred_until_explicit_restore() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.yaml");
        std::fs::write(
            &queue_path,
            "---\npending:\n  - id: task-1\n    cmd: download_force\n    args:\n      - n0001\n    meta: {}\n    status: pending\n    created_at: '2026-04-19T15:13:58+09:00'\nrunning:\n  - id: task-2\n    cmd: auto_update\n    args: []\n    meta: {}\n    status: running\n    created_at: '2026-04-19T15:14:58+09:00'\n    started_at: '2026-04-19T15:15:58+09:00'\nupdated_at: '2026-04-19T15:16:58+09:00'\n",
        )
        .unwrap();

        let queue = PersistentQueue::new(&queue_path).unwrap();
        assert!(queue.has_restorable_tasks());
        assert_eq!(queue.pending_count(), 1);
        assert_eq!(queue.running_count(), 1);
        assert!(queue.pop().is_none());

        let activated = queue.activate_restorable_tasks().unwrap();
        assert_eq!(activated, 2);
        assert!(!queue.has_restorable_tasks());
        assert_eq!(queue.pending_count(), 2);
        assert_eq!(queue.running_count(), 0);

        let first = queue.pop().unwrap();
        assert_eq!(first.id, "task-2");
        let second = queue.pop().unwrap();
        assert_eq!(second.id, "task-1");
    }

    #[test]
    fn defer_restorable_tasks_requeues_running_on_disk() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.yaml");
        std::fs::write(
            &queue_path,
            "---\npending:\n  - id: pending-1\n    cmd: update_by_tag\n    args:\n      - tag:modified\n    meta: {source: web}\n    status: pending\n    created_at: '2026-04-19T15:13:58+09:00'\nrunning:\n  - id: running-1\n    cmd: freeze\n    args:\n      - --on\n      - - '12'\n        - '34'\n    meta: {source: restore}\n    status: running\n    created_at: '2026-04-19T15:14:58+09:00'\n    started_at: '2026-04-19T15:15:58+09:00'\nupdated_at: '2026-04-19T15:16:58+09:00'\n",
        )
        .unwrap();

        let queue = PersistentQueue::new(&queue_path).unwrap();
        assert_eq!(queue.defer_restorable_tasks().unwrap(), 1);
        assert!(queue.has_restorable_tasks());
        assert_eq!(queue.pending_count(), 2);
        assert_eq!(queue.running_count(), 0);

        let saved = std::fs::read_to_string(&queue_path).unwrap();
        assert!(saved.contains("cmd: update_by_tag"));
        assert!(saved.contains("cmd: freeze"));
        assert!(saved.contains("status: pending"));
        assert!(!saved.contains("started_at:"));
        assert!(saved.contains("source: restore"));
    }

    #[test]
    fn save_preserves_running_section_and_legacy_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.yaml");
        let queue = PersistentQueue::new(&queue_path).unwrap();
        let mut meta = Mapping::new();
        meta.insert(
            Value::String("source".to_string()),
            Value::String("ruby".to_string()),
        );
        let id = queue
            .push_with_legacy(
                JobType::Update,
                "tag:modified",
                "update_by_tag",
                vec![Value::String("tag:modified".to_string())],
                meta,
            )
            .unwrap();

        let popped = queue.pop().unwrap();
        assert_eq!(popped.id, id);

        let saved = std::fs::read_to_string(&queue_path).unwrap();

        assert!(saved.contains("pending: []"));
        assert!(saved.contains("running:"));
        assert!(saved.contains("cmd: update_by_tag"));
        assert!(saved.contains("source: ruby"));
        assert!(saved.contains("status: running"));
        assert!(saved.contains("started_at:"));
    }

    #[test]
    fn load_preserves_supported_legacy_commands_and_nested_args() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.yaml");
        std::fs::write(
            &queue_path,
            "---\npending:\n  - id: task-freeze\n    cmd: freeze\n    args:\n      - --off\n      - - '12'\n        - '34'\n    meta:\n      source: ruby\n    status: pending\n    created_at: '2026-04-19T15:13:58+09:00'\nrunning:\n  - id: task-burn\n    cmd: setting_burn\n    args:\n      - - '56'\n        - '78'\n    meta:\n      source: ruby\n    status: running\n    created_at: '2026-04-19T15:14:58+09:00'\n    started_at: '2026-04-19T15:15:58+09:00'\nupdated_at: '2026-04-19T15:16:58+09:00'\n",
        )
        .unwrap();

        let queue = PersistentQueue::new(&queue_path).unwrap();
        let pending = queue.get_pending_tasks();
        let running = queue.get_running_tasks();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target, "--off\t12\t34");
        assert!(matches!(pending[0].job_type, JobType::Update));
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].target, "56\t78");

        let saved = std::fs::read_to_string(&queue_path).unwrap();
        assert!(saved.contains("cmd: freeze"));
        assert!(saved.contains("cmd: setting_burn"));
        assert!(saved.contains("source: ruby"));
        assert!(saved.contains("- - '12'"));
        assert!(saved.contains("- - '56'"));
    }
}
