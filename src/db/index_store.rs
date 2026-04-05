use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::inventory::{Inventory, InventoryScope};
use crate::error::Result;

const INVENTORY_NAME: &str = "database_index";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexStoreData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub by_toc_url: HashMap<String, Vec<i64>>,
    #[serde(default)]
    pub by_title: HashMap<String, Vec<i64>>,
    #[serde(default)]
    pub meta: HashMap<i64, MetaEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetaEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toc_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

pub struct IndexStore {
    data: IndexStoreData,
    dirty: bool,
}

impl IndexStore {
    pub fn new() -> Self {
        Self {
            data: IndexStoreData::default(),
            dirty: false,
        }
    }

    pub fn load(inventory: &Inventory) -> Result<Self> {
        let raw = inventory.load_raw(INVENTORY_NAME, InventoryScope::Local)?;
        let data = if raw.is_empty() {
            IndexStoreData::default()
        } else {
            serde_yaml::from_str(&raw)?
        };
        Ok(Self { data, dirty: false })
    }

    pub fn reconcile(&mut self, database: &HashMap<i64, super::NovelRecord>) {
        let new_fingerprint = compute_fingerprint(database);
        if self.data.fingerprint.as_deref() == Some(&new_fingerprint) {
            return;
        }
        self.data.fingerprint = Some(new_fingerprint);
        self.data.by_toc_url.clear();
        self.data.by_title.clear();
        self.data.meta.clear();

        let mut sorted_ids: Vec<i64> = database.keys().copied().collect();
        sorted_ids.sort();

        for id in sorted_ids {
            if let Some(record) = database.get(&id) {
                self.store_entry(id, record);
            }
        }
        self.dirty = true;
    }

    pub fn lookup_by_toc_url(&self, toc_url: &str) -> Option<i64> {
        let normalized = normalize_url(toc_url)?;
        self.data
            .by_toc_url
            .get(&normalized)
            .and_then(|ids| ids.first().copied())
    }

    pub fn lookup_by_title(&self, title: &str) -> Option<i64> {
        let normalized = normalize_title(title)?;
        self.data
            .by_title
            .get(&normalized)
            .and_then(|ids| ids.first().copied())
    }

    pub fn upsert(&mut self, id: i64, toc_url: Option<&str>, title: Option<&str>) {
        self.remove(id);
        let mut meta = MetaEntry::default();
        if let Some(url) = toc_url {
            meta.toc_url = Some(url.to_string());
            let normalized = normalize_url(url);
            if let Some(norm) = normalized {
                Self::add_to_index(&mut self.data.by_toc_url, norm, id);
            }
        }
        if let Some(t) = title {
            meta.title = Some(t.to_string());
            let normalized = normalize_title(t);
            if let Some(norm) = normalized {
                Self::add_to_index(&mut self.data.by_title, norm, id);
            }
        }
        self.data.meta.insert(id, meta);
        self.dirty = true;
    }

    pub fn delete(&mut self, id: i64) {
        self.remove(id);
        self.dirty = true;
    }

    pub fn flush(&mut self, inventory: &Inventory) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let content = serde_yaml::to_string(&self.data)?;
        inventory.save_raw(INVENTORY_NAME, InventoryScope::Local, &content)?;
        self.dirty = false;
        Ok(())
    }

    fn remove(&mut self, id: i64) {
        if let Some(meta) = self.data.meta.remove(&id) {
            if let Some(url) = meta.toc_url {
                let normalized = normalize_url(&url);
                if let Some(norm) = normalized {
                    Self::remove_from_index(&mut self.data.by_toc_url, &norm, id);
                }
            }
            if let Some(title) = meta.title {
                let normalized = normalize_title(&title);
                if let Some(norm) = normalized {
                    Self::remove_from_index(&mut self.data.by_title, &norm, id);
                }
            }
        }
    }

    fn store_entry(&mut self, id: i64, record: &super::NovelRecord) {
        let mut meta = MetaEntry::default();
        meta.toc_url = Some(record.toc_url.clone());
        meta.title = Some(record.title.clone());

        let toc_norm = normalize_url(&record.toc_url);
        if let Some(norm) = toc_norm {
            Self::add_to_index(&mut self.data.by_toc_url, norm, id);
        }
        let title_norm = normalize_title(&record.title);
        if let Some(norm) = title_norm {
            Self::add_to_index(&mut self.data.by_title, norm, id);
        }
        self.data.meta.insert(id, meta);
    }

    fn add_to_index(index: &mut HashMap<String, Vec<i64>>, key: String, id: i64) {
        let ids = index.entry(key).or_default();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }

    fn remove_from_index(index: &mut HashMap<String, Vec<i64>>, key: &str, id: i64) {
        if let Some(ids) = index.get_mut(key) {
            ids.retain(|&i| i != id);
            if ids.is_empty() {
                index.remove(key);
            }
        }
    }
}

fn normalize_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_lowercase())
}

fn normalize_title(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_lowercase())
}

fn compute_fingerprint(database: &HashMap<i64, super::NovelRecord>) -> String {
    use sha2::{Digest, Sha256};

    let mut entries: Vec<&super::NovelRecord> = database.values().collect();
    entries.sort_by_key(|r| r.id);

    let mut hasher = Sha256::new();
    for record in entries {
        hasher.update(record.id.to_le_bytes());
        hasher.update(record.toc_url.as_bytes());
        hasher.update(record.title.as_bytes());
        hasher.update(record.last_update.timestamp().to_le_bytes());
    }

    hex::encode(hasher.finalize())
}
