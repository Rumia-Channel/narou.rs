use narou_rs::mail::{
    MailSettingLoadError, ensure_mail_setting_file, load_mail_setting, send_target_with_setting,
};

pub struct MailOptions {
    pub targets: Vec<String>,
    pub force: bool,
}

pub fn cmd_mail(opts: MailOptions) {
    if let Err(code) = cmd_mail_inner(opts) {
        std::process::exit(code);
    }
}

fn cmd_mail_inner(opts: MailOptions) -> Result<(), i32> {
    if let Err(e) = narou_rs::db::init_database() {
        eprintln!("Error initializing database: {}", e);
        return Err(1);
    }

    let setting = match load_mail_setting() {
        Ok(setting) => setting,
        Err(MailSettingLoadError::NotFound(_)) => {
            let path = ensure_mail_setting_file().map_err(|e| {
                eprintln!("Error: {}", e);
                1
            })?;
            println!("created {}", path.display());
            println!(
                "メールの設定用ファイルを作成しました。設定ファイルを書き換えることで mail コマンドが有効になります。"
            );
            return Ok(());
        }
        Err(MailSettingLoadError::Incomplete(_)) => {
            eprintln!(
                "設定ファイルの書き換えが終了していないようです。\n設定ファイルは mail_setting.yaml にあります"
            );
            return Err(127);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            return Err(127);
        }
    };

    let send_all = opts.targets.is_empty();
    let targets = if send_all {
        collect_all_targets()
    } else {
        expand_targets(&opts.targets)
    };

    for target in targets {
        if let Err(e) = send_target_with_setting(&setting, &target, send_all, opts.force) {
            eprintln!("{}", e);
            return Err(127);
        }
    }

    let _ = narou_rs::db::with_database_mut(|db| db.save());
    Ok(())
}

fn collect_all_targets() -> Vec<String> {
    let mut ids = narou_rs::db::with_database(|db| Ok(db.ids())).unwrap_or_default();
    ids.sort_unstable();
    ids.into_iter()
        .filter(|id| {
            narou_rs::db::with_database(|db| {
                Ok(db
                    .get(*id)
                    .map(|record| !record.tags.contains(&"frozen".to_string()))
                    .unwrap_or(false))
            })
            .unwrap_or(false)
        })
        .map(|id| id.to_string())
        .collect()
}

fn expand_targets(targets: &[String]) -> Vec<String> {
    let (tag_index, all_ids) = narou_rs::db::with_database(|db| {
        Ok::<_, narou_rs::error::NarouError>((db.tag_index(), db.ids()))
    })
    .unwrap_or_default();
    let mut all_sorted = all_ids;
    all_sorted.sort_unstable();

    let mut expanded = Vec::new();
    for target in targets {
        if let Ok(id) = target.parse::<i64>() {
            let exists =
                narou_rs::db::with_database(|db| Ok(db.get(id).is_some())).unwrap_or(false);
            if exists {
                expanded.push(id.to_string());
                continue;
            }
        }

        if let Some(tag_name) = target.strip_prefix("^tag:") {
            if let Some(exclude_ids) = tag_index.get(tag_name) {
                let exclude: std::collections::HashSet<i64> = exclude_ids.iter().copied().collect();
                expanded.extend(
                    all_sorted
                        .iter()
                        .filter(|id| !exclude.contains(id))
                        .map(|id| id.to_string()),
                );
            } else {
                expanded.push(tag_name.to_string());
            }
            continue;
        }

        if let Some(tag_name) = target.strip_prefix("tag:") {
            if let Some(ids) = tag_index.get(tag_name) {
                expanded.extend(ids.iter().map(|id| id.to_string()));
            } else {
                expanded.push(tag_name.to_string());
            }
            continue;
        }

        if let Some(ids) = tag_index.get(target) {
            expanded.extend(ids.iter().map(|id| id.to_string()));
        } else {
            expanded.push(target.clone());
        }
    }

    let mut seen = std::collections::HashSet::new();
    expanded
        .into_iter()
        .filter(|target| seen.insert(target.clone()))
        .collect()
}
