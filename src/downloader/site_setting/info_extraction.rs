use std::collections::HashMap;

use regex::RegexBuilder;

use super::{SiteSetting, SiteSettingEntry, SiteSettingValue};
use crate::downloader::security::{MAX_REGEX_INPUT_LEN, MAX_YAML_REGEX_PATTERN_LEN};
use crate::downloader::util::{DEFAULT_REGEX_SIZE_LIMIT, decode_html_text};

fn build_standard_regex(pattern: &str) -> std::result::Result<regex::Regex, regex::Error> {
    if pattern.len() > MAX_YAML_REGEX_PATTERN_LEN {
        return Err(regex::Error::Syntax(format!(
            "YAML regex pattern exceeds {} bytes",
            MAX_YAML_REGEX_PATTERN_LEN
        )));
    }

    RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .multi_line(true)
        .size_limit(DEFAULT_REGEX_SIZE_LIMIT)
        .build()
}

#[derive(Debug)]
enum InfoDiagnostic { PatternTooLarge, InputTooLarge }

impl InfoDiagnostic {
    async fn emit(self, key: &str) {
        use crate::application::messages::{self, Stream};
        let message = match self {
            Self::PatternTooLarge => messages::download::warn_fancy_pattern_large(key),
            Self::InputTooLarge => messages::download::warn_fancy_input_large(key),
        };
        messages::emit_default_async(Stream::Stderr, &message).await;
    }
}

fn fancy_regex_allowed(pattern: &str, source: &str) -> Result<(), InfoDiagnostic> {
    if pattern.len() > MAX_YAML_REGEX_PATTERN_LEN { return Err(InfoDiagnostic::PatternTooLarge); }
    if source.len() > MAX_REGEX_INPUT_LEN { return Err(InfoDiagnostic::InputTooLarge); }
    Ok(())
}

/// Try matching with the standard regex crate first; fall back to fancy-regex
/// for patterns that use lookahead/lookbehind (unsupported by the regex crate).
fn try_regex_captures(pattern: &str, source: &str, key: &str) -> Result<Option<String>, InfoDiagnostic> {
    let re = build_standard_regex(pattern);

    if let Ok(re) = re {
        if let Some(caps) = re.captures(source) {
            for name in re.capture_names().flatten() {
                if capture_name_matches_key(key, name)
                    && let Some(m) = caps.name(name)
                {
                    return Ok(Some(decode_html_text(m.as_str())));
                }
            }
            if let Some(m) = caps.get(1) {
                return Ok(Some(decode_html_text(m.as_str())));
            }
        }
        return Ok(None);
    }

    // Fallback to fancy-regex for lookahead/lookbehind patterns
    fancy_regex_allowed(pattern, source)?;
    let fre = fancy_regex::RegexBuilder::new(pattern)
        .dot_matches_new_line(true)
        .multi_line(true)
        .build();
    if let Ok(fre) = fre
        && let Ok(Some(caps)) = fre.captures(source)
    {
        for name in fre.capture_names().flatten() {
            if capture_name_matches_key(key, name)
                && let Some(m) = caps.name(name)
            {
                return Ok(Some(decode_html_text(m.as_str())));
            }
        }
        if let Some(m) = caps.get(1) {
            return Ok(Some(decode_html_text(m.as_str())));
        }
    }
    Ok(None)
}

fn try_regex_captures_all(pattern: &str, source: &str, key: &str) -> Result<Vec<String>, InfoDiagnostic> {
    let re = build_standard_regex(pattern);

    if let Ok(re) = re {
        let mut values: Vec<String> = Vec::new();
        for caps in re.captures_iter(source) {
            let mut matched = false;
            for name in re.capture_names().flatten() {
                if capture_name_matches_key(key, name)
                    && let Some(m) = caps.name(name)
                {
                    values.push(decode_html_text(m.as_str()));
                    matched = true;
                    break;
                }
            }
            if !matched && let Some(m) = caps.get(1) {
                values.push(decode_html_text(m.as_str()));
            }
        }
        return Ok(values);
    }

    fancy_regex_allowed(pattern, source)?;
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
                    values.push(decode_html_text(m.as_str()));
                    matched = true;
                    break;
                }
            }
            if !matched && let Some(m) = caps.get(1) {
                values.push(decode_html_text(m.as_str()));
            }
        }
        return Ok(values);
    }

    Ok(Vec::new())
}

impl SiteSetting {
    pub async fn resolve_info_pattern(&self, key: &str, source: &str) -> Option<String> {
        self.resolve_info_pattern_with_captures(key, source, &HashMap::new()).await
    }

