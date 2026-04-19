use chrono::{DateTime, FixedOffset, LocalResult, NaiveDateTime, TimeZone, Utc};
use serde::de::{self, Visitor};
use serde::{Deserializer, Serializer};
use std::fmt;

fn jst_offset() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).expect("valid JST offset")
}

fn format_ruby_time(value: &DateTime<Utc>) -> String {
    value
        .with_timezone(&jst_offset())
        .format("%Y-%m-%d %H:%M:%S.%f %:z")
        .to_string()
}

fn parse_ruby_time(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(ts) = value.parse::<i64>() {
        return DateTime::from_timestamp(ts, 0);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f %:z",
        "%Y-%m-%d %H:%M:%S%.f %z",
        "%Y-%m-%d %H:%M:%S %:z",
        "%Y-%m-%d %H:%M:%S %z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(value, fmt) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(value, fmt) {
            return match jst_offset().from_local_datetime(&dt) {
                LocalResult::Single(local) | LocalResult::Ambiguous(local, _) => {
                    Some(local.with_timezone(&Utc))
                }
                LocalResult::None => None,
            };
        }
    }
    None
}

pub fn serialize<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&format_ruby_time(value))
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(RubyTimeVisitor)
}

struct RubyTimeVisitor;

impl<'de> Visitor<'de> for RubyTimeVisitor {
    type Value = DateTime<Utc>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a Ruby/narou.rb timestamp string or Unix timestamp")
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        DateTime::from_timestamp(value, 0)
            .ok_or_else(|| E::custom(format!("invalid timestamp: {value}")))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let value = i64::try_from(value)
            .map_err(|_| E::custom(format!("timestamp is too large: {value}")))?;
        self.visit_i64(value)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        parse_ruby_time(value)
            .ok_or_else(|| E::custom(format!("invalid Ruby timestamp: {value}")))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&value)
    }
}

pub mod option {
    use super::*;

    pub fn serialize<S>(
        value: &Option<DateTime<Utc>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(value) => serializer.serialize_some(&format_ruby_time(value)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_option(OptionalRubyTimeVisitor)
    }

    struct OptionalRubyTimeVisitor;

    impl<'de> Visitor<'de> for OptionalRubyTimeVisitor {
        type Value = Option<DateTime<Utc>>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("null, a Ruby/narou.rb timestamp string, or Unix timestamp")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            super::deserialize(deserializer).map(Some)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, Serialize)]
    struct RequiredTime {
        #[serde(with = "super")]
        value: DateTime<Utc>,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct OptionalTime {
        #[serde(with = "super::option", default)]
        value: Option<DateTime<Utc>>,
    }

    #[test]
    fn parses_ruby_jst_timestamp_as_utc_instant() {
        let parsed: RequiredTime =
            serde_yaml::from_str("value: 2016-06-19 16:24:51.761484000 +09:00\n")
                .unwrap();

        assert_eq!(parsed.value.year(), 2016);
        assert_eq!(parsed.value.month(), 6);
        assert_eq!(parsed.value.day(), 19);
        assert_eq!(parsed.value.hour(), 7);
        assert_eq!(parsed.value.minute(), 24);
        assert_eq!(parsed.value.second(), 51);
    }

    #[test]
    fn parses_legacy_rust_epoch_seconds() {
        let parsed: RequiredTime = serde_yaml::from_str("value: 1466321091\n").unwrap();

        assert_eq!(parsed.value.timestamp(), 1466321091);
    }

    #[test]
    fn serializes_as_narou_rb_jst_timestamp() {
        let value = RequiredTime {
            value: Utc.with_ymd_and_hms(2016, 6, 19, 7, 24, 51).unwrap(),
        };

        let yaml = serde_yaml::to_string(&value).unwrap();

        assert!(yaml.contains("value: 2016-06-19 16:24:51.000000000 +09:00"));
    }

    #[test]
    fn option_accepts_null() {
        let parsed: OptionalTime = serde_yaml::from_str("value:\n").unwrap();

        assert!(parsed.value.is_none());
    }
}
