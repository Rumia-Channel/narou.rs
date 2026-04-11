use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::index_store::IndexStore;
use super::inventory::{Inventory, InventoryScope};
use super::novel_record::NovelRecord;

use crate::error::Result;

const ARCHIVE_ROOT_DIR: &str = "小説データ";
const DATABASE_NAME: &str = "database";

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
