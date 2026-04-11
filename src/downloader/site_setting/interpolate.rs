use std::collections::HashMap;

use regex::Regex;

use super::SiteSetting;

impl SiteSetting {
    pub fn interpolate(&self, pattern: &str) -> String {
        self.interpolate_with_captures(pattern, &HashMap::new())
    }

    pub fn interpolate_with_captures(
        &self,
        pattern: &str,
        captures: &HashMap<String, String>,
    ) -> String {
        let base = self.build_base_vars();
        let re = Regex::new(r"\\+k<(.+?)>").unwrap();
        re.replace_all(pattern, |caps: &regex::Captures| {
            let key = &caps[1];
            self.resolve_k_key(key, captures, &base, &mut Vec::new())
        })
        .to_string()
    }

    pub(super) fn resolve_k_key(
        &self,
        key: &str,
        captures: &HashMap<String, String>,
        base: &HashMap<String, String>,
        seen: &mut Vec<String>,
    ) -> String {
        if seen.contains(&key.to_string()) {
            return String::new();
        }
        seen.push(key.to_string());
        let value = captures
            .get(key)
            .cloned()
            .or_else(|| base.get(key).cloned());
        match value {
            Some(v) => {
                let re = Regex::new(r"\\+k<(.+?)>").unwrap();
                re.replace_all(&v, |caps: &regex::Captures| {
                    let k = &caps[1];
                    self.resolve_k_key(k, captures, base, seen)
                })
                .to_string()
            }
            None => format!("\\k<{}>", key),
        }
    }

    pub(super) fn build_base_vars(&self) -> HashMap<String, String> {
        let mut vars = HashMap::new();
        vars.insert("scheme".to_string(), self.scheme.clone());
        vars.insert("domain".to_string(), self.domain.clone());
        vars.insert("top_url".to_string(), self.top_url.clone());
        vars.insert("toc_url".to_string(), self.toc_url.clone());
        vars.insert("name".to_string(), self.name.clone());
        vars.insert("sitename".to_string(), self.sitename.clone());
        vars.insert("encoding".to_string(), self.encoding.clone());
        if let Some(ref v) = self.cookie {
            vars.insert("cookie".to_string(), v.clone());
        }
        if let Some(ref v) = self.href {
            vars.insert("href".to_string(), v.clone());
        }
        if let Some(ref v) = self.next_toc {
            vars.insert("next_toc".to_string(), v.clone());
        }
        if let Some(ref v) = self.next_url {
            vars.insert("next_url".to_string(), v.clone());
        }
        if let Some(ref v) = self.toc_page_max {
            vars.insert("toc_page_max".to_string(), v.clone());
        }
        if let Some(ref v) = self.body_pattern {
            vars.insert("body_pattern".to_string(), v.clone());
        }
        if let Some(ref v) = self.introduction_pattern {
            vars.insert("introduction_pattern".to_string(), v.clone());
        }
        if let Some(ref v) = self.postscript_pattern {
            vars.insert("postscript_pattern".to_string(), v.clone());
        }
        if let Some(ref v) = self.novel_info_url {
            vars.insert("novel_info_url".to_string(), v.clone());
        }
        if let Some(ref v) = self.error_message {
            vars.insert("error_message".to_string(), v.clone());
        }
        if let Some(ref v) = self.title_strip_pattern {
            vars.insert("title_strip_pattern".to_string(), v.clone());
        }
        if let Some(ref v) = self.narou_api_url {
            vars.insert("narou_api_url".to_string(), v.clone());
        }
        if let Some(ref v) = self.illust_current_url {
            vars.insert("illust_current_url".to_string(), v.clone());
        }
        if let Some(ref v) = self.illust_grep_pattern {
            vars.insert("illust_grep_pattern".to_string(), v.clone());
        }
        vars
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
}
