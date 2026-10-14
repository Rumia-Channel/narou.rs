use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum IniValue {
    Integer(i64),
    Float(f64),
    Boolean(bool),
    String(String),
    #[default]
    Null,
}

#[derive(Debug, Clone)]
pub struct IniData {
    pub sections: HashMap<String, HashMap<String, IniValue>>,
}

impl Default for IniData {
    fn default() -> Self {
        Self::new()
    }
}

impl IniData {
    pub fn new() -> Self {
        let mut sections = HashMap::new();
        sections.insert("global".to_string(), HashMap::new());
        Self { sections }
    }

    pub fn load(text: &str) -> Self {
        let mut data = Self::new();
        let mut current_section = "global".to_string();

        for line in text.lines() {
            let trimmed = line.trim();

            if trimmed.is_empty() || trimmed.starts_with(';') {
                continue;
            }

            if let Some(section_name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
            {
                current_section = section_name.trim().to_string();
                data.sections.entry(current_section.clone()).or_default();
                continue;
            }

            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim().to_string();
                let value = value.trim();
                let ini_value = cast_ini_value(value);
                data.sections
                    .entry(current_section.clone())
                    .or_default()
                    .insert(key, ini_value);
            }
        }

        data
    }

    pub fn load_file(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let content = fs::read_to_string(path)?;
        Ok(Self::load(&content))
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&IniValue> {
        self.sections.get(section).and_then(|s| s.get(key))
    }

    pub fn get_global(&self, key: &str) -> Option<&IniValue> {
        self.get("global", key)
    }

    pub fn set(&mut self, section: &str, key: &str, value: IniValue) {
        self.sections
            .entry(section.to_string())
            .or_default()
            .insert(key.to_string(), value);
    }

    pub fn set_global(&mut self, key: &str, value: IniValue) {
        self.set("global", key, value);
    }

    pub fn to_ini_string(&self) -> String {
        use std::fmt::Write;
        let mut output = String::new();
        let sections = self.sections.get_key_value("global").into_iter()
            .chain(self.sections.iter().filter(|(name, _)| name.as_str() != "global"));
        let mut first = true;
        for (section, values) in sections {
            if !first { output.push('\n'); }
            first = false;
            if section != "global" {
                writeln!(&mut output, "[{section}]").expect("writing to a String is infallible");
            }
            for (key, value) in values {
                write!(&mut output, "{key} = ").expect("writing to a String is infallible");
                match value {
                    IniValue::String(value) => {
                        output.push('"');
                        output.push_str(value);
                        output.push('"');
                    }
                    IniValue::Integer(value) => {
                        write!(&mut output, "{value}").expect("writing to a String is infallible");
                    }
                    IniValue::Float(value) => {
                        write!(&mut output, "{value}").expect("writing to a String is infallible");
                        if value.is_finite() && value.fract() == 0.0 { output.push_str(".0"); }
                    }
                    IniValue::Boolean(value) => output.push_str(if *value { "true" } else { "false" }),
                    IniValue::Null => {}
                }
                output.push('\n');
            }
        }
        output
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let output = self.to_ini_string();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, output)?;
        Ok(())
    }

    pub fn global_section(&self) -> &HashMap<String, IniValue> {
        self.sections.get("global").unwrap()
    }
}

fn cast_ini_value(s: &str) -> IniValue {
    if s.is_empty() {
        return IniValue::Null;
    }

    if let Some(quoted) = s.strip_prefix('"').and_then(|value| value.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|value| value.strip_suffix('\'')))
        .filter(|value| !value.is_empty())
    {
        return IniValue::String(quoted.to_string());
    }
    if s.eq_ignore_ascii_case("true") { return IniValue::Boolean(true); }
    if s.eq_ignore_ascii_case("false") { return IniValue::Boolean(false); }
    if s.eq_ignore_ascii_case("nil") || s.eq_ignore_ascii_case("null") { return IniValue::Null; }
    if let Ok(value) = s.parse::<i64>() { return IniValue::Integer(value); }
    let unsigned = s.strip_prefix('+').or_else(|| s.strip_prefix('-')).unwrap_or(s);
    if unsigned.split_once('.').is_some_and(|(whole, fraction)| {
        whole.bytes().all(|byte| byte.is_ascii_digit()) && !fraction.is_empty()
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
    }) && let Ok(value) = s.parse::<f64>() {
        return IniValue::Float(value);
    }

    IniValue::String(s.to_string())
}
