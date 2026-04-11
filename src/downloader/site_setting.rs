use std::collections::HashMap;
use std::path::PathBuf;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteSetting {
    pub name: String,
    pub domain: String,
    #[serde(default)]
    pub scheme: String,
    pub top_url: String,
    #[serde(default)]
    pub version: f64,
    #[serde(default)]
    pub url: Option<SiteSettingValue>,
    #[serde(default)]
    pub encoding: String,
    #[serde(default, deserialize_with = "deserialize_yes_no_bool")]
    pub confirm_over18: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
    pub sitename: String,
    #[serde(default, deserialize_with = "deserialize_yes_no_bool")]
    pub append_title_to_folder_name: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_strip_pattern: Option<String>,
    pub toc_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitles: Option<SiteSettingValue>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_toc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toc_page_max: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introduction_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postscript_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub novel_info_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default)]
    pub is_narou: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narou_api_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub illust_current_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub illust_grep_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nt: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ga: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gf: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nu: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gl: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub novel_type_string: Option<HashMap<String, u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<SiteSettingValue>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preprocess: Option<String>,

    #[serde(skip)]
    compiled_preprocess: Option<crate::downloader::preprocess::PreprocessPipeline>,

    #[serde(skip)]
    compiled_url: Vec<Regex>,
    #[serde(skip)]
    compiled_subtitles: Option<Regex>,
    #[serde(skip)]
    compiled_body: Option<Regex>,
    #[serde(skip)]
    compiled_introduction: Option<Regex>,
    #[serde(skip)]
    compiled_postscript: Option<Regex>,
    #[serde(skip)]
    compiled_error_message: Option<Regex>,
    #[serde(skip)]
    compiled_next_toc: Option<Regex>,
    #[serde(skip)]
    compiled_toc_page_max: Option<Regex>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SiteSettingValue {
    Single(String),
    Multiple(Vec<SiteSettingEntry>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SiteSettingEntry {
    Plain(String),
    Eval { eval: String },
}

fn deserialize_yes_no_bool<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct YesNoBoolVisitor;

    impl<'de> Visitor<'de> for YesNoBoolVisitor {
        type Value = bool;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a boolean or a string \"yes\"/\"no\"")
        }

        fn visit_bool<E>(self, value: bool) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(value)
        }

        fn visit_str<E>(self, value: &str) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            match value.to_lowercase().as_str() {
                "yes" | "true" | "on" | "1" => Ok(true),
                "no" | "false" | "off" | "0" => Ok(false),
                _ => Err(de::Error::invalid_value(de::Unexpected::Str(value), &self)),
            }
        }
    }

    deserializer.deserialize_any(YesNoBoolVisitor)
}

impl SiteSetting {


    pub fn load_all() -> Result<Vec<Self>> {
        let mut load_dirs = Vec::new();

        if let Some(exe_dir) = std::env::current_exe()?.parent() {
            load_dirs.push(exe_dir.join("webnovel"));
        }

        #[cfg(debug_assertions)]
        {
            load_dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("webnovel"));
        }

        if let Ok(cwd) = std::env::current_dir() {
            load_dirs.push(cwd.join("webnovel"));
        }

        dedup_paths(&mut load_dirs);

