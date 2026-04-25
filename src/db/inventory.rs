use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use fs2::FileExt;
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{NarouError, Result};

const CACHE_MAX_SIZE: usize = 200;
const CACHE_TARGET_SIZE: usize = 160;
pub(crate) const MAX_YAML_SIZE_BYTES: u64 = 32 * 1024 * 1024;

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
}

pub struct Inventory {
    root_dir: PathBuf,
    cache: Mutex<InventoryCache>,
}

struct InventoryCache {
    entries: HashMap<String, CacheEntry>,
    access_order: Vec<String>,
}

impl InventoryCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            access_order: Vec::new(),
        }
    }

    fn touch(&mut self, name: &str) {
        if self.entries.contains_key(name) {
            self.access_order.retain(|k| k != name);
            self.access_order.push(name.to_string());
        }
    }

    fn remember(&mut self, name: &str, data: String) {
        if !self.entries.contains_key(name) && self.entries.len() >= CACHE_MAX_SIZE {
            self.evict_if_needed();
        }
        self.entries.insert(name.to_string(), CacheEntry { data });
        self.touch(name);
    }

    fn evict_if_needed(&mut self) {
        if self.entries.len() >= CACHE_MAX_SIZE {
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
        {
            let mut cache = self.cache.lock();
            if let Some(entry) = cache.entries.get(name) {
                let data = entry.data.clone();
                cache.touch(name);
                return Ok(data);
            }
        }

        let path = self.inventory_path(name, scope);
        let content = if path.exists() {
            ensure_yaml_size_limit(&path)?;
            fs::read_to_string(&path)?
        } else {
            String::new()
        };

        self.cache.lock().remember(name, content.clone());
        Ok(content)
    }

    pub fn save_raw(&self, name: &str, scope: InventoryScope, content: &str) -> Result<()> {
        let path = self.inventory_path(name, scope);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, content)?;
        self.cache.lock().remember(name, content.to_string());
        Ok(())
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
        let content = serialize_yaml_content(data)?;
        self.save_raw(name, scope, &content)
    }

    pub fn update_yaml<T, D, F>(&self, name: &str, scope: InventoryScope, update: F) -> Result<T>
    where
        D: DeserializeOwned + Default + Serialize,
        F: FnOnce(D) -> Result<(D, T)>,
    {
        let path = self.inventory_path(name, scope);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let (content, result) = update_locked_yaml_file::<T, D, _>(&path, update)?;
        self.cache.lock().remember(name, content);
        Ok(result)
    }

    pub fn clear_cache(&self) {
        let mut cache = self.cache.lock();
        cache.entries.clear();
        cache.access_order.clear();
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

static PROCESS_WRITE_LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Arc<StdMutex<()>>>>> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
pub enum InventoryScope {
    Local,
    Global,
}

pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    with_exclusive_file_lock(path, || atomic_write_locked(path, content))
}

pub fn update_locked_yaml_file<T, D, F>(path: &Path, update: F) -> Result<(String, T)>
where
    D: DeserializeOwned + Default + Serialize,
    F: FnOnce(D) -> Result<(D, T)>,
{
    with_locked_file_update(path, |raw| {
        let current = if raw.is_empty() {
            D::default()
        } else {
            serde_yaml::from_str(&raw)?
        };
        let (updated, result) = update(current)?;
        let content = serialize_yaml_content(&updated)?;
        Ok((content, result))
    })
}

pub(crate) fn with_locked_file_update<T, F>(path: &Path, update: F) -> Result<(String, T)>
where
    F: FnOnce(String) -> Result<(String, T)>,
{
    with_exclusive_file_lock(path, || {
        let current = if path.exists() {
            ensure_yaml_size_limit(path)?;
            fs::read_to_string(path)?
        } else {
            String::new()
        };
        let (new_content, result) = update(current)?;
        atomic_write_locked(path, &new_content)?;
        Ok((new_content, result))
    })
}

fn with_exclusive_file_lock<T, F>(path: &Path, operation: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let process_lock = process_write_lock_for(path);
    let _process_guard = process_lock.lock().unwrap_or_else(|e| e.into_inner());
    let lock_path = lock_file_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)?;
    lock_file.lock_exclusive()?;
    operation()
}

