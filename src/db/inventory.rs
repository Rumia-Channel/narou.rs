use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{NarouError, Result};

const CACHE_MAX_SIZE: usize = 200;
const CACHE_TARGET_SIZE: usize = 160;

const PROTECTED_KEYS: &[&str] = &[
    "local_setting",
    "database",
    "global_setting",
    "latest_convert",
    "database_index",
];

#[derive(Debug)]
struct CacheEntry {
    data: String,
    access_order: usize,
}

pub struct Inventory {
    root_dir: PathBuf,
    cache: Mutex<InventoryCache>,
}

struct InventoryCache {
    entries: HashMap<String, CacheEntry>,
    access_counter: usize,
    access_order: Vec<String>,
}

impl InventoryCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            access_counter: 0,
            access_order: Vec::new(),
        }
    }

    fn touch(&mut self, name: &str) {
        if let Some(entry) = self.entries.get_mut(name) {
            entry.access_order = self.access_counter;
            self.access_counter += 1;
            self.access_order.retain(|k| k != name);
            self.access_order.push(name.to_string());
        }
    }

    fn evict_if_needed(&mut self) {
        if self.entries.len() > CACHE_MAX_SIZE {
            while self.entries.len() > CACHE_TARGET_SIZE {
                if let Some(evict_key) = self.access_order.first() {
                    if PROTECTED_KEYS.contains(&evict_key.as_str()) {
                        if self.access_order.len() <= 1 {
                            break;
                        }
                        self.access_order.rotate_left(1);
                        continue;
                    }
                    let key = evict_key.clone();
                    self.access_order.remove(0);
                    self.entries.remove(&key);
                } else {
                    break;
                }
            }
        }
    }
}

impl Inventory {
    pub fn new(root_dir: PathBuf) -> Self {
        Self {
            root_dir,
            cache: Mutex::new(InventoryCache::new()),
        }
    }

    pub fn with_default_root() -> Result<Self> {
        let root = find_narou_root()?;
        Ok(Self::new(root))
    }

    fn inventory_path(&self, name: &str, scope: InventoryScope) -> PathBuf {
        let dir = match scope {
            InventoryScope::Local => self.root_dir.join(".narou"),
            InventoryScope::Global => {
                let home = dirs_home();
                home.join(".narousetting")
            }
        };
        dir.join(format!("{}.yaml", name))
    }

    pub fn load_raw(&self, name: &str, scope: InventoryScope) -> Result<String> {
        let path = self.inventory_path(name, scope);
        if !path.exists() {
            return Ok(String::new());
        }
        Ok(fs::read_to_string(&path)?)
    }

    pub fn save_raw(&self, name: &str, scope: InventoryScope, content: &str) -> Result<()> {
        let path = self.inventory_path(name, scope);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, content)
    }

    pub fn load<T: DeserializeOwned>(&self, name: &str, scope: InventoryScope) -> Result<T> {
        let raw = self.load_raw(name, scope)?;
        if raw.is_empty() {
            let default: HashMap<String, serde_yaml::Value> = HashMap::new();
            return Ok(serde_yaml::from_value(serde_yaml::to_value(default)?)?);
        }
        Ok(serde_yaml::from_str(&raw)?)
    }

    pub fn save<T: Serialize>(&self, name: &str, scope: InventoryScope, data: &T) -> Result<()> {
        let content = serde_yaml::to_string(data)?;
        self.save_raw(name, scope, &content)
    }

    pub fn clear_cache(&self) {
        let mut cache = self.cache.lock();
        cache.entries.clear();
        cache.access_order.clear();
        cache.access_counter = 0;
    }

    pub fn unload(&self, name: &str) {
        let mut cache = self.cache.lock();
        cache.entries.remove(name);
        cache.access_order.retain(|k| k != name);
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }
}

#[derive(Debug, Clone, Copy)]
pub enum InventoryScope {
    Local,
    Global,
}

pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp_path = temporary_write_path(path);
    let retries = 20u32;
    let mut last_error = None;

    for attempt in 0..retries {
        let _ = fs::remove_file(&tmp_path);
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
        }

        match fs::rename(&tmp_path, path) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_error = Some(e);
                let _ = fs::remove_file(&tmp_path);
                if cfg!(windows) && attempt + 1 < retries {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
                break;
            }
        }
    }

    Err(NarouError::Io(last_error.unwrap_or_else(|| {
        std::io::Error::other(format!("failed to atomically write {}", path.display()))
    })))
}

fn temporary_write_path(path: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let filename = path.file_name().and_then(|name| name.to_str()).unwrap_or("inventory");
    let tmp_name = format!(".{}.{}.{}.tmp", filename, std::process::id(), stamp);
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(tmp_name)
}

fn find_narou_root() -> Result<PathBuf> {
    let mut current = std::env::current_dir()?;
    loop {
        if current.join(".narou").exists() {
            return Ok(current);
        }
        if !current.pop() {
            return Err(NarouError::Database(
                ".narou directory not found in any parent directory".to_string(),
            ));
        }
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/"))
        })
}