        Ok(load_all_from_dirs(load_dirs))
    }

    fn compile(&mut self) {
        if let Some(ref src) = self.preprocess {
            self.compiled_preprocess =
                crate::downloader::preprocess::PreprocessPipeline::compile(src).ok();
        }
        self.compiled_url = self.compile_url_patterns();
        self.compiled_subtitles = self.subtitles.as_ref().and_then(|v| self.compile_value(v));
        self.compiled_body = self
            .body_pattern
            .as_deref()
            .and_then(|s| Regex::new(s).ok());
        self.compiled_introduction = self
            .introduction_pattern
            .as_deref()
            .and_then(|s| Regex::new(s).ok());
        self.compiled_postscript = self
            .postscript_pattern
            .as_deref()
            .and_then(|s| Regex::new(s).ok());
        self.compiled_error_message = self
            .error_message
            .as_deref()
            .and_then(|s| Regex::new(s).ok());
        self.compiled_next_toc = self.next_toc.as_deref().and_then(|s| Regex::new(s).ok());
        self.compiled_toc_page_max = self
            .toc_page_max
            .as_deref()
            .and_then(|s| Regex::new(s).ok());
    }

    fn compile_value(&self, value: &SiteSettingValue) -> Option<Regex> {
        let pattern = match value {
            SiteSettingValue::Single(s) => s.clone(),
            SiteSettingValue::Multiple(entries) => {
                let first_plain = entries
                    .iter()
                    .find(|e| matches!(e, SiteSettingEntry::Plain(_)));
                match first_plain {
                    Some(SiteSettingEntry::Plain(s)) => s.clone(),
                    _ => return None,
                }
            }
        };
        let resolved = self.interpolate(&pattern);
        RegexBuilder::new(&resolved)
            .dot_matches_new_line(true)
            .multi_line(true)
            .build()
            .ok()
    }

    fn compile_url_patterns(&self) -> Vec<Regex> {
        let mut patterns = Vec::new();
        if let Some(ref url_val) = self.url {
            match url_val {
                SiteSettingValue::Single(s) => {
                    let resolved = self.interpolate(s);
                    if let Ok(re) = Regex::new(&resolved) {
                        patterns.push(re);
                    }
                }
                SiteSettingValue::Multiple(entries) => {
                    for entry in entries {
                        if let SiteSettingEntry::Plain(s) = entry {
                            let resolved = self.interpolate(s);
                            if let Ok(re) = Regex::new(&resolved) {
                                patterns.push(re);
                            }
                        }
                    }
                }
            }
        }
        patterns
    }

    pub fn interpolate(&self, pattern: &str) -> String {
        let top_url_resolved = {
            let re = Regex::new(r"\\+k<(.+?)>").unwrap();
            let scheme = self.scheme.as_str();
            let domain = self.domain.as_str();
            let mut result = self.top_url.as_str().to_string();
            result = re
                .replace_all(&result, |caps: &regex::Captures| {
                    let key = &caps[1];
                    match key {
                        "scheme" => scheme.to_string(),
                        "domain" => domain.to_string(),
                        _ => caps
                            .get(0)
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_default(),
                    }
                })
                .to_string();
            result
        };

        let vars: HashMap<&str, &str> = [
            ("scheme", self.scheme.as_str()),
            ("domain", self.domain.as_str()),
            ("top_url", &top_url_resolved),
            ("toc_url", self.toc_url.as_str()),
        ]
        .into_iter()
        .collect();

        let re = Regex::new(r"\\+k<(.+?)>").unwrap();
        re.replace_all(pattern, |caps: &regex::Captures| {
            let key = &caps[1];
            match vars.get(key).copied() {
                Some(v) => v.to_string(),
                None => caps
                    .get(0)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default(),
            }
        })
        .to_string()
    }

    pub fn matches_url(&self, url: &str) -> bool {
        self.compiled_url.iter().any(|re| re.is_match(url))
    }

    pub fn debug_url_pattern(&self) -> Option<String> {
        self.compiled_url.first().map(|r| r.as_str().to_string())
    }

    pub fn toc_url(&self) -> String {
        self.interpolate(&self.toc_url)
    }

    pub fn extract_url_captures(&self, url: &str) -> Option<HashMap<String, String>> {
        for re in &self.compiled_url {
            if let Some(caps) = re.captures(url) {
                let mut captures: HashMap<String, String> = HashMap::new();
                for name in re.capture_names().flatten() {
                    if let Some(m) = caps.name(name) {
                        captures.insert(name.to_string(), m.as_str().to_string());
                    }
                }
                return Some(captures);
            }
        }
        None
    }

    pub fn toc_url_with_url_captures(&self, url: &str) -> Option<String> {
        let captures = self.extract_url_captures(url)?;
        Some(self.interpolate_with_captures(&self.toc_url, &captures))
    }

    pub fn novel_info_url_with_captures(
        &self,
        url_captures: &HashMap<String, String>,
    ) -> Option<String> {
        self.novel_info_url
            .as_ref()
            .map(|u| self.interpolate_with_captures(u, url_captures))
    }

    pub fn top_url(&self) -> String {
        self.interpolate(&self.top_url)
    }

    pub fn encoding(&self) -> &str {
        if self.encoding.is_empty() {
            "UTF-8"
        } else {
            &self.encoding
        }
    }

    pub fn cookie(&self) -> Option<&str> {
        self.cookie.as_deref()
    }

    pub fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }

    pub fn body_pattern(&self) -> Option<&str> {
        self.body_pattern.as_deref()
    }

    pub fn introduction_pattern(&self) -> Option<&str> {
        self.introduction_pattern.as_deref()
    }

    pub fn postscript_pattern(&self) -> Option<&str> {
        self.postscript_pattern.as_deref()
    }

    pub fn subtitles_pattern(&self) -> Option<&Regex> {
        self.compiled_subtitles.as_ref()
    }

    pub fn next_toc_pattern(&self) -> Option<&Regex> {
        self.compiled_next_toc.as_ref()
    }

    pub fn toc_page_max_pattern(&self) -> Option<&Regex> {
        self.compiled_toc_page_max.as_ref()
    }

    pub fn preprocess_pipeline(&self) -> Option<&crate::downloader::preprocess::PreprocessPipeline> {
        self.compiled_preprocess.as_ref()
    }

    pub fn resolve_info_pattern(&self, key: &str, source: &str) -> Option<String> {
        let value = match key {
            "t" => &self.t,
            "w" => &self.w,
            "s" => &self.s,
            "nt" => &self.nt,
            "ga" => &self.ga,
            "gf" => &self.gf,
            "nu" => &self.nu,
            "gl" => &self.gl,
            "l" => &self.l,
            "tags" => &self.tags,
            _ => return None,
        };

        let value = value.as_ref()?;

        let entries: Vec<SiteSettingEntry> = match value {
            SiteSettingValue::Single(s) => vec![SiteSettingEntry::Plain(s.clone())],
            SiteSettingValue::Multiple(entries) => entries.clone(),
        };

        for entry in &entries {
            let pattern = match entry {
                SiteSettingEntry::Plain(s) => s.as_str(),
                SiteSettingEntry::Eval { .. } => {
                    continue;
                }
            };

            let resolved = self.interpolate(pattern);
            if let Ok(re) = Regex::new(&resolved) {
                if let Some(caps) = re.captures(source) {
                    for name in re.capture_names().flatten() {
                        if name != key {
                            continue;
                        }
                        if let Some(m) = caps.name(name) {
                            return Some(m.as_str().to_string());
                        }
                    }
                    if let Some(m) = caps.get(1) {
                        return Some(m.as_str().to_string());
                    }
                }
            }
        }
        None
    }

    pub fn multi_match(&self, source: &str, keys: &[&str]) -> HashMap<String, String> {
        let mut match_values: HashMap<String, String> = HashMap::new();

        for key in keys {
            if let Some(value) = self.resolve_info_pattern_with_captures(key, source, &match_values)
            {
                match_values.insert(key.to_string(), value);
            }
        }

        match_values
    }

    fn resolve_info_pattern_with_captures(
        &self,
        key: &str,
        source: &str,
        prev_captures: &HashMap<String, String>,
    ) -> Option<String> {
        let value = match key {
            "t" => &self.t,
            "w" => &self.w,
            "s" => &self.s,
            "nt" => &self.nt,
            "ga" => &self.ga,
            "gf" => &self.gf,
            "nu" => &self.nu,
            "gl" => &self.gl,
            "l" => &self.l,
            "tags" => &self.tags,
            "title" => &self.t,
            "author" => &self.w,
            "story" => &self.s,
            _ => return None,
        };

        let value = value.as_ref()?;

        let entries: Vec<SiteSettingEntry> = match value {
            SiteSettingValue::Single(s) => vec![SiteSettingEntry::Plain(s.clone())],
            SiteSettingValue::Multiple(entries) => entries.clone(),
        };

        for entry in &entries {
            let pattern = match entry {
                SiteSettingEntry::Plain(s) => s.as_str(),
                SiteSettingEntry::Eval { .. } => {
                    continue;
                }
            };
            let resolved = self.interpolate_with_captures(pattern, prev_captures);
            let re = RegexBuilder::new(&resolved)
                .dot_matches_new_line(true)
                .multi_line(true)
                .build();
            if let Ok(re) = re {
                if let Some(caps) = re.captures(source) {
                    for name in re.capture_names().flatten() {
                        if let Some(m) = caps.name(name) {
                            let v = m.as_str().to_string();
                            if name == key {
                                return Some(v);
                            }
                        }
                    }
                    if let Some(m) = caps.get(1) {
                        return Some(m.as_str().to_string());
                    }
                }
            }
        }
        None
    }

    pub fn interpolate_subtitles_href(
        &self,
        template: &str,
        index: &str,
        url_captures: &HashMap<String, String>,
    ) -> String {
        let mut vars = url_captures.clone();
        vars.insert("index".to_string(), index.to_string());
        self.interpolate_with_captures(template, &vars)
    }

    pub fn interpolate_with_captures(
        &self,
        pattern: &str,
        captures: &HashMap<String, String>,
    ) -> String {
        let mut result = self.interpolate(pattern);

        let re = Regex::new(r"\\+k<(.+?)>").unwrap();
        result = re
            .replace_all(&result, |caps: &regex::Captures| {
                let key = &caps[1];
                captures
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| self.interpolate_key(key))
            })
            .to_string();

        result
    }

    fn interpolate_key(&self, key: &str) -> String {
        match key {
            "scheme" => self.scheme.clone(),
            "domain" => self.domain.clone(),
            "top_url" => self.interpolate(&self.top_url),
            _ => String::new(),
        }
    }

    pub fn get_novel_type_from_string(&self, status_text: &str) -> (u8, bool) {
        let empty = HashMap::new();
        let mapping: &HashMap<String, u8> = self.novel_type_string.as_ref().unwrap_or(&empty);
        let status_code = mapping
            .iter()
            .find(|(k, _)| *k == status_text)
            .map(|(_, v)| *v)
            .unwrap_or(1);

        let is_end = status_code == 3;
        let novel_type = match status_code {
            1 | 3 => 1,
            2 => 2,
            _ => 1,
        };

        (novel_type, is_end)
    }

    pub fn get_toc_url_with_captures(&self, captures: &HashMap<String, String>) -> String {
        self.interpolate_with_captures(&self.toc_url, captures)
    }

    pub fn get_next_url_with_captures(
        &self,
        next_url: &str,
        captures: &HashMap<String, String>,
    ) -> String {
        self.interpolate_with_captures(next_url, captures)
    }
}

