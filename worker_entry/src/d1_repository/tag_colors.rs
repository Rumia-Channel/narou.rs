use narou_rs::application::tag_colors::TagColors;
use narou_rs::db::novel_codec::NovelBind;
use narou_rs::error::{NarouError, Result};
use serde::Deserialize;

pub(crate) const READ_SQL: &str =
    "SELECT scope, value_yaml, value_json FROM app_state
     WHERE (scope = 'inv' AND key = 'tag_colors')
        OR (scope = 'tag_colors' AND key = 'colors')";

// Presence guards distinguish an absent row from a present row with NULL
// columns. Each raw column participates, even when YAML takes precedence.
pub(crate) const UPDATE_SQL: &str =
    "INSERT INTO app_state(scope, key, value_yaml, value_json)
     SELECT 'inv', 'tag_colors', ?1, '{}'
     WHERE CASE WHEN ?2 THEN EXISTS(
         SELECT 1 FROM app_state WHERE scope = 'inv' AND key = 'tag_colors'
           AND value_yaml IS ?3 AND value_json IS ?4)
       ELSE NOT EXISTS(
         SELECT 1 FROM app_state WHERE scope = 'inv' AND key = 'tag_colors') END
       AND CASE WHEN ?5 THEN EXISTS(
         SELECT 1 FROM app_state WHERE scope = 'tag_colors' AND key = 'colors'
           AND value_yaml IS ?6 AND value_json IS ?7)
       ELSE NOT EXISTS(
         SELECT 1 FROM app_state WHERE scope = 'tag_colors' AND key = 'colors') END
     ON CONFLICT(scope, key) DO UPDATE SET
       value_yaml = excluded.value_yaml, value_json = '{}'
     RETURNING key";

// These two statements run in one ordered, transactional D1 batch. changes()
// is the preceding canonical CAS result, so a conflict cannot delete legacy.
pub(crate) const REMOVE_LEGACY_SQL: &str =
    "DELETE FROM app_state WHERE scope = 'tag_colors' AND key = 'colors'
       AND changes() > 0";

#[derive(Debug, Deserialize)]
pub(crate) struct TagColorRow {
    pub(crate) scope: String,
    pub(crate) value_yaml: Option<String>,
    pub(crate) value_json: Option<String>,
}

#[derive(Default)]
pub(crate) struct TagColorSnapshot {
    canonical: Option<TagColorRow>,
    legacy: Option<TagColorRow>,
}

impl TagColorSnapshot {
    pub(crate) fn from_rows(rows: impl IntoIterator<Item = TagColorRow>) -> Self {
        let mut snapshot = Self::default();
        for row in rows {
            match row.scope.as_str() {
                "inv" => snapshot.canonical = Some(row),
                "tag_colors" => snapshot.legacy = Some(row),
                _ => {}
            }
        }
        snapshot
    }

    pub(crate) fn colors(&self) -> Result<TagColors> {
        let canonical = self.canonical.as_ref().map(|row| {
            parse_tag_colors_row(row.value_yaml.as_deref(), row.value_json.as_deref())
        }).transpose()?.unwrap_or_default();
        if let Some(row) = &self.legacy {
            let mut legacy = parse_tag_colors_row(row.value_yaml.as_deref(), row.value_json.as_deref())?;
            legacy.merge(&canonical);
            Ok(legacy)
        } else {
            Ok(canonical)
        }
    }

    pub(crate) fn into_update_binds(self, value_yaml: String) -> Vec<NovelBind> {
        let mut binds = Vec::with_capacity(7);
        binds.push(NovelBind::Text(value_yaml));
        for row in [self.canonical, self.legacy] {
            binds.push(NovelBind::Int(i64::from(row.is_some())));
            let (yaml, json) = row.map(|row| (row.value_yaml, row.value_json))
                .unwrap_or_default();
            binds.push(yaml.map(NovelBind::Text).unwrap_or(NovelBind::Null));
            binds.push(json.map(NovelBind::Text).unwrap_or(NovelBind::Null));
        }
        binds
    }
}

pub(crate) fn parse_tag_colors_row(value_yaml: Option<&str>, value_json: Option<&str>) -> Result<TagColors> {
    if let Some(yaml) = value_yaml.filter(|value| !value.trim().is_empty() && value.trim() != "{}") {
        return parse_tag_colors_payload(yaml);
    }
    if let Some(json) = value_json.filter(|value| !value.trim().is_empty() && value.trim() != "{}") {
        return parse_tag_colors_payload(json);
    }
    if let Some(yaml) = value_yaml {
        return parse_tag_colors_payload(yaml);
    }
    Ok(TagColors::default())
}

pub(crate) fn parse_tag_colors_payload(payload: &str) -> Result<TagColors> {
    TagColors::from_yaml_payload(payload)
        .map_err(|error| NarouError::Platform(format!("invalid D1 tag colors: {error}")))
}
