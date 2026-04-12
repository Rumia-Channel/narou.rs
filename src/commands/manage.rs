use super::resolve_target_to_id;

pub fn cmd_list(tag: Option<&str>, frozen: bool) {
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

    for target in targets {
        let Some(id) = resolve_target_to_id(target) else {
            eprintln!("{} は存在しません", target);
            continue;
        };

        let result = db::with_database_mut(|db| {
            let mut frozen_list: std::collections::HashMap<i64, serde_yaml::Value> = db
                .inventory()
                .load("freeze", narou_rs::db::inventory::InventoryScope::Local)?;
            let record = db
                .get(id)
                .cloned()
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
            let title = record.title.clone();
            let is_frozen = record.tags.contains(&"frozen".to_string());

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

pub fn cmd_remove(targets: &[String]) {
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

        remove_by_id(id);
    }
}

pub fn freeze_by_target(target: &str) {
    use narou_rs::db;

    let Some(id) = resolve_target_to_id(target) else {
        return;
    };

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
    let Some(id) = resolve_target_to_id(target) else {
        return;
    };

    remove_by_id(id);
}

fn remove_by_id(id: i64) {
    use narou_rs::db;

    let result = db::with_database_mut(|db| {
        if let Some(record) = db.remove(id) {
            let dir = db::existing_novel_dir_for_record(db.archive_root(), &record);
            let _ = std::fs::remove_dir_all(&dir);
            db.save()?;
            Ok::<String, narou_rs::error::NarouError>(record.title)
        } else {
            Err(narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))
        }
    });

    match result {
        Ok(title) => println!("  Removed: {} (ID: {})", title, id),
        Err(e) => eprintln!("  Error: {}", e),
    }
}
