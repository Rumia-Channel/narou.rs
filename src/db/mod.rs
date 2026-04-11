pub mod index_store;
pub mod inventory;
pub mod novel_record;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use index_store::IndexStore;
use inventory::{Inventory, InventoryScope};
use novel_record::NovelRecord;

use crate::error::{NarouError, Result};

const ARCHIVE_ROOT_DIR: &str = "小説データ";
const DATABASE_NAME: &str = "database";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseIndex {
    #[serde(default)]
    pub by_toc_url: HashMap<String, Vec<i64>>,
    #[serde(default)]
    pub by_title: HashMap<String, Vec<i64>>,
    #[serde(default)]
    pub meta: HashMap<i64, index_store::MetaEntry>,
}

pub struct Database {
    data: HashMap<i64, NovelRecord>,
    index: IndexStore,
    inventory: Inventory,
    archive_root: PathBuf,
}

impl Database {
    pub fn new() -> Result<Self> {
        let inventory = Inventory::with_default_root()?;
        let archive_root = inventory.root_dir().join(ARCHIVE_ROOT_DIR);
        std::fs::create_dir_all(&archive_root)?;

        let mut db = Self {
            data: HashMap::new(),
            index: IndexStore::load(&inventory)?,
            inventory,
            archive_root,
        };
        db.refresh()?;
        Ok(db)
    }

    pub fn refresh(&mut self) -> Result<()> {
        let raw = self
            .inventory
            .load_raw(DATABASE_NAME, InventoryScope::Local)?;
        if !raw.is_empty() {
            let loaded: HashMap<i64, NovelRecord> = serde_yaml::from_str(&raw)?;
            self.data = loaded;
        } else {
            self.data.clear();
        }
        self.index.reconcile(&self.data);
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        let content = serde_yaml::to_string(&self.data)?;
        self.inventory
            .save_raw(DATABASE_NAME, InventoryScope::Local, &content)?;
        self.index.flush(&self.inventory)?;
        Ok(())
    }

    pub fn get(&self, id: i64) -> Option<&NovelRecord> {
        self.data.get(&id)
    }

    pub fn insert(&mut self, record: NovelRecord) {
        let id = record.id;
        let toc_url = Some(record.toc_url.clone());
        let title = Some(record.title.clone());
        self.data.insert(id, record);
        self.index.upsert(id, toc_url.as_deref(), title.as_deref());
    }

    pub fn remove(&mut self, id: i64) -> Option<NovelRecord> {
        let record = self.data.remove(&id)?;
        self.index.delete(id);
        Some(record)
    }

    pub fn ids(&self) -> Vec<i64> {
        self.data.keys().copied().collect()
    }

    pub fn all_records(&self) -> &HashMap<i64, NovelRecord> {
        &self.data
    }

    pub fn all_records_mut(&mut self) -> &mut HashMap<i64, NovelRecord> {
        &mut self.data
    }

    pub fn novel_exists(&self, id: i64) -> bool {
        self.data.contains_key(&id)
    }

    pub fn get_by_toc_url(&self, toc_url: &str) -> Option<&NovelRecord> {
        self.index
            .lookup_by_toc_url(toc_url)
            .and_then(|id| self.data.get(&id))
    }

    pub fn get_by_title(&self, title: &str) -> Option<&NovelRecord> {
        self.index
            .lookup_by_title(title)
            .and_then(|id| self.data.get(&id))
    }

    pub fn find_by_title(&self, title: &str) -> Option<&NovelRecord> {
        if let Some(id) = self.index.lookup_by_title(title) {
            return self.data.get(&id);
        }
        let lower = title.to_lowercase();
        for record in self.data.values() {
            if record.title.to_lowercase() == lower {
                return Some(record);
            }
        }
        None
    }

    pub fn create_new_id(&self) -> i64 {
        self.data.keys().copied().max().map(|m| m + 1).unwrap_or(1)
    }

