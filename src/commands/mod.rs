use std::collections::HashMap;

use narou_rs::compat::yaml_value_to_string;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::{Downloader, TargetType};
use narou_rs::db::inventory::{Inventory, InventoryScope};

pub mod alias;
pub mod backup;
pub mod browser;
pub mod clean;
pub mod convert;
pub mod csv;
pub mod diff;
pub mod download;
pub mod folder;
pub mod help;
pub mod init;
pub mod inspect;
pub mod log;
pub mod mail;
pub mod manage;
pub mod send;
pub mod setting;
pub mod trace;
pub mod update;
pub mod version;
pub mod web;

fn resolve_alias_target(target: &str) -> String {
    narou_rs::db::with_database(|db| {
        let aliases: HashMap<String, serde_yaml::Value> =
            db.inventory().load("alias", InventoryScope::Local)?;
        Ok(aliases.get(target).and_then(yaml_value_to_string))
    })
    .ok()
    .flatten()
    .unwrap_or_else(|| target.to_string())
}

fn resolve_target_to_id(target: &str) -> Option<i64> {
    let target = resolve_alias_target(target);
    if let Ok(i) = target.parse::<i64>() {
        return narou_rs::db::with_database(|db| Ok(db.get(i).map(|r| r.id)))
            .ok()
            .flatten();
    }
    match Downloader::get_target_type(&target) {
        TargetType::Url => {
            let site_settings = SiteSetting::load_all().ok()?;
            let setting = site_settings.iter().find(|s| s.matches_url(&target))?;
            let toc_url = setting
                .toc_url_with_url_captures(&target)
                .unwrap_or_else(|| setting.toc_url());
            narou_rs::db::with_database(|db| Ok(db.get_by_toc_url(&toc_url).map(|r| r.id)))
                .ok()
                .flatten()
        }
        TargetType::Ncode => {
            let ncode = target.to_lowercase();
            narou_rs::db::with_database(|db| {
                Ok(db
                    .all_records()
                    .values()
                    .find(|r| {
                        r.ncode.as_deref() == Some(ncode.as_str())
                            || r.toc_url
                                .to_lowercase()
                                .trim_end_matches('/')
                                .ends_with(&format!("/{}", ncode))
                    })
                    .map(|r| r.id))
            })
            .ok()
            .flatten()
        }
        TargetType::Id => None,
        TargetType::Other => {
            narou_rs::db::with_database(|db| Ok(db.find_by_title(&target).map(|r| r.id)))
                .ok()
                .flatten()
        }
    }
}

fn latest_convert_target() -> Option<String> {
    let inventory = Inventory::with_default_root().ok()?;
    let latest: HashMap<String, serde_yaml::Value> = inventory
        .load("latest_convert", InventoryScope::Local)
        .ok()?;
    latest.get("id").and_then(yaml_value_to_string)
}

#[cfg(test)]
mod tests {
    use super::{latest_convert_target, resolve_target_to_id};
    use chrono::{TimeZone, Utc};
    use narou_rs::db::{self, NovelRecord};
    use narou_rs::error::NarouError;

    fn sample_record(id: i64, toc_url: &str, ncode: Option<&str>, title: &str) -> NovelRecord {
        NovelRecord {
            id,
            author: "author".to_string(),
            title: title.to_string(),
            file_title: format!("file-{id}"),
            toc_url: toc_url.to_string(),
            sitename: "小説家になろう".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 4, 20, 0, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: ncode.map(|value| value.to_string()),
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn database_parity_latest_convert_target_reads_zero_id() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        std::fs::write(temp.path().join(".narou").join("latest_convert.yaml"), "id: 0\n").unwrap();

        assert_eq!(latest_convert_target().as_deref(), Some("0"));
    }

    #[test]
    fn resolve_target_to_id_accepts_id_ncode_url_and_alias() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        std::fs::write(temp.path().join(".narou").join("alias.yaml"), "sample: n9669bk\n").unwrap();

        db::init_database().unwrap();
        db::with_database_mut(|db| {
            db.insert(sample_record(
                0,
                "https://ncode.syosetu.com/n9669bk/",
                Some("n9669bk"),
                "sample title",
            ));
            Ok::<(), NarouError>(())
        })
        .unwrap();

        assert_eq!(resolve_target_to_id("0"), Some(0));
        assert_eq!(resolve_target_to_id("n9669bk"), Some(0));
        assert_eq!(resolve_target_to_id("https://ncode.syosetu.com/n9669bk/"), Some(0));
        assert_eq!(resolve_target_to_id("sample"), Some(0));
    }
}
