mod info_extraction;
mod interpolate;
mod loader;
mod serde_helpers;

use std::collections::HashMap;
use std::path::PathBuf;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::error::Result;

pub use serde_helpers::deserialize_yes_no_bool;

fn looks_like_pattern(s: &str) -> bool {
    s.contains("(?<") || s.contains("(?P<")
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sitename_pattern: Option<SiteSettingValue>,
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
    pub(super) compiled_preprocess: Option<crate::downloader::preprocess::PreprocessPipeline>,

    #[serde(skip)]
    pub(super) compiled_url: Vec<Regex>,
    #[serde(skip)]
    pub(super) compiled_subtitles: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_body: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_introduction: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_postscript: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_error_message: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_next_toc: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_toc_page_max: Option<Regex>,
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

        loader::dedup_paths(&mut load_dirs);

        Ok(loader::load_all_from_dirs(load_dirs))
    }

    pub(super) fn compile(&mut self) {
        if looks_like_pattern(&self.sitename) && self.sitename_pattern.is_none() {
            self.sitename_pattern = Some(SiteSettingValue::Single(self.sitename.clone()));
            self.sitename = self.name.clone();
        }
        if let Some(ref src) = self.preprocess {
            let result = crate::downloader::preprocess::PreprocessPipeline::compile(src);
            self.compiled_preprocess = result.ok();
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

    pub fn preprocess_pipeline(
        &self,
    ) -> Option<&crate::downloader::preprocess::PreprocessPipeline> {
        self.compiled_preprocess.as_ref()
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

        let settings = loader::load_all_from_dirs(vec![bundled, user]);
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

        let settings = loader::load_all_from_dirs(vec![bundled, user]);
        let setting = settings.iter().find(|s| s.name == "Example").unwrap();

        assert_eq!(setting.sitename, "Bundled");
        assert_eq!(setting.body_pattern.as_deref(), Some("bundled"));

        let _ = std::fs::remove_dir_all(root);
    }
}
