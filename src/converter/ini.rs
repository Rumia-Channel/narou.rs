use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IniValue {
    Integer(i64),
    Float(f64),
    Boolean(bool),
    String(String),
    Null,
}

impl Default for IniValue {
    fn default() -> Self {
        IniValue::Null
    }
}

#[derive(Debug, Clone)]
pub struct IniData {
    pub sections: HashMap<String, HashMap<String, IniValue>>,
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
        let mut output = String::new();

        let mut first = true;
        for (section, values) in &self.sections {
            if !first {
                output.push('\n');
            }
            first = false;

            if section != "global" {
                output.push_str(&format!("[{}]\n", section));
            }

            for (key, value) in values {
                let value_str = match value {
                    IniValue::String(s) => s.clone(),
                    IniValue::Integer(i) => i.to_string(),
                    IniValue::Float(f) => f.to_string(),
                    IniValue::Boolean(b) => {
                        if *b {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        }
                    }
                    IniValue::Null => String::new(),
                };
                output.push_str(&format!("{} = {}\n", key, value_str));
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

    if let Some(quoted) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return IniValue::String(quoted.to_string());
    }
    if let Some(quoted) = s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return IniValue::String(quoted.to_string());
    }

    let lower = s.to_lowercase();
    if lower == "true" {
        return IniValue::Boolean(true);
    }
    if lower == "false" {
        return IniValue::Boolean(false);
    }
    if lower == "nil" || lower == "null" {
        return IniValue::Null;
    }

    if let Ok(i) = s.parse::<i64>() {
        return IniValue::Integer(i);
    }

    if let Ok(f) = s.parse::<f64>() {
        return IniValue::Float(f);
    }

    IniValue::String(s.to_string())
}
