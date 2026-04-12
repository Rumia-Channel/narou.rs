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

pub fn cmd_freeze(targets: &[String], off: bool) {
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
            if off {
                updated.tags.retain(|t| t != "frozen");
            } else if !updated.tags.contains(&"frozen".to_string()) {
                updated.tags.push("frozen".to_string());
            }
            db.insert(updated);
            db.save()
        });

        let action = if off { "Unfroze" } else { "Froze" };
        match result {
            Ok(()) => println!("  {} ID: {}", action, id),
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
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        if !updated.tags.contains(&"frozen".to_string()) {
            updated.tags.push("frozen".to_string());
        }
        db.insert(updated);
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
