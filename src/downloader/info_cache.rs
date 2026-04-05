use std::collections::HashMap;
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::error::Result;

const MAX_CACHE_ENTRIES: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedNovelInfo {
    pub title: String,
    pub author: String,
    pub story: Option<String>,
    pub novel_type: Option<u8>,
    pub end: bool,
    pub general_firstup: Option<String>,
    pub general_lastup: Option<String>,
    pub novelupdated_at: Option<String>,
    pub length: Option<i64>,
    pub tags: Option<String>,
    pub cached_at: i64,
}

pub struct NovelInfoCache {
    cache: Mutex<HashMap<String, CachedNovelInfo>>,
    cache_dir: PathBuf,
}

impl NovelInfoCache {
    pub fn new(cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        Self {
            cache: Mutex::new(HashMap::new()),
            cache_dir,
        }
    }

    pub fn with_default() -> Result<Self> {
        let cache_dir = find_narou_root()?.join(".narou").join("cache");
        Ok(Self::new(cache_dir))
    }

    pub fn get(&self, key: &str) -> Option<CachedNovelInfo> {
        let cache = self.cache.lock();
        cache.get(key).cloned()
    }

    pub fn insert(&self, key: &str, info: CachedNovelInfo) {
        let mut cache = self.cache.lock();
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.clear();
            let _ = self.load_from_disk(&mut cache);
        }
        cache.insert(key.to_string(), info);
        let _ = self.save_to_disk(&cache);
    }

    pub fn invalidate(&self, key: &str) {
        let mut cache = self.cache.lock();
        cache.remove(key);
        let path = self.cache_dir.join(format!("info_{}.yaml", key));
        let _ = std::fs::remove_file(path);
    }

    pub fn clear(&self) {
        let mut cache = self.cache.lock();
        cache.clear();
        let _ = std::fs::remove_dir_all(&self.cache_dir);
        let _ = std::fs::create_dir_all(&self.cache_dir);
    }

    fn load_from_disk(&self, cache: &mut HashMap<String, CachedNovelInfo>) {
        if !self.cache_dir.exists() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(info) = serde_yaml::from_str::<CachedNovelInfo>(&content) {
                            if let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) {
                                if let Some(key) = file_stem.strip_prefix("info_") {
                                    cache.insert(key.to_string(), info);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn save_to_disk(&self, cache: &HashMap<String, CachedNovelInfo>) -> Result<()> {
        for (key, info) in cache {
            let filename = format!("info_{}.yaml", key);
            let path = self.cache_dir.join(filename);
            let content = serde_yaml::to_string(info)?;
            std::fs::write(path, content)?;
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.cache.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.lock().is_empty()
    }
}

fn find_narou_root() -> Result<PathBuf> {
    let mut current = std::env::current_dir()?;
    loop {
        if current.join(".narou").exists() {
            return Ok(current);
        }
        if !current.pop() {
            return Err(crate::error::NarouError::Database(
                ".narou directory not found".to_string(),
            ));
        }
    }
}
