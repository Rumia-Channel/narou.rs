mod info_extraction;
mod interpolate;
mod loader;
mod serde_helpers;

use std::collections::HashMap;
#[cfg(debug_assertions)]
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
    pub series_url: Option<SiteSettingValue>,
    /// Pattern recognising an author page (作者ページ), e.g.
    /// `https://mypage.syosetu.com/(?<author_id>\d+)/`.
    ///
    /// `narou author add <url>` uses it to decide which definition owns the
    /// page; the tracked entry keeps only the site and the URL.
    #[serde(default)]
    pub author_url: Option<SiteSettingValue>,
    /// Pattern over an author page (or its API response) that yields the works
    /// it lists.
    ///
    /// Every match captures either `novel_url` (an absolute work URL) or the
    /// pieces `author_work_url` needs (`ncode`, `novel_id`, …). The result is
    /// downloaded exactly like a URL typed on the command line; works already
    /// in the library are skipped.
    #[serde(default)]
    pub author_novel_pattern: Option<String>,
    /// API endpoint that lists an author's works, when the site offers one
    /// (なろう: `https://api.syosetu.com/novelapi/api/?out=json&userid=\k<author_id>&lim=500`).
    ///
    /// Fetched instead of the author page itself; captures from `author_url`
    /// are available as `\k<...>`.
    #[serde(default)]
    pub author_api_url: Option<String>,
    /// Template that turns the captures of `author_novel_pattern` into a work
    /// URL, for sites whose listing carries ids rather than links
    /// (`\k<top_url>/\k<ncode>/`).
    #[serde(default)]
    pub author_work_url: Option<String>,
    /// URL of one series' episode list, for author listings that mix the
    /// episodes of a series in with standalone works (Pixiv).
    ///
    /// `\k<series_id>` plus whatever `author_url` captured can be used.
    #[serde(default)]
    pub author_series_episodes_url: Option<String>,
    /// Pattern over that episode list yielding `novel_id` (or `ncode`), so the
    /// episodes can be kept out of the author's work list.
    #[serde(default)]
    pub author_series_episodes_pattern: Option<String>,
    /// Pattern whose `author_next` capture is the next page of an author
    /// listing (ハーメルン pages its works). Followed until a page has no next
    /// link or points at one already visited — the visited set is what keeps a
    /// pager that links back from looping, so there is no page limit to hit.
    #[serde(default)]
    pub author_next_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_item_url: Option<String>,
    #[serde(default)]
    pub encoding: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, deserialize_with = "deserialize_yes_no_bool")]
    pub confirm_over18: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
    /// Extra request headers sent with every fetch for this site (e.g. the
    /// `Referer` some image hosts require). Names/values that could inject a
    /// second header are dropped when the policy is built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    /// Login page the `narou_rs_login` executable opens for this site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_url: Option<String>,
    /// Page content that marks a login wall (a page served with HTTP 200 that
    /// asks for authentication). Matching it retries with the stored cookie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_pattern: Option<SiteSettingValue>,
    /// Marker a definition emits when an anonymous fetch returned only part of
    /// the list (a retry with the stored cookie may see more).
    #[serde(default)]
    pub login_partial_pattern: Option<SiteSettingValue>,
    /// Minimum seconds between requests to this site. Sites that rate-limit
    /// aggressively (Pixiv) declare a floor; `None` uses `download.interval`.
    #[serde(default)]
    pub min_interval: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over18_pattern: Option<SiteSettingValue>,
    pub sitename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sitename_pattern: Option<SiteSettingValue>,
    #[serde(default, deserialize_with = "deserialize_yes_no_bool")]
    pub append_title_to_folder_name: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_strip_pattern: Option<String>,
    pub toc_url: SiteUrlTemplate,
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
    /// Pattern that yields the record's `ncode` from the fetched page. Used by
    /// sites whose URL carries no site-specific prefix (e.g. pixiv, where the
    /// numeric work id alone would collide between novels and series).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ncode: Option<SiteSettingValue>,
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
    pub title: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<SiteSettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub story: Option<SiteSettingValue>,
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
    pub(super) compiled_series_url: Vec<Regex>,
    #[serde(skip)]
    pub(super) compiled_author_url: Vec<Regex>,
    #[serde(skip)]
    pub(super) compiled_author_novel: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_author_next: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_author_series_episodes: Option<Regex>,
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
    pub(super) compiled_over18_pattern: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_login_pattern: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_login_partial_pattern: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_next_toc: Option<Regex>,
    #[serde(skip)]
    pub(super) compiled_toc_page_max: Option<Regex>,
}

