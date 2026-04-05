use std::collections::HashMap;
use std::path::PathBuf;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteSetting {
    pub name: String,
    pub domain: String,
    #[serde(default)]
    pub scheme: String,
    pub top_url: String,
    pub version: f64,
    #[serde(default)]
    pub url: Option<SiteSettingValue>,
    #[serde(default)]
    pub encoding: String,
    #[serde(default)]
    pub confirm_over18: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
    pub sitename: String,
    #[serde(default)]
    pub append_title_to_folder_name: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_strip_pattern: Option<String>,
    pub toc_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitles: Option<SiteSettingValue>,
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

    #[serde(skip)]
    compiled_url: Option<Regex>,
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

impl SiteSetting {
    pub fn load_all() -> Result<Vec<Self>> {
        let mut settings_map: HashMap<String, Self> = HashMap::new();

        if let Some(exe_dir) = std::env::current_exe()?.parent() {
            load_settings_from_dir(exe_dir.join("webnovel"), &mut settings_map);
        }

        if let Ok(cwd) = std::env::current_dir() {
            load_settings_from_dir(cwd.join("webnovel"), &mut settings_map);
        }

        let mut settings: Vec<Self> = settings_map.into_values().collect();
        settings.sort_by(|a, b| {
            b.version
                .partial_cmp(&a.version)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        for setting in &mut settings {
            setting.compile();
        }

        Ok(settings)
    }

    fn compile(&mut self) {
        self.compiled_url = self.url.as_ref().and_then(|v| self.compile_value(v));
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
                let first = entries.first()?;
                match first {
                    SiteSettingEntry::Plain(s) => s.clone(),
                    SiteSettingEntry::Eval { .. } => return None,
                }
            }
        };
        let resolved = self.interpolate(&pattern);
        Regex::new(&resolved).ok()
    }

    fn interpolate(&self, pattern: &str) -> String {
        let mut result = pattern.to_string();
        let vars: HashMap<&str, &str> = [
            ("scheme", self.scheme.as_str()),
            ("domain", self.domain.as_str()),
            ("top_url", self.top_url.as_str()),
        ]
        .into_iter()
        .collect();

        let re = Regex::new(r"\\k<(.+?)>").unwrap();
        result = re
            .replace_all(&result, |caps: &regex::Captures| {
                let key = &caps[1];
                vars.get(key).copied().unwrap_or("")
            })
            .to_string();

        result
    }

    pub fn matches_url(&self, url: &str) -> bool {
        self.compiled_url
            .as_ref()
            .map_or(false, |re| re.is_match(url))
    }

    pub fn toc_url(&self) -> String {
        self.interpolate(&self.toc_url)
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

        let patterns: Vec<&str> = match value {
            SiteSettingValue::Single(s) => vec![s.as_str()],
            SiteSettingValue::Multiple(entries) => entries
                .iter()
                .filter_map(|e| match e {
                    SiteSettingEntry::Plain(s) => Some(s.as_str()),
                    SiteSettingEntry::Eval { .. } => None,
                })
                .collect(),
        };

        for pattern in patterns {
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
}

fn load_settings_from_dir(dir: PathBuf, settings_map: &mut HashMap<String, SiteSetting>) {
    if !dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml")
                || path.extension().and_then(|e| e.to_str()) == Some("yml")
            {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(setting) = serde_yaml::from_str::<SiteSetting>(&content) {
                        let key = setting.domain.clone();
                        let should_insert = match settings_map.get(&key) {
                            Some(existing) => setting.version > existing.version,
                            None => true,
                        };
                        if should_insert {
                            settings_map.insert(key, setting);
                        }
                    }
                }
            }
        }
    }
}
