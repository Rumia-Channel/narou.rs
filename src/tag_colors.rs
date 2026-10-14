use crate::compat;
use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;

pub use crate::application::tag_colors::{
    apply_tag_color_update, is_valid_new_tag_color_value, is_valid_tag_color, tag_color_names,
    MemoryTagColorStore, TagColorService, TagColorStore, TagColorUpdate, TagColors,
    NEW_TAG_COLOR_SETTING, TAG_COLOR_DEFAULT,
};

pub fn load_tag_colors(inventory: &Inventory) -> Result<TagColors> {
    let raw = inventory.load_raw("tag_colors", InventoryScope::Local)?;
    if raw.trim().is_empty() {
        return Ok(TagColors::default());
    }

    Ok(TagColors::from_yaml_payload(&raw)?)
}

/// Applies an owned delta while the inventory holds its file lock or SQLite
/// transaction. Resolve configured colors before calling this function.
pub fn update_tag_colors(inventory: &Inventory, update: TagColorUpdate) -> Result<TagColors> {
    inventory.update_yaml::<TagColors, serde_yaml::Value, _>(
        "tag_colors",
        InventoryScope::Local,
        move |value| {
            let mut colors = TagColors::from_yaml_value(&value);
            apply_tag_color_update(&mut colors, &update);
            Ok((colors.to_yaml_value(), colors))
        },
    )
}

pub fn configured_new_tag_color() -> Option<String> {
    compat::load_local_setting_string(NEW_TAG_COLOR_SETTING)
        .and_then(|raw| normalize_default_color(&raw))
}

