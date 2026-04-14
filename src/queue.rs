use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::error::{NarouError, Result};

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
    Convert,
    Send,
    Backup,
    Mail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueState {
    pub jobs: VecDeque<QueueJob>,
    pub completed: Vec<String>,
    pub failed: Vec<String>,
}

impl Default for QueueState {
    fn default() -> Self {
        Self {
            jobs: VecDeque::new(),
            completed: Vec::new(),
            failed: Vec::new(),
        }
    }
}

pub struct PersistentQueue {
    path: PathBuf,
    state: Mutex<QueueState>,
}

impl PersistentQueue {
    pub fn new(path: &Path) -> Result<Self> {
        let mut queue = Self {
            path: path.to_path_buf(),
            state: Mutex::new(QueueState::default()),
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
            let state: QueueState = serde_yaml::from_str(&content)?;
            *self.state.lock() = state;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_yaml::to_string(&*self.state.lock())?;
        fs::write(&self.path, content)?;
        Ok(())
    }

    pub fn push(&self, job_type: JobType, target: &str) -> Result<String> {
        let id = generate_job_id(job_type, target);
        let job = QueueJob {
            id: id.clone(),
            job_type,
            target: target.to_string(),
            created_at: chrono::Utc::now().timestamp(),
            retry_count: 0,
            max_retries: 3,
        };
        self.state.lock().jobs.push_back(job);
        self.save()?;
        Ok(id)
    }

    pub fn push_batch(&self, jobs: &[(JobType, String)]) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut state = self.state.lock();
        for (job_type, target) in jobs {
            let id = generate_job_id(*job_type, target);
            state.jobs.push_back(QueueJob {
                id: id.clone(),
                job_type: *job_type,
                target: target.clone(),
                created_at: chrono::Utc::now().timestamp(),
                retry_count: 0,
                max_retries: 3,
            });
            ids.push(id);
        }
        drop(state);
        self.save()?;
        Ok(ids)
    }

    pub fn pop(&self) -> Option<QueueJob> {
        let job = {
            let mut state = self.state.lock();
            state.jobs.pop_front()?
        };
        let _ = self.save();
        Some(job)
    }

    pub fn complete(&self, job_id: &str) -> Result<()> {
        {
            let mut state = self.state.lock();
            state.completed.push(job_id.to_string());
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
            state.failed.push(job_id.to_string());
            if state.failed.len() > 1000 {
                let drain_count = state.failed.len() - 500;
                state.failed.drain(..drain_count);
            }
        }
        self.save()
    }

    pub fn requeue_failed(&self) -> Result<usize> {
        let mut state = self.state.lock();
        let failed = std::mem::take(&mut state.failed);
        let count = failed.len();
        for job_id in failed {
            state.jobs.push_back(QueueJob {
                id: job_id,
                job_type: JobType::Update,
                target: String::new(),
                created_at: chrono::Utc::now().timestamp(),
                retry_count: 0,
                max_retries: 3,
            });
        }
        drop(state);
        self.save()?;
        Ok(count)
    }

    pub fn len(&self) -> usize {
        self.state.lock().jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().jobs.is_empty()
    }

    pub fn pending_count(&self) -> usize {
        self.state.lock().jobs.len()
    }

    pub fn completed_count(&self) -> usize {
        self.state.lock().completed.len()
    }

    pub fn failed_count(&self) -> usize {
        self.state.lock().failed.len()
    }

    pub fn snapshot(&self) -> QueueState {
        self.state.lock().clone()
    }

    pub fn clear(&self) -> Result<()> {
        {
            let mut state = self.state.lock();
            state.jobs.clear();
            state.completed.clear();
            state.failed.clear();
        }
        self.save()
    }
}

fn generate_job_id(job_type: JobType, target: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    match job_type {
        JobType::Download => "dl".hash(&mut hasher),
        JobType::Update => "up".hash(&mut hasher),
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
    use super::{JobType, PersistentQueue};

    #[test]
    fn clear_saves_without_relocking_deadlock() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.yaml");
        let queue = PersistentQueue::new(&queue_path).unwrap();
        queue.push(JobType::Download, "1").unwrap();
        queue.complete("done").unwrap();
        queue.fail("failed").unwrap();

        queue.clear().unwrap();

        let reloaded = PersistentQueue::new(&queue_path).unwrap();
        assert_eq!(reloaded.pending_count(), 0);
        assert_eq!(reloaded.completed_count(), 0);
        assert_eq!(reloaded.failed_count(), 0);
    }
}
