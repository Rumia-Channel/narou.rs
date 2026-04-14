pub mod convert;
pub mod download;
pub mod help;
pub mod init;
pub mod mail;
pub mod manage;
pub mod setting;
pub mod version;
pub mod update;
pub mod web;

fn resolve_target_to_id(target: &str) -> Option<i64> {
    if let Ok(i) = target.parse::<i64>() {
        return Some(i);
    }
    narou_rs::db::with_database(|db| Ok(db.find_by_title(target).map(|r| r.id)))
        .ok()
        .flatten()
}
