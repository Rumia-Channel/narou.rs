use std::collections::{BTreeMap, HashMap};
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
        Self::with_inventory(inventory)
    }

    fn with_inventory(inventory: Inventory) -> Result<Self> {
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
            let loaded: BTreeMap<i64, NovelRecord> = serde_yaml::from_str(&raw)?;
            self.data = loaded
                .into_iter()
                .map(|(id, mut record)| {
                    record.id = id;
                    (id, record)
                })
                .collect();
        } else {
            self.data.clear();
        }
        self.index.reconcile(&self.data);
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        let content = serde_yaml::to_string(
            &self
                .data
                .iter()
                .map(|(&id, record)| {
                    let mut normalized = record.clone();
                    normalized.id = id;
                    (id, normalized)
                })
                .collect::<BTreeMap<_, _>>(),
        )?;
        self.inventory
            .save_raw(DATABASE_NAME, InventoryScope::Local, &content)?;
        self.index.flush(&self.inventory)?;
        Ok(())
    }

    pub fn update_records<T, F>(&mut self, update: F) -> Result<T>
    where
        F: FnOnce(BTreeMap<i64, NovelRecord>) -> Result<(BTreeMap<i64, NovelRecord>, T)>,
    {
        let result = self.inventory.update_yaml(
            DATABASE_NAME,
            InventoryScope::Local,
            |mut current: BTreeMap<i64, NovelRecord>| {
                for (id, record) in current.iter_mut() {
                    record.id = *id;
                }
                let (mut updated, result) = update(current)?;
                for (id, record) in updated.iter_mut() {
                    record.id = *id;
                }
                Ok((updated, result))
            },
        )?;
        self.refresh()?;
        self.index.flush(&self.inventory)?;
        Ok(result)
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
        self.data.keys().copied().max().map(|m| m + 1).unwrap_or(0)
    }

    pub fn sort_by(&self, key: &str, reverse: bool) -> Vec<&NovelRecord> {
        let mut records: Vec<&NovelRecord> = self.data.values().collect();
        records.sort_by(|a, b| {
            let ordering = compare_records_by_key(a, b, key).then_with(|| a.id.cmp(&b.id));
            if reverse {
                ordering.reverse()
            } else {
                ordering
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

fn compare_records_by_key(a: &NovelRecord, b: &NovelRecord, key: &str) -> std::cmp::Ordering {
    match key {
        "id" => a.id.cmp(&b.id),
        "title" => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
        "author" => a.author.to_lowercase().cmp(&b.author.to_lowercase()),
        "last_update" => a.last_update.cmp(&b.last_update),
        "general_lastup" => compare_optional(a.general_lastup, b.general_lastup),
        "sitename" => a.sitename.cmp(&b.sitename),
        "novel_type" => a.novel_type.cmp(&b.novel_type),
        "length" => compare_optional(a.length, b.length),
        _ => a.id.cmp(&b.id),
    }
}

fn compare_optional<T: Ord>(a: Option<T>, b: Option<T>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::Database;
    use crate::db::inventory::{Inventory, InventoryScope};
    use crate::db::novel_record::NovelRecord;
    use chrono::{TimeZone, Utc};

    #[test]
    fn database_parity_create_new_id_starts_at_zero() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let db = Database::with_inventory(Inventory::new(temp.path().to_path_buf())).unwrap();
        assert_eq!(db.create_new_id(), 0);
    }

    #[test]
    fn database_parity_save_preserves_unknown_fields_and_zero_id() {
        let temp = tempfile::tempdir().unwrap();
        let narou_dir = temp.path().join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        std::fs::write(
            narou_dir.join("database.yaml"),
            r#"0:
  id: 999
  author: author
  title: title
  file_title: file title
  toc_url: https://example.com/0/
  sitename: Example
  novel_type: 1
  end: false
  last_update: 2026-04-20 00:00:00.000000000 +09:00
  custom_flag: true
  nested:
    answer: 42
"#,
        )
        .unwrap();

        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();
        assert_eq!(db.get(0).unwrap().id, 0);
        assert_eq!(
            db.get(0)
                .unwrap()
                .extra_fields
                .get("custom_flag")
                .and_then(serde_yaml::Value::as_bool),
            Some(true)
        );

        db.save().unwrap();

        let saved = db
            .inventory()
            .load_raw("database", InventoryScope::Local)
            .unwrap();
        assert!(saved.contains("0:\n"));
        assert!(saved.contains("id: 0"));
        assert!(saved.contains("custom_flag: true"));
        assert!(saved.contains("answer: 42"));
        assert_eq!(db.create_new_id(), 1);
    }

    #[test]
    fn update_records_merges_against_current_database_yaml() {
        let temp = tempfile::tempdir().unwrap();
        let narou_dir = temp.path().join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();

        db.insert(sample_record(1, Some(10), &["old"]));
        db.save().unwrap();
        std::fs::write(
            narou_dir.join("database.yaml"),
            std::fs::read_to_string(narou_dir.join("database.yaml"))
                .unwrap()
                .replace("- old", "- edited"),
        )
        .unwrap();

        db.update_records(|mut records| {
            let record = records.get_mut(&1).unwrap();
            record.length = Some(20);
            Ok((records, ()))
        })
        .unwrap();

        let record = db.get(1).unwrap();
        assert_eq!(record.tags, vec!["edited"]);
        assert_eq!(record.length, Some(20));
    }

    #[test]
    fn sort_by_uses_typed_ordering_and_stable_none_order() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());
        let mut db = Database::with_inventory(inventory).unwrap();

        db.insert(sample_record(10, Some(2), &[]));
        db.insert(sample_record(9, Some(10), &[]));
        db.insert(sample_record(1, None, &[]));

        let by_id = db.sort_by("id", false).iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(by_id, vec![1, 9, 10]);
        let by_length = db
            .sort_by("length", false)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert_eq!(by_length, vec![10, 9, 1]);
    }

    fn sample_record(id: i64, length: Option<i64>, tags: &[&str]) -> NovelRecord {
        NovelRecord {
            id,
            author: "author".to_string(),
            title: format!("title {id}"),
            file_title: format!("title {id}"),
            toc_url: format!("https://example.com/{id}/"),
            sitename: "Example".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.timestamp_opt(1_700_000_000 + id, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length,
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }
}
