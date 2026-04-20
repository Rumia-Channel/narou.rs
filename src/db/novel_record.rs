use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::de;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NovelRecord {
    pub id: i64,
    pub author: String,
    pub title: String,
    pub file_title: String,
    pub toc_url: String,
    pub sitename: String,
    #[serde(default)]
    pub novel_type: u8,
    #[serde(default, deserialize_with = "deserialize_nilable_bool")]
    pub end: bool,
    #[serde(with = "crate::db::ruby_time")]
    pub last_update: DateTime<Utc>,
    #[serde(with = "crate::db::ruby_time::option", default)]
    pub new_arrivals_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub use_subdirectory: bool,
    #[serde(with = "crate::db::ruby_time::option", default)]
    pub general_firstup: Option<DateTime<Utc>>,
    #[serde(with = "crate::db::ruby_time::option", default)]
    pub novelupdated_at: Option<DateTime<Utc>>,
    #[serde(with = "crate::db::ruby_time::option", default)]
    pub general_lastup: Option<DateTime<Utc>>,
    #[serde(with = "crate::db::ruby_time::option", default)]
    pub last_mail_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub ncode: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub general_all_no: Option<i64>,
    #[serde(default)]
    pub length: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_nilable_bool")]
    pub suspend: bool,
    #[serde(default, deserialize_with = "deserialize_nilable_bool")]
    pub is_narou: bool,
    #[serde(with = "crate::db::ruby_time::option", default)]
    pub last_check_date: Option<DateTime<Utc>>,
    #[serde(
        default,
        deserialize_with = "deserialize_nilable_bool",
        skip_serializing_if = "std::ops::Not::not",
        rename = "_convert_failure"
    )]
    pub convert_failure: bool,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_fields: BTreeMap<String, serde_yaml::Value>,
}

fn deserialize_nilable_bool<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct NilableBoolVisitor;

    impl<'de> de::Visitor<'de> for NilableBoolVisitor {
        type Value = bool;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a boolean, a bool-like string/number, or null")
        }

        fn visit_bool<E>(self, value: bool) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(value)
        }

        fn visit_i64<E>(self, value: i64) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(value != 0)
        }

        fn visit_u64<E>(self, value: u64) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(value != 0)
        }

        fn visit_str<E>(self, value: &str) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            match value.trim().to_ascii_lowercase().as_str() {
                "" | "no" | "false" | "off" | "0" => Ok(false),
                "yes" | "true" | "on" | "1" => Ok(true),
                _ => Err(de::Error::invalid_value(de::Unexpected::Str(value), &self)),
            }
        }

        fn visit_none<E>(self) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(false)
        }

        fn visit_unit<E>(self) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(false)
        }
    }

    deserializer.deserialize_any(NilableBoolVisitor)
}

#[cfg(test)]
mod tests {
    use super::NovelRecord;

    #[test]
    fn deserialize_blank_end_as_false() {
        let yaml = r#"---
id: 115
author: 風見鶏
title: 異世界に来たけど至って普通に喫茶店とかやってますが何か問題でも？
file_title: 6858 異世界に来たけど至って普通に喫茶店とかやってますが何か問題でも？
toc_url: http://www.mai-net.net/bbs/sst/sst.php?act=dump&cate=all&all=6858&n=0&count=1
sitename: Arcadia
novel_type: 1
end:
last_update: 2023-08-04 19:40:38.885197200 +09:00
new_arrivals_date: 2023-08-04 19:40:38.885199900 +09:00
use_subdirectory: false
general_firstup:
novelupdated_at: 2023-08-01 18:25:00.000000000 +09:00
general_lastup: 2023-08-01 18:25:00.000000000 +09:00
length: 123
suspend: false
is_narou: false
last_check_date:
tags: []
"#;
        let record: NovelRecord = serde_yaml::from_str(yaml).unwrap();
        assert!(!record.end);
    }

    #[test]
    fn database_parity_preserves_unknown_fields_during_round_trip() {
        let yaml = r#"---
id: 0
author: author
title: title
file_title: file title
toc_url: https://example.com/0/
sitename: Example
last_update: 2026-04-20 00:00:00.000000000 +09:00
custom_flag: true
custom_map:
  answer: 42
"#;
        let record: NovelRecord = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            record.extra_fields.get("custom_flag"),
            Some(&serde_yaml::Value::Bool(true))
        );
        let nested = record.extra_fields.get("custom_map").unwrap();
        assert_eq!(nested["answer"].as_i64(), Some(42));

        let dumped = serde_yaml::to_string(&record).unwrap();
        assert!(dumped.contains("custom_flag: true"));
        assert!(dumped.contains("answer: 42"));
    }
}
