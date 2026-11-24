use serde::de;

pub fn deserialize_yes_no_bool<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct YesNoBoolVisitor;

    impl<'de> de::Visitor<'de> for YesNoBoolVisitor {
        type Value = bool;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a boolean or a string \"yes\"/\"no\"")
        }

        fn visit_bool<E>(self, value: bool) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(value)
        }

        fn visit_str<E>(self, value: &str) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            match value.to_lowercase().as_str() {
                "yes" | "true" | "on" | "1" => Ok(true),
                "no" | "false" | "off" | "0" => Ok(false),
                _ => Err(de::Error::invalid_value(de::Unexpected::Str(value), &self)),
            }
        }

        // Psych の真理値判定に合わせる: 整数・浮動小数点 (0 含む) は真、
        // null は偽。`cookie_session: 0` や `1` で定義ごと落ちないようにする。
        fn visit_u64<E>(self, _value: u64) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(true)
        }

        fn visit_i64<E>(self, _value: i64) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(true)
        }

        fn visit_f64<E>(self, _value: f64) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(true)
        }

        fn visit_unit<E>(self) -> std::result::Result<bool, E>
        where
            E: de::Error,
        {
            Ok(false)
        }
    }

    deserializer.deserialize_any(YesNoBoolVisitor)
}

/// `Option<bool>` variant of [`deserialize_yes_no_bool`]: an absent value stays
/// `None`, a present one accepts the same yes/no dialect.
pub fn deserialize_yes_no_opt_bool<'de, D>(deserializer: D) -> std::result::Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    struct Helper(#[serde(deserialize_with = "deserialize_yes_no_bool")] bool);
    Ok(<Option<Helper> as serde::Deserialize>::deserialize(deserializer)?.map(|Helper(value)| value))
}
