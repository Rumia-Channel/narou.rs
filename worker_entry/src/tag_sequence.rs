//! タグの登録順の D1 実装。
//!
//! 保存形式は native と同じ `app_state(scope='inv', key='tag_sequence')`。
//! `value_yaml` は YAML 管理への書き戻し互換のため、`value_json` と同じ
//! 内容を YAML でも保存する。
//!
//! native の `src/native/tag_sequence.rs` と同じ形式・規則を使う。
//!
//! 連番表の更新は **小説の保存と同じ D1 batch** の中に入れる。batch は
//! 1 トランザクションなので、作品と順序が同時に確定し、片方だけ確定した
//! 状態にならない。

use narou_rs::application::tag_sequence::{LibraryTags, TagSequence};
use narou_rs::error::{NarouError, Result};
use serde::Deserialize;
use worker::wasm_bindgen::JsValue;
use worker::D1PreparedStatement;

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

/// CAS が外れたときに batch を中断する文。
///
/// 連番表が更新されていない (CAS 不一致) 場合に、同じ行へ二重に挿入して
/// 一意制約違反を起こす。batch は 1 トランザクションなので、作品も
/// ロールバックされる (作品だけ確定した状態を作らない)。
const GUARD_SQL: &str = r#"
INSERT INTO app_state(scope,key,value_json,value_yaml)
SELECT 'inv','tag_sequence',?1,?2
WHERE (SELECT value_json FROM app_state WHERE scope='inv' AND key='tag_sequence') IS NOT ?3
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
struct TagRow {
    tag: String,
    novel_id: i64,
}

fn db_error(value: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(value.to_string())
}

/// 読み取った連番表と、CAS の比較に使う生の値。
pub(crate) struct Snapshot {
    /// 復元した連番表。
    pub sequence: TagSequence,
    /// CAS で比較する `value_json` の値。行が無ければ `None`。
    pub raw_json: Option<String>,
}

/// タグの登録順を読む。未保存なら空。
///
/// native は `value_yaml` に YAML を書き (`value_json` は `{}`)、Worker は
/// `value_json` に JSON を書く。どちらの形式でも復元できるようにする。
pub(crate) async fn read_snapshot(db: &DbHandle) -> Result<Snapshot> {
    let row = db
        .prepare(SELECT_SQL)
        .first::<Row>(None)
        .await
        .map_err(db_error)?;
    Ok(match row {
        Some(row) => Snapshot {
            sequence: decode_row(&row),
            raw_json: row.value_json,
        },
        None => Snapshot {
            sequence: TagSequence::default(),
            raw_json: None,
        },
    })
}

/// D1 ハンドルからタグの登録順を読む。
///
/// タグ一覧の表示順 (登録順) で使う。読み取り専用の経路なので UI セッションの
/// ハンドルでよい。
pub(crate) async fn load_with_db(db: &DbHandle) -> Result<TagSequence> {
    Ok(read_snapshot(db).await?.sequence)
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

/// 連番表を CAS 付きで保存する文を作る。
pub(crate) fn upsert_statement(
    db: &DbHandle,
    expected: Option<&str>,
    state: &TagSequence,
) -> Result<D1PreparedStatement> {
    let encoded = serde_json::to_string(state).map_err(db_error)?;
    let yaml = serde_yaml::to_string(state)?;
    let previous = match expected {
        Some(value) => value.to_string().into(),
        None => JsValue::NULL,
    };
    db.prepare(SAVE_SQL)
        .bind(&[encoded.into(), yaml.into(), previous])
        .map_err(db_error)
}

/// CAS が外れたときに batch を中断する文を作る。
pub(crate) fn guard_statement(
    db: &DbHandle,
    state: &TagSequence,
) -> Result<D1PreparedStatement> {
    let encoded = serde_json::to_string(state).map_err(db_error)?;
    let yaml = serde_yaml::to_string(state)?;
    db.prepare(GUARD_SQL)
        .bind(&[encoded.clone().into(), yaml.into(), encoded.into()])
        .map_err(db_error)
}

/// 連番表を進める (次の連番表を返す ✓)。
///
/// `before` は書き込み前のライブラリのタグ (連番表が未初期化のときの
/// バックフィルに使う)、`written` は書き込み対象の小説に付いたタグ。
pub(crate) fn advance(
    snapshot: &Snapshot,
    before: Option<&LibraryTags>,
    written: &[&str],
) -> TagSequence {
    let mut next = snapshot.sequence.clone();
    next.advance(before, written.iter().copied());
    next
}