fn atomic_write_locked(path: &Path, content: &str) -> Result<()> {
    let retries = 20u32;
    let mut last_error = None;

    for attempt in 0..retries {
        let (mut file, tmp_path) = temporary_write_file(path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);

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

fn serialize_yaml_content<T: Serialize>(data: &T) -> Result<String> {
    let mut content = serde_yaml::to_string(data)?;
    // Strip the `---` document-start header that serde_yaml emits by default,
    // to match Ruby Psych output and keep files byte-compatible with narou.rb.
    if content.starts_with("---\n") {
        content.drain(..4);
    } else if content.starts_with("---") {
        // Handle `---` without trailing newline (unlikely but safe)
        let after = content[3..].trim_start_matches('\r').trim_start_matches('\n');
        content = after.to_string();
    }
    Ok(content)
}

pub(crate) fn ensure_yaml_size_limit(path: &Path) -> Result<()> {
    let size = fs::metadata(path)?.len();
    if size > MAX_YAML_SIZE_BYTES {
        return Err(NarouError::Database(format!(
            "{} exceeds maximum supported YAML size ({} bytes)",
            path.display(),
            MAX_YAML_SIZE_BYTES
        )));
    }
    Ok(())
}

fn temporary_write_file(path: &Path) -> Result<(fs::File, PathBuf)> {
    let filename = path.file_name().and_then(|name| name.to_str()).unwrap_or("inventory");
    let tempfile = tempfile::Builder::new()
        .prefix(&format!(".{filename}."))
        .suffix(".tmp")
        .rand_bytes(16)
        .tempfile_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    tempfile.keep().map_err(|e| NarouError::Io(e.error))
}

fn lock_file_path(path: &Path) -> PathBuf {
    let filename = path.file_name().and_then(|name| name.to_str()).unwrap_or("inventory");
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{filename}.lock"))
}

fn process_write_lock_for(path: &Path) -> Arc<StdMutex<()>> {
    let key = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut locks = PROCESS_WRITE_LOCKS
        .get_or_init(|| StdMutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    locks
        .entry(key)
        .or_insert_with(|| Arc::new(StdMutex::new(())))
        .clone()
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        CACHE_MAX_SIZE, CACHE_TARGET_SIZE, Inventory, InventoryScope, MAX_YAML_SIZE_BYTES,
    };

    #[test]
    fn load_raw_uses_cache_until_unload() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let narou_dir = root.join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        let path = narou_dir.join("local_setting.yaml");
        std::fs::write(&path, "foo: 1\n").unwrap();

        let inventory = Inventory::new(root);
        assert_eq!(
            inventory
                .load_raw("local_setting", InventoryScope::Local)
                .unwrap(),
            "foo: 1\n"
        );

        std::fs::write(&path, "foo: 2\n").unwrap();
        assert_eq!(
            inventory
                .load_raw("local_setting", InventoryScope::Local)
                .unwrap(),
            "foo: 1\n"
        );

        inventory.unload("local_setting");
        assert_eq!(
            inventory
                .load_raw("local_setting", InventoryScope::Local)
                .unwrap(),
            "foo: 2\n"
        );
    }

    #[test]
    fn eviction_keeps_protected_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".narou")).unwrap();
        let inventory = Inventory::new(root);

        inventory
            .load_raw("local_setting", InventoryScope::Local)
            .unwrap();
        for index in 0..CACHE_MAX_SIZE {
            inventory
                .load_raw(&format!("cache-{index}"), InventoryScope::Local)
                .unwrap();
        }

        let cache = inventory.cache.lock();
        assert_eq!(cache.entries.len(), CACHE_TARGET_SIZE + 1);
        assert!(cache.entries.contains_key("local_setting"));
    }

    #[test]
    fn load_raw_rejects_yaml_larger_than_32mb() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let narou_dir = root.join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        let path = narou_dir.join("local_setting.yaml");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_YAML_SIZE_BYTES + 1).unwrap();

        let inventory = Inventory::new(root);
        let err = inventory
            .load_raw("local_setting", InventoryScope::Local)
            .unwrap_err();

        assert!(err.to_string().contains("maximum supported YAML size"));
    }

    #[test]
    fn update_yaml_performs_read_modify_write_under_same_helper() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let narou_dir = root.join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        std::fs::write(narou_dir.join("freeze.yaml"), "1: true\n").unwrap();

        let inventory = Inventory::new(root.clone());
        inventory
            .update_yaml::<(), HashMap<i64, serde_yaml::Value>, _>(
                "freeze",
                InventoryScope::Local,
                |mut frozen| {
                    frozen.insert(2, serde_yaml::Value::Bool(true));
                    frozen.remove(&1);
                    Ok((frozen, ()))
                },
            )
            .unwrap();

        let raw = std::fs::read_to_string(narou_dir.join("freeze.yaml")).unwrap();
        assert!(!raw.contains("1:"));
        assert!(raw.contains("2: true"));
        assert!(narou_dir.join("freeze.yaml.lock").exists());
    }
}
