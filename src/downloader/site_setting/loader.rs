use std::path::PathBuf;

use super::SiteSetting;

pub fn load_all_from_dirs(load_dirs: Vec<PathBuf>) -> Vec<SiteSetting> {
    let mut settings = Vec::new();
    for dir in load_dirs {
        load_settings_from_dir(dir, &mut settings);
    }
    for setting in &mut settings {
        setting.compile();
    }
    settings
}

fn load_settings_from_dir(dir: PathBuf, settings: &mut Vec<SiteSetting>) {
    if !dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            let is_yaml = matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("yaml") | Some("yml")
            );
            if !is_yaml {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                tracing::warn!("サイト定義 {} を読み込めませんでした", path.display());
                continue;
            };
            let parsed = serde_yaml::from_str::<serde_yaml::Value>(&content);
            if let Err(error) = &parsed {
                tracing::warn!(
                    "サイト定義 {} を読み込めませんでした ({error})",
                    path.display()
                );
            }
            if let Ok(raw_yaml) = parsed {
                let name = raw_yaml
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);

                if let Some(existing) = name
                    .as_ref()
                    .and_then(|name| settings.iter_mut().find(|s| s.name == *name))
                {
                    // version gate とキー単位マージは
                    // `super::merge_user_definition_yaml` に一元化してある
                    // (core の `SiteDefinitions::effective_runtime()` と
                    //  規則が逸れないように)。
                    match serde_yaml::to_string(existing) {
                        Ok(bundled_yaml) => {
                            let merged = super::merge_user_definition_yaml(&bundled_yaml, &content);
                            match serde_yaml::from_str::<SiteSetting>(&merged) {
                                Ok(merged) => *existing = merged,
                                Err(error) => tracing::warn!(
                                    "サイト定義 {} を読み込めませんでした ({error})",
                                    path.display()
                                ),
                            }
                        }
                        Err(error) => tracing::warn!(
                            "サイト定義 {} を書き出せませんでした ({error})",
                            path.display()
                        ),
                    }
                } else {
                    match serde_yaml::from_value::<SiteSetting>(raw_yaml) {
                        Ok(setting) => settings.push(setting),
                        // 壊れた定義を黙って捨てると、ユーザーは原因不明で
                        // 「そのサイトが無い」ことになる。読み込み失敗を出す。
                        Err(error) => tracing::warn!(
                            "サイト定義 {} を読み込めませんでした ({error})",
                            path.display()
                        ),
                    }
                }
            }
        }
    }
}

pub fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut deduped = Vec::new();
    for path in paths.drain(..) {
        if !deduped.iter().any(|p: &PathBuf| p == &path) {
            deduped.push(path);
        }
    }
    *paths = deduped;
}