/// A URL template that may depend on the shape of the target URL.
///
/// Sites that expose several kinds of target under one domain (Pixiv has
/// novels, novel series, artworks and manga series) need a different API URL
/// for each. The list form picks the first entry whose `match` regex applies
/// to the target URL; the plain string form is the single-template case.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SiteUrlTemplate {
    Single(String),
    ByTarget { by_target: Vec<SiteUrlTemplateEntry> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteUrlTemplateEntry {
    #[serde(rename = "match")]
    pub pattern: String,
    pub url: String,
}

impl SiteUrlTemplate {
    fn selected(&self, target_url: Option<&str>) -> Option<&str> {
        match self {
            SiteUrlTemplate::Single(url) => Some(url.as_str()),
            SiteUrlTemplate::ByTarget { by_target } => by_target
                .iter()
                .find(|entry| {
                    target_url.is_some_and(|target| {
                        Regex::new(&entry.pattern)
                            .map(|re| re.is_match(target))
                            .unwrap_or(false)
                    })
                })
                .map(|entry| entry.url.as_str()),
        }
    }
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

/// Preprocess marker carrying one work URL of an author listing.
const AUTHOR_NOVEL_MARKER: &str = "author_novel::";
/// Preprocess marker carrying a series whose episodes must be left out.
const AUTHOR_SERIES_MARKER: &str = "author_series::";

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

    /// Parse and compile bundled site definitions from YAML strings.
    ///
    /// Worker callers embed the `webnovel/*.yaml` files as static strings
    /// (e.g. from `build.rs`) and supply them here; no filesystem access is
    /// involved. User/bundled merge semantics are not applied — the caller
    /// supplies the effective list. Fails loudly on any malformed definition
    /// so a Worker never silently shrinks the supported site set.
    pub fn load_bundled(contents: &[&str]) -> Result<Vec<Self>> {
        let mut settings = Vec::new();
        for content in contents {
            let raw_yaml: serde_yaml::Value = serde_yaml::from_str(content)?;
            settings.push(serde_yaml::from_value::<SiteSetting>(raw_yaml)?);
        }
        for setting in &mut settings {
            setting.compile();
        }
        Ok(settings)
    }

    pub(super) fn compile(&mut self) {
        if looks_like_pattern(&self.sitename) && self.sitename_pattern.is_none() {
            self.sitename_pattern = Some(SiteSettingValue::Single(self.sitename.clone()));
            self.sitename = self.name.clone();
        }
        if let Some(ref src) = self.preprocess {
            match crate::downloader::preprocess::PreprocessPipeline::compile(src) {
                Ok(pipeline) => self.compiled_preprocess = Some(pipeline),
                Err(err) => {
                    tracing::warn!(
                        "preprocess compile failed for {}: {err}",
                        self.name
                    );
                }
            }
        }
        self.compiled_url = self.compile_url_patterns();
        self.compiled_series_url = self.compile_url_patterns_for(self.series_url.as_ref());
        self.compiled_author_url = self.compile_url_patterns_for(self.author_url.as_ref());
        self.compiled_author_novel = self
            .author_novel_pattern
            .as_deref()
            .and_then(|pattern| crate::downloader::util::compile_html_pattern(pattern).ok());
        self.compiled_author_next = self
            .author_next_pattern
            .as_deref()
            .and_then(|pattern| crate::downloader::util::compile_html_pattern(pattern).ok());
        self.compiled_author_series_episodes = self
            .author_series_episodes_pattern
            .as_deref()
            .and_then(|pattern| crate::downloader::util::compile_html_pattern(pattern).ok());
        self.compiled_subtitles = self.subtitles.as_ref().and_then(|v| self.compile_value(v));
        self.compiled_body = self
            .body_pattern
            .as_deref()
            .and_then(|s| crate::downloader::util::compile_html_pattern(s).ok());
        self.compiled_introduction = self
            .introduction_pattern
            .as_deref()
            .and_then(|s| crate::downloader::util::compile_html_pattern(s).ok());
        self.compiled_postscript = self
            .postscript_pattern
            .as_deref()
            .and_then(|s| crate::downloader::util::compile_html_pattern(s).ok());
        self.compiled_error_message = self
            .error_message
            .as_deref()
            .and_then(|s| crate::downloader::util::compile_html_pattern(s).ok());
        self.compiled_over18_pattern = self
            .over18_pattern
            .as_ref()
            .and_then(|v| self.compile_value(v));
        self.compiled_login_pattern = self
            .login_pattern
            .as_ref()
            .and_then(|v| self.compile_value(v));
        self.compiled_login_partial_pattern = self
            .login_partial_pattern
            .as_ref()
            .and_then(|v| self.compile_value(v));
        self.compiled_next_toc = self.next_toc.as_deref().and_then(|s| Regex::new(s).ok());
        self.compiled_toc_page_max = self
            .toc_page_max
            .as_deref()
            .and_then(|s| Regex::new(s).ok());
    }

    pub(crate) fn site_timezone(&self) -> super::SiteTimezone {
        super::site_timezone(self.timezone.as_deref())
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
        if resolved.len() > crate::downloader::security::MAX_YAML_REGEX_PATTERN_LEN {
            return None;
        }

        RegexBuilder::new(&resolved)
            .dot_matches_new_line(true)
            .multi_line(true)
            .size_limit(crate::downloader::util::DEFAULT_REGEX_SIZE_LIMIT)
            .build()
            .ok()
    }

    fn compile_url_patterns(&self) -> Vec<Regex> {
        self.compile_url_patterns_for(self.url.as_ref())
    }

    fn compile_url_patterns_for(&self, url_val: Option<&SiteSettingValue>) -> Vec<Regex> {
        let mut patterns = Vec::new();
        if let Some(url_val) = url_val {
            match url_val {
                SiteSettingValue::Single(s) => {
                    let resolved = self.interpolate(s);
                    if resolved.len() <= crate::downloader::security::MAX_YAML_REGEX_PATTERN_LEN
                        && let Ok(re) = RegexBuilder::new(&resolved)
                            .size_limit(crate::downloader::util::DEFAULT_REGEX_SIZE_LIMIT)
                            .build()
                    {
                        patterns.push(re);
                    }
                }
                SiteSettingValue::Multiple(entries) => {
                    for entry in entries {
                        if let SiteSettingEntry::Plain(s) = entry {
                            let resolved = self.interpolate(s);
                            if resolved.len()
                                <= crate::downloader::security::MAX_YAML_REGEX_PATTERN_LEN
                                && let Ok(re) = RegexBuilder::new(&resolved)
                                    .size_limit(crate::downloader::util::DEFAULT_REGEX_SIZE_LIMIT)
                                    .build()
                            {
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

    pub fn matches_series_url(&self, url: &str) -> bool {
        self.compiled_series_url.iter().any(|re| re.is_match(url))
    }

    /// Whether `url` is this site's author page.
    pub fn matches_author_url(&self, url: &str) -> bool {
        self.compiled_author_url.iter().any(|re| re.is_match(url))
    }

    /// Fetch target for `page_url`: the author API when the definition has
    /// one, otherwise the page itself.
    pub fn author_fetch_url(&self, page_url: &str) -> String {
        let Some(template) = self.author_api_url.as_deref() else {
            return page_url.to_string();
        };
        let captures = self.extract_author_url_captures(page_url).unwrap_or_default();
        self.interpolate_with_captures(template, &captures)
    }

    /// Next page of an author listing, when the definition pages it.
    /// Series the listing asked to check (`author_series::<id>` lines).
    ///
    /// Pixiv mixes the episodes of a series into the author's novel list; the
    /// definition emits the series so those episodes can be left out.
    pub fn author_series_ids(&self, source: &str) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for line in source.lines() {
            if let Some(id) = line.trim().strip_prefix(AUTHOR_SERIES_MARKER) {
                let id = id.trim().to_string();
                if !id.is_empty() && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        ids
    }

    /// URL of one series' episode list, for keeping its episodes out of the
    /// author listing. `captures` come from `author_url`.
    pub fn author_series_episodes_fetch_url(
        &self,
        captures: &HashMap<String, String>,
        series_id: &str,
    ) -> Option<String> {
        let template = self.author_series_episodes_url.as_deref()?;
        let mut named = captures.clone();
        named.insert("series_id".to_string(), series_id.to_string());
        Some(self.interpolate_with_captures(template, &named))
    }

    /// Episode ids found in a series' episode list.
    pub fn author_series_episode_ids(&self, source: &str) -> Vec<String> {
        let Some(pattern) = self.compiled_author_series_episodes.as_ref() else {
            return Vec::new();
        };
        pattern
            .captures_iter(source)
            .filter_map(|captures| {
                captures
                    .name("novel_id")
                    .or_else(|| captures.name("ncode"))
                    .map(|matched| matched.as_str().to_string())
            })
            .collect()
    }

    pub fn author_next_url(&self, source: &str) -> Option<String> {
        let pattern = self.compiled_author_next.as_ref()?;
        let captures = pattern.captures(source)?;
        captures
            .name("author_next")
            .map(|matched| crate::downloader::util::decode_html_text(matched.as_str()))
            .filter(|url| !url.is_empty())
    }

    /// Captures of `author_url` for a concrete author page.
    pub fn extract_author_url_captures(&self, url: &str) -> Option<HashMap<String, String>> {
        extract_captures_from_patterns(&self.compiled_author_url, url)
    }

    /// 作者ページ（またはその API 応答）から作品 URL を抜く。
    ///
    /// 定義が無ければ None。`novel_url` を capture しない定義では
    /// `author_work_url` に capture を流し込んで URL を作る。
    pub fn author_novel_urls(&self, source: &str) -> Option<Vec<String>> {
        // `preprocess:` の DSL が `author_novel::<url>` を出すサイト (Pixiv) は
        // パターン無しでも作品を列挙できる。列挙する口 (API + DSL) が
        // 定義されていなければ「対応していない」と答える。
        let declared = self.compiled_author_novel.is_some()
            || (self.author_api_url.is_some() && self.preprocess_pipeline().is_some());
        if !declared {
            return None;
        }
        let mut urls: Vec<String> = Vec::new();
        for line in source.lines() {
            if let Some(url) = line.trim().strip_prefix(AUTHOR_NOVEL_MARKER) {
                let url = crate::downloader::util::decode_html_text(url.trim());
                if !url.is_empty() && !urls.contains(&url) {
                    urls.push(url);
                }
            }
        }
        let Some(pattern) = self.compiled_author_novel.as_ref() else {
            return Some(urls);
        };
        for captures in pattern.captures_iter(source) {
            let named: HashMap<String, String> = pattern
                .capture_names()
                .flatten()
                .filter_map(|name| {
                    captures
                        .name(name)
                        .map(|value| (name.to_string(), value.as_str().to_string()))
                })
                .collect();
            let url = match named.get("novel_url") {
                // href は `&amp;` のまま取れることがある (実ページの書き方次第)。
                Some(url) => crate::downloader::util::decode_html_text(url),
                None => {
                    let Some(template) = self.author_work_url.as_deref() else {
                        continue;
                    };
                    self.interpolate_with_captures(template, &named)
                }
            };
            if !url.is_empty() && !urls.contains(&url) {
                urls.push(url);
            }
        }
        Some(urls)
    }

    pub fn debug_url_pattern(&self) -> Option<String> {
        self.compiled_url.first().map(|r| r.as_str().to_string())
    }

    /// Returns URL patterns for validation, with named groups replaced by non-capturing groups.
    /// Matches Ruby's validate_url_regexp_list behavior.
    pub fn url_patterns_for_validation(&self) -> Vec<String> {
        let named_group_re = regex::Regex::new(r"\?P?<[^>]+>").unwrap();
        self.compiled_url
            .iter()
            .chain(self.compiled_series_url.iter())
            .map(|re| {
                let pattern = re.as_str().to_string();
                let cleaned = named_group_re.replace_all(&pattern, "?:");
                format!("({cleaned})")
            })
            .collect()
    }

    pub fn toc_url(&self) -> String {
        match self.toc_url.selected(None) {
            Some(template) => self.interpolate(template),
            None => String::new(),
        }
    }

    /// Resolve `toc_url` for a concrete target URL, so definitions with several
    /// target shapes pick the right API endpoint. The selected template is also
    /// exposed as `\k<toc_url>` so `novel_info_url: \k<toc_url>` keeps working.
    pub fn toc_url_with_url_captures(&self, url: &str) -> Option<String> {
        let mut captures = self.extract_url_captures(url)?;
        let template = self.toc_url.selected(Some(url))?.to_string();
        captures.insert("toc_url".to_string(), template.clone());
        Some(self.interpolate_with_captures(&template, &captures))
    }

    pub fn extract_url_captures(&self, url: &str) -> Option<HashMap<String, String>> {
        extract_captures_from_patterns(&self.compiled_url, url)
    }

    pub fn extract_series_url_captures(&self, url: &str) -> Option<HashMap<String, String>> {
        extract_captures_from_patterns(&self.compiled_series_url, url)
    }

    pub fn compile_series_item_pattern(&self) -> Option<Regex> {
        let pattern = self.series_item_url.as_ref()?;
        let resolved = self.interpolate(pattern);
        if resolved.len() > crate::downloader::security::MAX_YAML_REGEX_PATTERN_LEN {
            return None;
        }
        RegexBuilder::new(&resolved)
            .dot_matches_new_line(true)
            .multi_line(true)
            .size_limit(crate::downloader::util::DEFAULT_REGEX_SIZE_LIMIT)
            .build()
            .ok()
    }

    pub fn series_item_url_from_captures(
        &self,
        series_url: &str,
        pattern: &Regex,
        caps: &regex::Captures,
    ) -> Option<String> {
        if let Some(href) = caps.name("href") {
            return Some(crate::downloader::util::build_section_url(
                self,
                series_url,
                href.as_str(),
            ));
        }

        let mut captures = HashMap::new();
        for name in pattern.capture_names().flatten() {
            if let Some(m) = caps.name(name) {
                captures.insert(name.to_string(), m.as_str().to_string());
            }
        }
        (!captures.is_empty()).then(|| {
            let template = self.toc_url.selected(None).unwrap_or_default();
            self.interpolate_with_captures(template, &captures)
        })
    }

    /// Resolve `novel_info_url`. `\k<toc_url>` inside it means "the same page as
    /// the table of contents", so the template for the current target is
    /// substituted first — a definition with several target shapes must not
    /// resolve it to another shape's endpoint.
    pub fn novel_info_url_with_captures(
        &self,
        url_captures: &HashMap<String, String>,
    ) -> Option<String> {
        let mut captures = url_captures.clone();
        if let Some(template) = self
            .toc_url
            .selected(captures.get("__target_url").map(String::as_str))
        {
            captures.insert("toc_url".to_string(), template.to_string());
        }
        self.novel_info_url
            .as_ref()
            .map(|u| self.interpolate_with_captures(u, &captures))
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

    /// Site-declared request headers, in a stable order, with `\k<...>`
    /// placeholders resolved (the values usually embed `top_url`).
    pub fn header_list(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .flat_map(|map| map.iter())
            .map(|(name, value)| (name.clone(), self.interpolate(value)))
            .collect()
    }

    /// Login page for this site, with `\k<...>` placeholders resolved.
    pub fn login_url(&self) -> Option<String> {
        self.login_url.as_ref().map(|url| self.interpolate(url))
    }

    pub fn compiled_login_pattern(&self) -> Option<&Regex> {
        self.compiled_login_pattern.as_ref()
    }

    /// Whether `source` shows only part of the list because the request was
    /// anonymous.
    pub fn is_partial_login_view(&self, source: &str) -> bool {
        self.compiled_login_partial_pattern
            .as_ref()
            .is_some_and(|pattern| pattern.is_match(source))
    }

    pub fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }

    pub fn over18_pattern(&self) -> Option<&Regex> {
        self.compiled_over18_pattern.as_ref()
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

    /// Section-extraction patterns compiled once at load with the same flags
    /// `compile_html_pattern` applies (dot-matches-newline + size limit).
    /// These replace per-section `compile_html_pattern` calls.
    pub fn compiled_body_pattern(&self) -> Option<&Regex> {
        self.compiled_body.as_ref()
    }

    pub fn compiled_introduction_pattern(&self) -> Option<&Regex> {
        self.compiled_introduction.as_ref()
    }

    pub fn compiled_postscript_pattern(&self) -> Option<&Regex> {
        self.compiled_postscript.as_ref()
    }

    pub fn compiled_error_message_pattern(&self) -> Option<&Regex> {
        self.compiled_error_message.as_ref()
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
        let target = captures.get("__target_url").map(String::as_str);
        match self.toc_url.selected(target) {
            Some(template) => self.interpolate_with_captures(template, captures),
            None => String::new(),
        }
    }

    pub fn get_next_url_with_captures(
        &self,
        next_url: &str,
        captures: &HashMap<String, String>,
    ) -> String {
        self.interpolate_with_captures(next_url, captures)
    }
}

fn extract_captures_from_patterns(
    patterns: &[Regex],
    url: &str,
) -> Option<HashMap<String, String>> {
    for re in patterns {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_page_works_are_extracted_from_the_page_or_its_api() {
        // API を持たないサイト向け: 作者ページの HTML から作品 URL を拾う。
        let mut setting: SiteSetting = serde_yaml::from_str(
            "name: Example\nsitename: Example\ndomain: example.com\ntop_url: https://example.com\ntoc_url: \\k<top_url>/\\k<ncode>/\n\
             author_url: ^https?://example\\.com/user/(?<author_id>\\d+)/\n\
             author_novel_pattern: (?<novel_url>https?://example\\.com/n\\d+[a-z]+/)\n",
        )
        .expect("synthetic definition");
        setting.compile();
        assert!(setting.matches_author_url("https://example.com/user/2842627/"));
        assert!(!setting.matches_author_url("https://example.com/n0915mt/"));

        let html = r#"
            <a href="https://example.com/n0915mt/">作品1</a>
            <a href="https://example.com/n1261ku/">作品2</a>
            <a href="https://example.com/n0915mt/">作品1 (再掲)</a>
        "#;
        assert_eq!(
            setting.author_novel_urls(html).expect("pattern"),
            vec![
                "https://example.com/n0915mt/".to_string(),
                "https://example.com/n1261ku/".to_string()
            ]
        );

        // API 向け: capture から `author_work_url` で URL を組み立てる。
        let mut api: SiteSetting = serde_yaml::from_str(
            "name: Api\nsitename: Api\ndomain: api.example.com\ntop_url: https://example.com\ntoc_url: \\k<top_url>/\\k<ncode>/\n\
             author_url: ^https?://example\\.com/user/(?<author_id>\\d+)/\n\
             author_novel_pattern: '\"ncode\":\"(?<ncode>[nN]\\d+[A-Za-z]+)\"'\n\
             author_work_url: \\k<top_url>/\\k<lower:ncode>/\n",
        )
        .expect("synthetic api definition");
        api.compile();
        assert_eq!(
            api.author_fetch_url("https://example.com/user/2842627/"),
            "https://example.com/user/2842627/",
            "author_api_url が無ければ作者ページをそのまま取得する"
        );
        assert_eq!(
            api.author_novel_urls(r#"[{"ncode":"N5181MT"},{"ncode":"N6275MR"}]"#)
                .expect("pattern"),
            vec![
                "https://example.com/n5181mt/".to_string(),
                "https://example.com/n6275mr/".to_string()
            ]
        );
    }

    #[test]
    fn partial_login_pattern_detects_an_incomplete_listing() {
        let yaml = r#"
name: Example
domain: example.com
top_url: https://example.com
sitename: Example
toc_url: https://example.com/\k<url>
login_partial_pattern: ^login_partial::1$
"#;
        let mut setting: SiteSetting = serde_yaml::from_str(yaml).unwrap();
        setting.compile();

        assert!(setting.is_partial_login_view("login_partial::1\nEpisode;1;..."));
        assert!(!setting.is_partial_login_view("Episode;1;..."));

        // Definitions without the key never report a partial view.
        let mut plain: SiteSetting = serde_yaml::from_str(
            "name: Example\ndomain: example.com\ntop_url: https://example.com\nsitename: Example\ntoc_url: x\\k<url>\n",
        )
        .unwrap();
        plain.compile();
        assert!(!plain.is_partial_login_view("login_partial::1"));
    }

    #[test]
    fn a_user_definition_below_the_bundled_version_is_ignored() {
        // 同梱定義を更新したら version を上げる必要がある理由を固定する:
        // ユーザー側が古い (<) ときは無視され、同版以上 (>=) のときだけ
        // キー単位で上書きマージされる。
        let root = std::env::temp_dir().join(format!(
            "narou_rs_site_setting_version_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let bundled = root.join("bundled").join("webnovel");
        let user = root.join("user").join("webnovel");
        std::fs::create_dir_all(&bundled).unwrap();
        std::fs::create_dir_all(&user).unwrap();

        let definition = |version: &str, author_url: &str| {
            format!(
                "name: Example\ndomain: example.com\ntop_url: https://example.com\nsitename: Example\ntoc_url: https://example.com/\\k<ncode>/\nversion: {version}\nauthor_url: {author_url}\n"
            )
        };
        std::fs::write(
            bundled.join("example.yaml"),
            definition("2.4", "^https?://example\\.com/bundled/"),
        )
        .unwrap();
        // 古いユーザー定義: 同梱版より低いので無視される。
        std::fs::write(
            user.join("example.yaml"),
            definition("2.3", "^https?://example\\.com/user/old/"),
        )
        .unwrap();
        let settings = loader::load_all_from_dirs(vec![bundled.clone(), user.clone()]);
        let setting = settings.iter().find(|s| s.name == "Example").unwrap();
        assert!(setting.matches_author_url("https://example.com/bundled/"));
        assert!(!setting.matches_author_url("https://example.com/user/old/"));

        // 同じ版のユーザー定義: そのキーが優先される。
        std::fs::write(
            user.join("example.yaml"),
            definition("2.4", "^https?://example\\.com/user/new/"),
        )
        .unwrap();
        let settings = loader::load_all_from_dirs(vec![bundled, user]);
        let setting = settings.iter().find(|s| s.name == "Example").unwrap();
        assert!(setting.matches_author_url("https://example.com/user/new/"));
        assert!(!setting.matches_author_url("https://example.com/bundled/"));
        let _ = std::fs::remove_dir_all(&root);
    }

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

    #[test]
    fn series_url_patterns_are_compiled_and_interpolated() {
        let mut setting: SiteSetting = serde_yaml::from_str(
            r#"
name: Example
domain: example.com
top_url: https://example.com
version: 1.0
url: https://example\.com/works/(?<ncode>\d+)
series_url: https://example\.com/users/[^/]+/collections/(?<series>\d+)
series_item_url: href="(?<href>/works/\d+)"
sitename: Example
toc_url: https://example.com/works/\\k<ncode>
"#,
        )
        .unwrap();
        setting.compile();

        assert!(setting.matches_series_url(
            "https://example.com/users/author/collections/12345"
        ));
        let pattern = setting.compile_series_item_pattern().unwrap();
        let caps = pattern
            .captures(r#"<a class="item" href="/works/67890">title</a>"#)
            .unwrap();
        assert_eq!(
            setting
                .series_item_url_from_captures(
                    "https://example.com/users/author/collections/12345",
                    &pattern,
                    &caps
                )
                .unwrap(),
            "https://example.com/works/67890"
        );
    }

    #[test]
    fn bundled_series_url_patterns_match_supported_sites() {
        let settings = SiteSetting::load_all().unwrap();
        let narou = settings
            .iter()
            .find(|s| s.domain == "ncode.syosetu.com")
            .unwrap();
        assert!(narou.matches_series_url("https://ncode.syosetu.com/s3795b/"));
        assert!(narou.matches_series_url("https://ncode.syosetu.com/s0064g/"));

        let novel18 = settings
            .iter()
            .find(|s| s.domain == "novel18.syosetu.com")
            .unwrap();
        assert!(novel18.matches_series_url("https://novel18.syosetu.com/xs3480b/"));

        let kakuyomu = settings.iter().find(|s| s.domain == "kakuyomu.jp").unwrap();
        assert!(kakuyomu.matches_series_url(
            "https://kakuyomu.jp/users/bottyan_1129/collections/16816452219618328030"
        ));

        let narou_pattern = narou.compile_series_item_pattern().unwrap();
        assert!(
            narou_pattern
                .is_match(r#"<a href="https://ncode.syosetu.com/n7826bd/">title</a>"#)
        );
        let novel18_pattern = novel18.compile_series_item_pattern().unwrap();
        assert!(
            novel18_pattern
                .is_match(r#"<a href="https://novel18.syosetu.com/n0001aa/">title</a>"#)
        );
        assert!(novel18_pattern.is_match(r#"<a href="/n3412lp/">title</a>"#));
    }
}
