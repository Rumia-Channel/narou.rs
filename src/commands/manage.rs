use std::collections::HashSet;
use std::path::{Path, PathBuf};

use narou_rs::compat::confirm;
use narou_rs::db::inventory::InventoryScope;

use super::{download, help, log, resolve_target_to_id};

use crate::logger;

pub fn cmd_list(tag: Option<&str>, frozen: bool) {
    logger::without_logging(|| {
        use narou_rs::db;

        if let Err(e) = db::init_database() {
            eprintln!("Error initializing database: {}", e);
            std::process::exit(1);
        }

        let records = db::with_database(|db| {
            let mut list: Vec<_> = db.all_records().values().collect();
            list.sort_by_key(|r| r.id);

            if let Some(tag_filter) = tag {
                list.retain(|r| r.tags.iter().any(|t| t == tag_filter));
            }
            if frozen {
                list.retain(|r| r.tags.iter().any(|t| t == "frozen"));
            }

            for r in &list {
                let type_str = match r.novel_type {
                    1 => "連載",
                    2 => "短編",
                    _ => "?",
                };
                let end_str = if r.end { " [完]" } else { "" };
                let tags_str = if r.tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", r.tags.join(", "))
                };
                println!(
                    " ID:{:>4} | {} | {}{} | {} | {}",
                    r.id, type_str, r.title, end_str, r.author, tags_str
                );
            }

            Ok(list.len())
        })
        .unwrap_or(0);

        println!();
        println!("Total: {} novels", records);
    });
}

pub fn cmd_tag(add: Option<&str>, remove: Option<&str>, targets: &[String]) {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    for target in targets {
        let Some(id) = resolve_target_to_id(target) else {
            eprintln!("  Not found: {}", target);
            continue;
        };

        let result = db::with_database_mut(|db| {
            let record = db
                .get(id)
                .cloned()
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
            let mut updated = record;
            if let Some(tag) = add {
                if !updated.tags.contains(&tag.to_string()) {
                    updated.tags.push(tag.to_string());
                }
            }
            if let Some(tag) = remove {
                updated.tags.retain(|t| t != tag);
            }
            db.insert(updated);
            db.save()
        });

        match result {
            Ok(()) => println!("  Tagged ID: {}", id),
            Err(e) => eprintln!("  Error: {}", e),
        }
    }
}

pub fn cmd_freeze(targets: &[String], list: bool, on: bool, off: bool) {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    if list {
        cmd_list(None, true);
        return;
    }

    if targets.is_empty() {
        crate::commands::help::display_command_help("freeze");
        return;
    }

    for target in download::tagname_to_ids(targets) {
        let Some(data) = download::get_data_by_target(&target) else {
            eprintln!("{} は存在しません", target);
            continue;
        };
        let id = data.id;

        let result = db::with_database_mut(|db| {
            let mut frozen_list: std::collections::HashMap<i64, serde_yaml::Value> = db
                .inventory()
                .load("freeze", narou_rs::db::inventory::InventoryScope::Local)?;
            let record = db
                .get(id)
                .cloned()
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
            let title = record.title.clone();
            let is_frozen = frozen_list.contains_key(&id);

            let mut updated = record;

            let should_freeze = if on {
                true
            } else if off {
                false
            } else {
                !is_frozen
            };

            if should_freeze {
                if !is_frozen {
                    updated.tags.push("frozen".to_string());
                }
                frozen_list.insert(id, serde_yaml::Value::Bool(true));
                db.insert(updated);
                db.inventory().save(
                    "freeze",
                    narou_rs::db::inventory::InventoryScope::Local,
                    &frozen_list,
                )?;
                db.save()?;
                Ok::<(String, bool), narou_rs::error::NarouError>((title, true))
            } else {
                if is_frozen {
                    updated.tags.retain(|t| t != "frozen");
                }
                if updated.tags.contains(&"404".to_string()) {
                    updated.tags.retain(|t| t != "404");
                }
                frozen_list.remove(&id);
                db.insert(updated);
                db.inventory().save(
                    "freeze",
                    narou_rs::db::inventory::InventoryScope::Local,
                    &frozen_list,
                )?;
                db.save()?;
                Ok::<(String, bool), narou_rs::error::NarouError>((title, false))
            }
        });

        match result {
            Ok((title, true)) => println!("{} を凍結しました", title),
            Ok((title, false)) => println!("{} の凍結を解除しました", title),
            Err(e) => eprintln!("  Error: {}", e),
        }
    }
}

