#[cfg(feature = "native-runtime")]
pub mod database;
#[cfg(feature = "native-runtime")]
pub mod index_store;
#[cfg(feature = "native-runtime")]
pub mod inventory;
#[cfg(feature = "native-runtime")]
pub mod settings;
pub mod novel_codec;
pub mod novel_query;
#[cfg(feature = "native-runtime")]
pub(crate) mod state_inventory;
pub mod novel_record;
pub mod paths;
pub mod sort;
pub mod ruby_time;

#[cfg(feature = "native-runtime")]
pub use database::Database;
pub use sort::{compare_records_by_key, sort_key_valid, sort_keys, SORT_KEYS};
#[cfg(feature = "native-runtime")]
pub use inventory::narou_root_exists;
pub use novel_record::NovelRecord;
#[cfg(feature = "native-runtime")]
pub use paths::{
    create_subdirectory_name, existing_novel_dir_for_record, novel_dir_for_record,
};
#[cfg(all(feature = "worker-runtime", not(feature = "native-runtime")))]
pub use paths::{create_subdirectory_name, novel_dir_for_record};

#[cfg(feature = "native-runtime")]
use parking_lot::Mutex;

#[cfg(feature = "native-runtime")]
use crate::error::{NarouError, Result};

#[cfg(feature = "native-runtime")]
pub static DATABASE: Mutex<Option<Database>> = Mutex::new(None);

#[cfg(feature = "native-runtime")]
pub fn init_database() -> Result<()> {
    let mut db = Database::new()?;
    let sites = crate::downloader::site_setting::effective_site_settings();
    if db.all_records().values().any(|record| crate::application::attribute_tags::needs_initialization(record, &sites)) {
        db.update_records(|mut records| {
            for record in records.values_mut() {
                crate::application::attribute_tags::initialize_record(record, &sites);
            }
            Ok((records, ()))
        })?;
    }
    *DATABASE.lock() = Some(db);
    // 既存バーチャル小説の属性タグを、メンバーの属性構成に合わせて整合させる。
    // 混在しているものは保存済みの値を維持し、単一種別なら自動分類の値へ直す。
    // メンバーが見つからないものは既存値をそのまま残す。
    if let Err(error) = crate::native::virtual_novel::reconcile_attribute_tags() {
        tracing::warn!("バーチャル小説の属性タグを整合できません: {error}");
    }
    Ok(())
}

#[cfg(feature = "native-runtime")]
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

#[cfg(feature = "native-runtime")]
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