fn normalize_default_color(raw: &str) -> Option<String> {
    let color = raw.trim().to_ascii_lowercase();
    if is_valid_tag_color(&color) {
        Some(color)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use crate::native::sqlite::state::{self, StorageMode};

    fn library(mode: StorageMode, compat: bool) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let narou_dir = temp.path().join(".narou");
        fs::create_dir_all(&narou_dir).unwrap();
        state::write_mode(&narou_dir, mode).unwrap();
        if compat {
            fs::write(narou_dir.join("local_setting.yaml"), "narou-compat: true\n").unwrap();
        }
        temp
    }

    #[test]
    fn normalize_default_color_accepts_case_and_ignores_default() {
        assert_eq!(
            normalize_default_color(" White "),
            Some("white".to_string())
        );
        assert_eq!(normalize_default_color(TAG_COLOR_DEFAULT), None);
        assert_eq!(normalize_default_color(""), None);
    }

    #[test]
    fn tag_color_update_inside_with_database_does_not_read_settings() {
        let _legacy = crate::test_support::legacy_yaml_guard();
        // Configured defaults are read before the database lock. The atomic
        // persistence callback only decodes, applies the delta, and encodes.
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());
        fs::create_dir_all(temp.path().join(".narou")).unwrap();
        fs::write(
            temp.path().join(".narou").join("local_setting.yaml"),
            "webui.new-tag-color: white\n",
        )
        .unwrap();

        *crate::db::DATABASE.lock() = None;
        crate::db::init_database().unwrap();

        let new_tag_color = configured_new_tag_color();
        let result = crate::db::with_database(|_db| {
            let inventory = Inventory::new(temp.path().to_path_buf());
            let colors = update_tag_colors(
                &inventory,
                TagColorUpdate::Ensure {
                    tags: vec!["fresh".to_string()],
                    default_color: new_tag_color,
                },
            )?;
            Ok(colors.color_for("fresh").map(str::to_string))
        })
        .unwrap();

        *crate::db::DATABASE.lock() = None;

        assert_eq!(result.as_deref(), Some("white"));
    }

    #[test]
    fn inventory_deltas_use_current_order_not_a_stale_loaded_table() {
        let _global = crate::test_support::global_state_guard();
        for (mode, compat) in [
            (StorageMode::Yaml, false),
            (StorageMode::Sqlite, false),
            (StorageMode::Sqlite, true),
        ] {
            let temp = library(mode, compat);
            let reader = Inventory::new(temp.path().to_path_buf());
            let writer = Inventory::new(temp.path().to_path_buf());
            update_tag_colors(&writer, TagColorUpdate::Set {
                tag: "X".to_string(), color: Some("green".to_string()),
            }).unwrap();
            let stale = load_tag_colors(&reader).unwrap();
            assert_eq!(stale.color_for("X"), Some("green"));
            update_tag_colors(&writer, TagColorUpdate::Set {
                tag: "X".to_string(), color: Some("red".to_string()),
            }).unwrap();
            let colors = update_tag_colors(&reader, TagColorUpdate::Ensure {
                tags: vec!["X".to_string(), "Y".to_string()], default_color: None,
            }).unwrap();
            assert_eq!(colors.color_for("X"), Some("red"));
            assert_eq!(colors.color_for("Y"), Some("white"));
            let colors = update_tag_colors(&reader, TagColorUpdate::Ensure {
                tags: vec!["X".to_string()], default_color: Some("blue".to_string()),
            }).unwrap();
            assert_eq!(colors.color_for("X"), Some("red"), "ensure must not overwrite");

            update_tag_colors(&writer, TagColorUpdate::Set {
                tag: "X".to_string(), color: None,
            }).unwrap();
            let colors = update_tag_colors(&reader, TagColorUpdate::Ensure {
                tags: vec!["Z".to_string()], default_color: None,
            }).unwrap();
            assert_eq!(colors.color_for("X"), None, "a stale reader must not resurrect removals");
            assert_eq!(colors.color_for("Z"), Some("green"));
            update_tag_colors(&writer, TagColorUpdate::Set {
                tag: "Z".to_string(), color: None,
            }).unwrap();
            let colors = update_tag_colors(&reader, TagColorUpdate::Ensure {
                tags: vec!["W".to_string()], default_color: None,
            }).unwrap();
            assert_eq!(colors.color_for("W"), Some("green"), "rotation uses the remaining last tag");

            let narou_dir = temp.path().join(".narou");
            let payload = if mode == StorageMode::Sqlite {
                let conn = rusqlite::Connection::open(narou_dir.join("db.sqlite")).unwrap();
                conn.query_row(
                    "SELECT value_yaml FROM app_state WHERE scope = 'inv' AND key = 'tag_colors'",
                    [], |row| row.get::<_, String>(0),
                ).unwrap()
            } else {
                fs::read_to_string(narou_dir.join("tag_colors.yaml")).unwrap()
            };
            if compat {
                assert_eq!(fs::read_to_string(narou_dir.join("tag_colors.yaml")).unwrap(), payload);
            } else if mode == StorageMode::Sqlite {
                assert!(!narou_dir.join("tag_colors.yaml").exists());
            }
            let persisted = TagColors::from_yaml_payload(&payload).unwrap();
            assert_eq!(persisted.order, vec!["Y".to_string(), "W".to_string()]);
            assert_eq!(persisted.color_for("X"), None);
            assert_eq!(persisted.color_for("W"), Some("green"));
        }
    }

    #[test]
    fn native_service_autoassignment_preserves_concurrent_set_and_remove() {
        let _global = crate::test_support::global_state_guard();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2).enable_all().build().unwrap();
        for mode in [StorageMode::Yaml, StorageMode::Sqlite] {
            for manual_color in [Some("red"), None] {
                let temp = library(mode, false);
                let store = std::sync::Arc::new(crate::native::application::NativeTagColorStore::new(
                    std::sync::Arc::new(Inventory::new(temp.path().to_path_buf())),
                ));
                let manual = TagColorService::new(store.clone());
                runtime.block_on(manual.set("X", Some("green"))).unwrap();
                let (entered_tx, entered_rx) = std::sync::mpsc::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let automatic = TagColorService::new(store.clone());
                let handle = runtime.handle().clone();
                let pending = std::thread::spawn(move || {
                    let tags = std::iter::once_with(move || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        "Y".to_string()
                    });
                    handle.block_on(automatic.for_tags(tags, None)).unwrap()
                });
                entered_rx.recv().unwrap();
                runtime.block_on(manual.set("X", manual_color)).unwrap();
                release_tx.send(()).unwrap();
                let result = pending.join().unwrap();
                let expected_y = if manual_color.is_some() { "white" } else { "green" };
                assert_eq!(result.get("X").map(String::as_str), manual_color);
                assert_eq!(result.get("Y").map(String::as_str), Some(expected_y));
                let persisted = load_tag_colors(&Inventory::new(temp.path().to_path_buf())).unwrap();
                assert_eq!(persisted.color_for("X"), manual_color);
                assert_eq!(persisted.color_for("Y"), Some(expected_y));
            }
        }
    }

    #[test]
    fn concurrent_native_manual_sets_merge_across_inventory_handles() {
        let _global = crate::test_support::global_state_guard();
        for (mode, compat) in [
            (StorageMode::Yaml, false),
            (StorageMode::Sqlite, false),
            (StorageMode::Sqlite, true),
        ] {
            let temp = library(mode, compat);
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(13));
            let writers = (0..12).map(|index| {
                let inventory = Inventory::new(temp.path().to_path_buf());
                assert!(load_tag_colors(&inventory).unwrap().into_map().is_empty());
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    update_tag_colors(&inventory, TagColorUpdate::Set {
                        tag: format!("tag-{index}"), color: Some("blue".to_string()),
                    }).unwrap();
                })
            }).collect::<Vec<_>>();
            barrier.wait();
            for writer in writers {
                writer.join().unwrap();
            }
            let persisted = load_tag_colors(&Inventory::new(temp.path().to_path_buf())).unwrap();
            assert_eq!(persisted.colors.len(), 12);
            for index in 0..12 {
                assert_eq!(persisted.color_for(&format!("tag-{index}")), Some("blue"));
            }
        }
    }
}