    pub fn sort_by(&self, key: &str, reverse: bool) -> Vec<&NovelRecord> {
        let mut records: Vec<&NovelRecord> = self.data.values().collect();
        records.sort_by(|a, b| {
            let va = match key {
                "id" => Some(format!("{}", a.id)),
                "title" => Some(a.title.to_lowercase()),
                "author" => Some(a.author.to_lowercase()),
                "last_update" => Some(format!("{}", a.last_update.timestamp())),
                "general_lastup" => a.general_lastup.map(|d| format!("{}", d.timestamp())),
                "sitename" => Some(a.sitename.clone()),
                "novel_type" => Some(format!("{}", a.novel_type)),
                "length" => a.length.map(|l| format!("{}", l)),
                _ => None,
            };
            let vb = match key {
                "id" => Some(format!("{}", b.id)),
                "title" => Some(b.title.to_lowercase()),
                "author" => Some(b.author.to_lowercase()),
                "last_update" => Some(format!("{}", b.last_update.timestamp())),
                "general_lastup" => b.general_lastup.map(|d| format!("{}", d.timestamp())),
                "sitename" => Some(b.sitename.clone()),
                "novel_type" => Some(format!("{}", b.novel_type)),
                "length" => b.length.map(|l| format!("{}", l)),
                _ => None,
            };
            match (va, vb) {
                (Some(a), Some(b_val)) => {
                    if reverse {
                        b_val.cmp(&a)
                    } else {
                        a.cmp(&b_val)
                    }
                }
                _ => std::cmp::Ordering::Equal,
            }
        });
        records
    }

    pub fn tag_index(&self) -> HashMap<String, Vec<i64>> {
        let mut index: HashMap<String, Vec<i64>> = HashMap::new();
        for (id, record) in &self.data {
            for tag in &record.tags {
                index.entry(tag.clone()).or_default().push(*id);
            }
        }
        index
    }

    pub fn archive_root(&self) -> &Path {
        &self.archive_root
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }
}

pub fn create_subdirectory_name(file_title: &str) -> String {
    let chars: String = if file_title.starts_with('n') {
        file_title.chars().skip(1).take(2).collect()
    } else {
        file_title.chars().take(2).collect()
    };
    chars.trim().to_string()
}

pub fn novel_dir_for_record(archive_root: &Path, record: &NovelRecord) -> PathBuf {
    let mut dir = archive_root.join(&record.sitename);
    if record.use_subdirectory {
        let subdirectory = create_subdirectory_name(&record.file_title);
        if !subdirectory.is_empty() {
            dir.push(subdirectory);
        }
    }
    dir.push(&record.file_title);
    dir
}

pub fn existing_novel_dir_for_record(archive_root: &Path, record: &NovelRecord) -> PathBuf {
    let canonical = novel_dir_for_record(archive_root, record);
    if canonical.exists() {
        return canonical;
    }

    let legacy = archive_root.join(&record.sitename).join(&record.file_title);
    if legacy.exists() {
        return legacy;
    }

    canonical
}

pub static DATABASE: Mutex<Option<Database>> = parking_lot::const_mutex(None);

pub fn init_database() -> Result<()> {
    let db = Database::new()?;
    *DATABASE.lock() = Some(db);
    Ok(())
}

pub fn with_database<F, T>(f: F) -> Result<T>
where
    F: FnOnce(&Database) -> Result<T>,
{
    let guard = DATABASE.lock();
    let db = guard
        .as_ref()
        .ok_or_else(|| NarouError::Database("Database not initialized".to_string()))?;
    f(db)
}

pub fn with_database_mut<F, T>(f: F) -> Result<T>
where
    F: FnOnce(&mut Database) -> Result<T>,
{
    let mut guard = DATABASE.lock();
    let db = guard
        .as_mut()
        .ok_or_else(|| NarouError::Database("Database not initialized".to_string()))?;
    f(db)
}

#[cfg(test)]
mod tests {
    use super::create_subdirectory_name;

    #[test]
    fn subdirectory_name_matches_narou_rb_rule() {
        assert_eq!(create_subdirectory_name("n8858hb title"), "88");
        assert_eq!(create_subdirectory_name("２１年版"), "２１");
        assert_eq!(create_subdirectory_name(" n8858hb"), "n");
    }
}