    pub async fn multi_match(&self, source: &str, keys: &[&str]) -> HashMap<String, String> {
        let mut match_values: HashMap<String, String> = HashMap::new();

        for key in keys {
            if let Some(value) = self.resolve_info_pattern_with_captures(key, source, &match_values).await
            {
                match_values.insert(key.to_string(), value);
            }
        }

        match_values
    }

    async fn resolve_info_pattern_with_captures(
        &self,
        key: &str,
        source: &str,
        prev_captures: &HashMap<String, String>,
    ) -> Option<String> {
        let value = match key {
            "t" => self.t.as_ref().or(self.title.as_ref()),
            "title" => self.title.as_ref().or(self.t.as_ref()),
            "w" => self.w.as_ref().or(self.author.as_ref()),
            "author" => self.author.as_ref().or(self.w.as_ref()),
            "s" => self.s.as_ref().or(self.story.as_ref()),
            "story" => self.story.as_ref().or(self.s.as_ref()),
            "nt" => self.nt.as_ref(),
            "ga" => self.ga.as_ref(),
            "gf" => self.gf.as_ref(),
            "nu" => self.nu.as_ref(),
            "gl" => self.gl.as_ref(),
            "l" => self.l.as_ref(),
            "tags" => self.tags.as_ref(),
            "sitename" => self.sitename_pattern.as_ref(),
            "ncode" => self.ncode.as_ref(),
            _ => return None,
        };

        let value = value?;

        let single = match value { SiteSettingValue::Single(value) => Some(value.as_str()), _ => None };
        let entries = match value { SiteSettingValue::Multiple(entries) => entries.as_slice(), _ => &[] };
        let patterns = single.into_iter().chain(entries.iter().filter_map(|entry| match entry {
            SiteSettingEntry::Plain(value) => Some(value.as_str()),
            SiteSettingEntry::Eval { .. } => None,
        }));
        // Each pure regex step returns at most one diagnostic. Await it before
        // evaluating the next user-defined pattern; never buffer a warning list.
        for pattern in patterns {
            let resolved = self.interpolate_with_captures(pattern, prev_captures);
            if key == "tags" {
                match try_regex_captures_all(&resolved, source, key) {
                    Ok(values) if !values.is_empty() => return Some(values.join("\n")),
                    Ok(_) => {}
                    Err(diagnostic) => diagnostic.emit(key).await,
                }
            }
            match try_regex_captures(&resolved, source, key) {
                Ok(Some(value)) => return Some(value),
                Ok(None) => {}
                Err(diagnostic) => diagnostic.emit(key).await,
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
    use crate::downloader::site_setting::SiteSetting;

    #[test]
    fn test_ncode_tags_regex_match() {
        // The ncode tags pattern uses negative lookahead which requires fancy-regex
        let pattern = "<dt class=\"p-infotop-data__title\">\\s*キーワード\\s*</dt>\\s*<dd class=\"p-infotop-data__value\">\\s*\n(?<tag>(?:(?!キーワードが設定されていません)[\\s\\S])*?)\\s*</dd>";

        let source = "<dt class=\"p-infotop-data__title\">キーワード</dt>\n<dd class=\"p-infotop-data__value\">\nR15&nbsp;残酷な描写あり&nbsp;近未来 シムワールド 無敵\n</dd>";

        let result = try_regex_captures(pattern, source, "tags").unwrap();
        assert!(
            result.is_some(),
            "tags regex should match via fancy-regex fallback"
        );
        let tag_val = result.unwrap();
        assert!(
            tag_val.contains("R15"),
            "should contain R15, got: {}",
            tag_val
        );
    }

    #[test]
    fn test_multiline_tag_capture_collects_all_matches() {
        let pattern = "^tag::(?<tag>.+?)$";
        let source = "tag::tag-a\ntag::tag-b\ntag::tag-c";
        let result = try_regex_captures_all(pattern, source, "tags").unwrap();
        assert_eq!(result, vec!["tag-a", "tag-b", "tag-c"]);
    }

    #[tokio::test]
    async fn multi_match_decodes_html_entities_in_story_capture() {
        let settings = SiteSetting::load_all().unwrap();
        let setting = settings
            .iter()
            .find(|s| s.domain == "novel18.syosetu.com")
            .unwrap();
        let html = r#"
<dt class="p-infotop-data__title">あらすじ</dt>
<dd class="p-infotop-data__value">&quot;氷の令嬢&quot; &amp; 隣人 &#39;test&#39;</dd>
"#;

        let values = setting.multi_match(html, &["s"]).await;

        assert_eq!(
            values.get("s").map(String::as_str),
            Some("\"氷の令嬢\" & 隣人 'test'")
        );
    }
}