pub fn cmd_remove(targets: &[String], yes: bool, with_file: bool, all_ss: bool) -> i32 {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        return 1;
    }

    let mut targets = targets.to_vec();
    if all_ss {
        let short_story_ids = collect_all_short_story_ids();
        if short_story_ids.is_empty() {
            println!("短編小説がひとつもありません");
            return 0;
        }
        targets.extend(short_story_ids);
    }

    if targets.is_empty() {
        help::display_command_help("remove");
        return 0;
    }

    let frozen_ids = load_inventory_ids("freeze");
    let locked_ids = load_inventory_ids("lock");

    for (index, target) in download::tagname_to_ids(&targets).into_iter().enumerate() {
        if index > 0 {
            println!("{}", "―".repeat(35));
        }

        let Some(data) = download::get_data_by_target(&target) else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };

        if locked_ids.contains(&data.id) {
            log::report_error(&format!("{} は変換中なため削除出来ませんでした", data.title));
            continue;
        }
        if frozen_ids.contains(&data.id) {
            println!("{} は凍結中です\n削除を中止しました", data.title);
            continue;
        }

        if !yes
            && !confirm(
                &build_remove_confirm_message(&data.title, with_file),
                false,
                true,
            )
        {
            continue;
        }

        match remove_novel_by_id(data.id, with_file) {
            Ok(outcome) => {
                if let Some(path) = outcome.removed_path {
                    println!("{} を完全に削除しました", path.display());
                }
                println!("{}", colorize_removed_message(&outcome.title));
            }
            Err(err) => log::report_error(&err),
        }
    }

    0
}

pub fn freeze_by_target(target: &str) {
    use narou_rs::db;

    let Some(data) = download::get_data_by_target(target) else {
        return;
    };
    let id = data.id;

    let result = db::with_database_mut(|db| {
        let mut frozen_list: std::collections::HashMap<i64, serde_yaml::Value> = db
            .inventory()
            .load("freeze", narou_rs::db::inventory::InventoryScope::Local)?;
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        if !updated.tags.contains(&"frozen".to_string()) {
            updated.tags.push("frozen".to_string());
        }
        frozen_list.insert(id, serde_yaml::Value::Bool(true));
        db.insert(updated);
        db.inventory().save(
            "freeze",
            narou_rs::db::inventory::InventoryScope::Local,
            &frozen_list,
        )?;
        db.save()
    });

    match result {
        Ok(()) => println!("  Froze ID: {}", id),
        Err(e) => eprintln!("  Error: {}", e),
    }
}

pub fn remove_by_target(target: &str) {
    let Some(data) = download::get_data_by_target(target) else {
        return;
    };

    match remove_novel_by_id(data.id, false) {
        Ok(outcome) => println!("{}", colorize_removed_message(&outcome.title)),
        Err(err) => log::report_error(&err),
    }
}

struct RemoveOutcome {
    title: String,
    removed_path: Option<PathBuf>,
}

fn remove_novel_by_id(id: i64, with_file: bool) -> Result<RemoveOutcome, String> {
    use narou_rs::db;

    let result = db::with_database_mut(|db| {
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
        let dir = db::existing_novel_dir_for_record(db.archive_root(), &record);
        remove_novel_files(&dir, with_file)
            .map_err(narou_rs::error::NarouError::Conversion)?;
        db.remove(id);
        db.save()?;
        Ok::<RemoveOutcome, narou_rs::error::NarouError>(RemoveOutcome {
            title: record.title,
            removed_path: with_file.then_some(dir),
        })
    });

    result.map_err(|e| e.to_string())
}

fn collect_all_short_story_ids() -> Vec<String> {
    use narou_rs::db;

    db::with_database(|db| {
        let mut ids = db
            .all_records()
            .values()
            .filter(|record| record.novel_type == 2)
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
    })
    .unwrap_or_default()
}

fn load_inventory_ids(name: &str) -> HashSet<i64> {
    use narou_rs::db;

    db::with_database(|db| {
        let values: std::collections::HashMap<i64, serde_yaml::Value> =
            db.inventory().load(name, InventoryScope::Local).unwrap_or_default();
        Ok(values.into_keys().collect::<HashSet<_>>())
    })
    .unwrap_or_default()
}

fn build_remove_confirm_message(title: &str, with_file: bool) -> String {
    if with_file {
        format!("{} を“完全に”削除しますか", title)
    } else {
        format!("{} を削除しますか", title)
    }
}

fn colorize_removed_message(title: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        format!("{} を削除しました", title)
    } else {
        format!("\x1b[1;32m{} を削除しました\x1b[0m", title)
    }
}

fn remove_novel_files(dir: &Path, with_file: bool) -> Result<(), String> {
    if with_file {
        if dir.exists() {
            std::fs::remove_dir_all(dir).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }

    let toc_path = dir.join("toc.yaml");
    if toc_path.exists() {
        std::fs::remove_file(toc_path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::remove_novel_files;

    #[test]
    fn remove_without_with_file_only_deletes_toc() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("novel");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("toc.yaml"), "toc").unwrap();
        std::fs::write(dir.join("section.txt"), "body").unwrap();

        remove_novel_files(&dir, false).unwrap();

        assert!(dir.exists());
        assert!(!dir.join("toc.yaml").exists());
        assert!(dir.join("section.txt").exists());
    }

    #[test]
    fn remove_with_file_deletes_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("novel");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("toc.yaml"), "toc").unwrap();

        remove_novel_files(&dir, true).unwrap();

        assert!(!dir.exists());
    }
}
