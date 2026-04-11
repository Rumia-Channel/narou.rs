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
    }

    deserializer.deserialize_any(YesNoBoolVisitor)
}