fn load_all_from_dirs(load_dirs: Vec<PathBuf>) -> Vec<SiteSetting> {
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
                        } else if let Ok(setting) = serde_yaml::from_value::<SiteSetting>(raw_yaml)
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

fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut deduped = Vec::new();
    for path in paths.drain(..) {
        if !deduped.iter().any(|p: &PathBuf| p == &path) {
            deduped.push(path);
        }
    }
    *paths = deduped;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_webnovel_yaml_merges_over_bundled_yaml_by_name() {
        let root = std::env::temp_dir().join(format!(
            "narou_rs_site_setting_merge_{}",
            std::process::id()
        ));
        let bundled = root.join("bundled").join("webnovel");
        let user = root.join("user").join("webnovel");
        std::fs::create_dir_all(&bundled).unwrap();
        std::fs::create_dir_all(&user).unwrap();

        std::fs::write(
            bundled.join("example.yaml"),
            r#"
name: Example
domain: example.com
top_url: https://example.com
version: 1.0
url: https://example\.com/(?<ncode>n\d+)
sitename: Bundled
toc_url: https://example.com/\\k<ncode>/
body_pattern: bundled
"#,
        )
        .unwrap();
        std::fs::write(
            user.join("example.yaml"),
            r#"
name: Example
version: 1.0
sitename: User
body_pattern: user
"#,
        )
        .unwrap();

        let settings = load_all_from_dirs(vec![bundled, user]);
        let setting = settings.iter().find(|s| s.name == "Example").unwrap();

        assert_eq!(setting.domain, "example.com");
        assert_eq!(setting.sitename, "User");
        assert_eq!(setting.body_pattern.as_deref(), Some("user"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn older_user_webnovel_yaml_is_skipped_like_narou_rb() {
        let root = std::env::temp_dir().join(format!(
            "narou_rs_site_setting_old_skip_{}",
            std::process::id()
        ));
        let bundled = root.join("bundled").join("webnovel");
        let user = root.join("user").join("webnovel");
        std::fs::create_dir_all(&bundled).unwrap();
        std::fs::create_dir_all(&user).unwrap();

        std::fs::write(
            bundled.join("example.yaml"),
            r#"
name: Example
domain: example.com
top_url: https://example.com
version: 2.0
url: https://example\.com/(?<ncode>n\d+)
sitename: Bundled
toc_url: https://example.com/\\k<ncode>/
body_pattern: bundled
"#,
        )
        .unwrap();
        std::fs::write(
            user.join("example.yaml"),
            r#"
name: Example
version: 1.0
sitename: User
body_pattern: user
"#,
        )
        .unwrap();

        let settings = load_all_from_dirs(vec![bundled, user]);
        let setting = settings.iter().find(|s| s.name == "Example").unwrap();

        assert_eq!(setting.sitename, "Bundled");
        assert_eq!(setting.body_pattern.as_deref(), Some("bundled"));

        let _ = std::fs::remove_dir_all(root);
    }
}
