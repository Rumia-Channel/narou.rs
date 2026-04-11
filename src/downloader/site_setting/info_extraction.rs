use std::collections::HashMap;

use regex::{Regex, RegexBuilder};

use super::{SiteSetting, SiteSettingEntry, SiteSettingValue};

impl SiteSetting {
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
            if let Some(value) =
                self.resolve_info_pattern_with_captures(key, source, &match_values)
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
