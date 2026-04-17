use std::collections::HashMap;

use regex::RegexBuilder;

use super::{SiteSetting, SiteSettingEntry, SiteSettingValue};

/// Try matching with the standard regex crate first; fall back to fancy-regex
/// for patterns that use lookahead/lookbehind (unsupported by the regex crate).
fn try_regex_captures(
    pattern: &str,
    source: &str,
    key: &str,
) -> Option<String> {
    let re = RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .multi_line(true)
        .build();

    if let Ok(re) = re {
        if let Some(caps) = re.captures(source) {
            for name in re.capture_names().flatten() {
                if capture_name_matches_key(key, name) {
                    if let Some(m) = caps.name(name) {
                        return Some(m.as_str().to_string());
                    }
                }
            }
            if let Some(m) = caps.get(1) {
                return Some(m.as_str().to_string());
            }
        }
        return None;
    }

    // Fallback to fancy-regex for lookahead/lookbehind patterns
    let fre = fancy_regex::RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .multi_line(true)
        .build();
    if let Ok(fre) = fre {
        if let Ok(Some(caps)) = fre.captures(source) {
            for name in fre.capture_names().flatten() {
                if capture_name_matches_key(key, name) {
                    if let Some(m) = caps.name(name) {
                        return Some(m.as_str().to_string());
                    }
                }
            }
            if let Some(m) = caps.get(1) {
                return Some(m.as_str().to_string());
            }
        }
    }
    None
}

fn try_regex_captures_all(pattern: &str, source: &str, key: &str) -> Vec<String> {
    let re = RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .multi_line(true)
        .build();

    if let Ok(re) = re {
        let mut values: Vec<String> = Vec::new();
        for caps in re.captures_iter(source) {
            let mut matched = false;
            for name in re.capture_names().flatten() {
                if capture_name_matches_key(key, name)
                    && let Some(m) = caps.name(name)
                {
                    values.push(m.as_str().to_string());
                    matched = true;
                    break;
                }
            }
            if !matched
                && let Some(m) = caps.get(1)
            {
                values.push(m.as_str().to_string());
            }
        }
        return values;
    }

    let fre = fancy_regex::RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .multi_line(true)
        .build();
    if let Ok(fre) = fre {
        let mut values: Vec<String> = Vec::new();
        for caps_result in fre.captures_iter(source) {
            let Ok(caps) = caps_result else {
                continue;
            };
            let mut matched = false;
            for name in fre.capture_names().flatten() {
                if capture_name_matches_key(key, name)
                    && let Some(m) = caps.name(name)
                {
                    values.push(m.as_str().to_string());
                    matched = true;
                    break;
                }
            }
            if !matched
                && let Some(m) = caps.get(1)
            {
                values.push(m.as_str().to_string());
            }
        }
        return values;
    }

    Vec::new()
}

impl SiteSetting {
    pub fn resolve_info_pattern(&self, key: &str, source: &str) -> Option<String> {
        let value = match key {
            "t" => self.t.as_ref().or(self.title.as_ref()),
            "w" => self.w.as_ref().or(self.author.as_ref()),
            "s" => self.s.as_ref().or(self.story.as_ref()),
            "nt" => self.nt.as_ref(),
            "ga" => self.ga.as_ref(),
            "gf" => self.gf.as_ref(),
            "nu" => self.nu.as_ref(),
            "gl" => self.gl.as_ref(),
            "l" => self.l.as_ref(),
            "tags" => self.tags.as_ref(),
            "sitename" => self.sitename_pattern.as_ref(),
            _ => return None,
        };

        let value = value?;

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
            if key == "tags" {
                let values = try_regex_captures_all(&resolved, source, key);
                if !values.is_empty() {
                    return Some(values.join("\n"));
                }
            }
            if let Some(v) = try_regex_captures(&resolved, source, key) {
                return Some(v);
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
            "t" | "title" => self.t.as_ref().or(self.title.as_ref()),
            "w" | "author" => self.w.as_ref().or(self.author.as_ref()),
            "s" | "story" => self.s.as_ref().or(self.story.as_ref()),
            "nt" => self.nt.as_ref(),
            "ga" => self.ga.as_ref(),
            "gf" => self.gf.as_ref(),
            "nu" => self.nu.as_ref(),
            "gl" => self.gl.as_ref(),
            "l" => self.l.as_ref(),
            "tags" => self.tags.as_ref(),
            "sitename" => self.sitename_pattern.as_ref(),
            _ => return None,
        };

        let value = value?;

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
            if key == "tags" {
                let values = try_regex_captures_all(&resolved, source, key);
                if !values.is_empty() {
                    return Some(values.join("\n"));
                }
            }
            if let Some(v) = try_regex_captures(&resolved, source, key) {
                return Some(v);
            }
        }
        None
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
}

fn capture_name_matches_key(key: &str, capture_name: &str) -> bool {
    let aliases: &[&str] = match key {
        "t" | "title" => &["t", "title"],
        "w" | "author" => &["w", "writer", "author"],
        "s" | "story" => &["s", "story"],
        "nt" => &["nt", "novel_type"],
        "ga" => &["ga", "general_all_no"],
        "gf" => &["gf", "general_firstup"],
        "nu" => &["nu", "novelupdated_at"],
        "gl" => &["gl", "general_lastup"],
        "l" => &["l", "length"],
        "tags" => &["tags", "tag"],
        "sitename" => &["sitename"],
        _ => &[key],
    };
    aliases.contains(&capture_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ncode_tags_regex_match() {
        // The ncode tags pattern uses negative lookahead which requires fancy-regex
        let pattern = "<dt class=\"p-infotop-data__title\">\\s*キーワード\\s*</dt>\\s*<dd class=\"p-infotop-data__value\">\\s*\n(?<tag>(?:(?!キーワードが設定されていません)[\\s\\S])*?)\\s*</dd>";

        let source = "<dt class=\"p-infotop-data__title\">キーワード</dt>\n<dd class=\"p-infotop-data__value\">\nR15&nbsp;残酷な描写あり&nbsp;近未来 シムワールド 無敵\n</dd>";

        let result = try_regex_captures(pattern, source, "tags");
        assert!(result.is_some(), "tags regex should match via fancy-regex fallback");
        let tag_val = result.unwrap();
        assert!(tag_val.contains("R15"), "should contain R15, got: {}", tag_val);
    }

    #[test]
    fn test_multiline_tag_capture_collects_all_matches() {
        let pattern = "^tag::(?<tag>.+?)$";
        let source = "tag::tag-a\ntag::tag-b\ntag::tag-c";
        let result = try_regex_captures_all(pattern, source, "tags");
        assert_eq!(result, vec!["tag-a", "tag-b", "tag-c"]);
    }
}
