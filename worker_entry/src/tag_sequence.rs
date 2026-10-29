//! タグの登録順の D1 実装。
//!
//! 保存形式は native と同じ `app_state(scope='inv', key='tag_sequence')`。
//! `value_yaml` は YAML 管理への書き戻し互換のため、`value_json` と同じ
//! 内容を YAML でも保存する。
//!
//! native の `src/native/tag_sequence.rs` と同じ形式・規則を使う。

use narou_rs::application::tag_sequence::{LibraryTags, TagSequence};
use narou_rs::error::{NarouError, Result};
use serde::Deserialize;
use worker::wasm_bindgen::JsValue;

use crate::db_handle::DbHandle;

const SELECT_SQL: &str =
    "SELECT value_json, value_yaml FROM app_state WHERE scope='inv' AND key='tag_sequence'";
const SAVE_SQL: &str = r#"
INSERT INTO app_state(scope,key,value_json,value_yaml)
SELECT 'inv','tag_sequence',?1,?2
WHERE ?3 IS NULL OR EXISTS(SELECT 1 FROM app_state WHERE scope='inv' AND key='tag_sequence' AND value_json IS ?3)
ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml
WHERE app_state.value_json IS ?3
RETURNING key
"#;

/// ライブラリに存在するタグと、そのタグが現れる最小小説 ID。
const LIBRARY_TAGS_SQL: &str = r#"
SELECT t.tag AS tag, MIN(t.novel_id) AS novel_id FROM novel_tags t
INNER JOIN novels n ON n.id = t.novel_id
GROUP BY t.tag COLLATE BINARY
"#;

#[derive(Deserialize)]
struct Row {
    #[serde(default)]
    value_json: Option<String>,
    #[serde(default)]
    value_yaml: Option<String>,
}

#[derive(Deserialize)]
struct KeyRow {
    key: String,
}

#[derive(Deserialize)]
struct TagRow {
    tag: String,
    novel_id: i64,
}

fn db_error(value: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(value.to_string())
}

/// タグの登録順を読む。未保存なら空。
///
/// native は `value_yaml` に YAML を書き (`value_json` は `{}`)、Worker は
/// `value_json` に JSON を書く。どちらの形式でも復元できるようにする。
pub(crate) async fn read(db: &DbHandle) -> Result<TagSequence> {
    let row = db
        .prepare(SELECT_SQL)
        .first::<Row>(None)
        .await
        .map_err(db_error)?;
    Ok(row.as_ref().map(decode_row).unwrap_or_default())
}

/// 保存された行から登録順を復元する。
fn decode_row(row: &Row) -> TagSequence {
    if let Some(json) = row.value_json.as_deref()
        && let Ok(sequence) = serde_json::from_str::<TagSequence>(json)
        && sequence.is_initialized()
    {
        return sequence;
    }
    if let Some(yaml) = row.value_yaml.as_deref()
        && let Ok(sequence) = serde_yaml::from_str::<TagSequence>(yaml)
    {
        return sequence;
    }
    TagSequence::default()
}

/// D1 ハンドルからタグの登録順を読む。
///
/// タグ一覧の表示順 (登録順) で使う。読み取り専用の経路なので UI セッションの
/// ハンドルでよい。
pub(crate) async fn load_with_db(db: &DbHandle) -> Result<TagSequence> {
    read(db).await
}

/// ライブラリに存在するタグ (最小小説 ID 付き) を読む。
///
/// 既存ライブラリの登録順をバックフィルするときの材料になる。
pub(crate) async fn library_tags(db: &DbHandle) -> Result<LibraryTags> {
    let result = db
        .prepare(LIBRARY_TAGS_SQL)
        .all()
        .await
        .map_err(db_error)?;
    let rows = result.results::<TagRow>().map_err(db_error)?;
    Ok(rows
        .into_iter()
        .filter(|row| !narou_rs::virtual_novel::is_internal_tag(&row.tag))
        .map(|row| (row.tag, row.novel_id))
        .collect())
}

/// スナップショット CAS で全置換する。競合したら `false`。
async fn write_if_unchanged(
    db: &DbHandle,
    expected: Option<&str>,
    state: &TagSequence,
) -> Result<bool> {
    let encoded = serde_json::to_string(state).map_err(db_error)?;
    let yaml = serde_yaml::to_string(state)?;
    let previous = match expected {
        Some(value) => value.to_string().into(),
        None => JsValue::NULL,
    };
    let rows = db
        .prepare(SAVE_SQL)
        .bind(&[encoded.into(), yaml.into(), previous])
        .map_err(db_error)?
        .all()
        .await
        .map_err(db_error)?
        .results::<KeyRow>()
        .map_err(db_error)?;
    Ok(rows.iter().any(|row| row.key == "tag_sequence"))
}

/// 小説の書き込みでタグの登録順を進める。
///
/// `before` は書き込み前のライブラリのタグ (登録順が未初期化のときの
/// バックフィルに使う)、`written` は書き込み対象の小説に付いたタグ。
/// 同時更新はスナップショット CAS で検出して再試行する。
pub(crate) async fn sync_with_db(
    db: &DbHandle,
    before: Option<&LibraryTags>,
    written: &[&str],
) -> Result<TagSequence> {
    for _ in 0..8 {
        let row = db
            .prepare(SELECT_SQL)
            .first::<Row>(None)
            .await
            .map_err(db_error)?;
        let current = row.as_ref().map(decode_row).unwrap_or_default();
        let mut next = current.clone();
        if !next.advance(before, written.iter().copied()) {
            return Ok(current);
        }
        // CAS は行の生のスナップショットで比較する (native も value_json が
        // `{}` なので、復元した値ではなく生の値を比較する)。
        let expected = row.as_ref().and_then(|row| row.value_json.clone());
        if write_if_unchanged(db, expected.as_deref(), &next).await? {
            return Ok(next);
        }
    }
    Err(db_error("タグの登録順が同時に更新されています。再試行してください"))
}
