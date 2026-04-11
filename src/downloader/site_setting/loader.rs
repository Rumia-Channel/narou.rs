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
            if path.extension().and_then(|e| e.to_str()) == Some("yaml")
                || path.extension().and_then(|e| e.to_str()) == Some("yml")
            {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(raw_yaml) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
                        let name = raw_yaml
                            .get("name")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);

                        if let Some(existing) = name
                            .as_ref()
                            .and_then(|name| settings.iter_mut().find(|s| s.name == *name))
                        {
                            let incoming_version = raw_yaml.get("version").and_then(|v| v.as_f64());
                            if should_merge_site_setting(existing, incoming_version) {
                                if let Ok(merged) = merge_site_setting(existing, &content) {
                                    *existing = merged;
                                }
                            }
                        } else if let Ok(setting) =
                            serde_yaml::from_value::<SiteSetting>(raw_yaml)
                        {
                            settings.push(setting);
                        }
                    }
                }
            }
        }
    }
}

fn should_merge_site_setting(existing: &SiteSetting, incoming_version: Option<f64>) -> bool {
    incoming_version.is_none_or(|version| version >= existing.version)
}

fn merge_site_setting(
    existing: &SiteSetting,
    incoming_yaml: &str,
) -> std::result::Result<SiteSetting, serde_yaml::Error> {
    let mut base = serde_yaml::to_value(existing)?;
    let incoming: serde_yaml::Value = serde_yaml::from_str(incoming_yaml)?;

    if let (Some(base_map), Some(incoming_map)) = (base.as_mapping_mut(), incoming.as_mapping()) {
        for (key, value) in incoming_map {
            if key.as_str() == Some("name") || key.as_str() == Some("version") {
                continue;
            }
            base_map.insert(key.clone(), value.clone());
        }
    }

    serde_yaml::from_value(base)
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
