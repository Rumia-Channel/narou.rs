//! native の Web サーバー (`src/web/*`) と Worker (`worker_entry/src/webui/*`)
//! が共有する Web UI ヘルパ (入力バリデーション・ソート状態・表示用の小道具)。
//!
//! ここにある関数は serde / `crate::db` の純粋な型と関数だけに依存し、HTTP
//! フレームワーク・ファイルシステム・インベントリへは触れない。native 側は
//! `src/web/mod.rs` / `src/web/sort_state.rs` が同じ名前で再エクスポートし、
//! Worker 側は `narou_rs::application::webui` から直接呼ぶ。

use crate::db::{NovelRecord, compare_records_by_key, sort_keys};

// ---------------------------------------------------------------------------
// リクエスト件数・長さの上限 (native `src/web/mod.rs` の定数の唯一の定義)
// ---------------------------------------------------------------------------

/// 1 リクエストあたりのターゲット/ID 件数上限
/// (`server-max-targets-per-request` 設定が無い/不正なときのフォールバック値)。
pub const MAX_WEB_TARGETS_PER_REQUEST: usize = 100_000;
/// 1 リクエストあたりのタグ件数上限。
pub const MAX_WEB_TAGS_PER_REQUEST: usize = 128;
/// 1 ターゲットのバイト長上限。
pub const MAX_WEB_TARGET_LENGTH: usize = 4096;
/// 1 タグ名のバイト長上限。
pub const MAX_WEB_TAG_LENGTH: usize = 255;

// ---------------------------------------------------------------------------
// 入力バリデーション (エラー文言は native のハンドラがそのまま応答に載せる)
// ---------------------------------------------------------------------------

/// Web UI のダウンロード/更新/変換対象文字列を検証し、トリム済みの値を返す。
pub fn validate_web_target_value(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("target is required".to_string());
    }
    if trimmed.len() > MAX_WEB_TARGET_LENGTH {
        return Err("target is too long".to_string());
    }
    if trimmed.starts_with('-') {
        return Err("invalid target".to_string());
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err("target contains invalid characters".to_string());
    }
    Ok(trimmed.to_string())
}

/// Web UI のタグ名を検証し、トリム済みの値を返す。
pub fn validate_web_tag_name(tag: &str) -> Result<String, String> {
    let trimmed = tag.trim();
    if trimmed.is_empty() {
        return Err("tag is required".to_string());
    }
    if trimmed.starts_with('-') {
        return Err("tag contains invalid characters".to_string());
    }
    if trimmed.len() > MAX_WEB_TAG_LENGTH {
        return Err("tag is too long".to_string());
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err("tag contains invalid characters".to_string());
    }
    Ok(trimmed.to_string())
}

/// トリム後 `tag:` プレフィックスを剥がして `validate_web_tag_name` と同じ
/// 検証を通す。
pub fn normalize_web_tag_name(tag: &str) -> Result<String, String> {
    let trimmed = tag.trim();
    let stripped = trimmed.strip_prefix("tag:").unwrap_or(trimmed);
    validate_web_tag_name(stripped)
}

/// Web UI のデバイス指定を受理可能なキーへ正規化する。
pub fn normalize_web_device_override(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let normalized = value.to_ascii_lowercase();
    match normalized.as_str() {
        "text" | "kindle" | "kobo" | "epub" | "ibunko" | "reader" | "ibooks" => {
            Ok(Some(normalized))
        }
        _ => Err("invalid device".to_string()),
    }
}

/// 数値・文字列・その他混在の JSON 値を文字列ターゲットに変換する。
pub fn targets_to_strings(targets: &[serde_json::Value]) -> Vec<String> {
    targets
        .iter()
        .map(|value| match value {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 機械可読エラー応答ボディ (`{error: {code, message?}}`)
// ---------------------------------------------------------------------------

/// `{error: {code, message?}}` 形の JSON ボディ。各ランタイムの `json_error`
/// 応答ヘルパ (axum / worker) はこれをそれぞれの Response 型に載せるだけに
/// する。`serde_json::Map` はキー順ソートなので `code` → `message` の順に
/// 直列化される。
pub fn json_error_body(code: &str, message: Option<&str>) -> serde_json::Value {
    match message {
        Some(message) => serde_json::json!({ "error": { "code": code, "message": message } }),
        None => serde_json::json!({ "error": { "code": code } }),
    }
}

// ---------------------------------------------------------------------------
// タグ一覧/履歴の HTML 断片で使う表示用ヘルパ
// ---------------------------------------------------------------------------

/// HTML テキストに埋め込むための最小限のエスケープ。
pub fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// タグ色名から Web UI の CSS クラス名を引く。未知の色は既定クラス。
pub fn tag_color_class(color: &str) -> &'static str {
    match color {
        "green" => "tag-green",
        "yellow" => "tag-yellow",
        "blue" => "tag-blue",
        "magenta" => "tag-magenta",
        "cyan" => "tag-cyan",
        "red" => "tag-red",
        "white" => "tag-white",
        _ => "tag-default",
    }
}

// ---------------------------------------------------------------------------
// サーバー側ソート状態 (native `src/web/sort_state.rs` の共有部分)
// ---------------------------------------------------------------------------

/// `server_setting` / 設定ストア (global スコープ) 内のソート状態キー名。
pub const CURRENT_SORT_KEY: &str = "current_sort";

/// `current_sort` 未保存・形式不正時の既定ソート。
pub const DEFAULT_CURRENT_SORT_COLUMN: usize = 2;
pub const DEFAULT_CURRENT_SORT_DIR: &str = "desc";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentSortState {
    pub column: usize,
    pub dir: String,
}

impl CurrentSortState {
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "column": self.column,
            "dir": self.dir,
        })
    }

    pub fn to_yaml_value(&self) -> serde_yaml::Value {
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(
            serde_yaml::Value::String("column".to_string()),
            serde_yaml::to_value(self.column).expect("serialize sort column"),
        );
        mapping.insert(
            serde_yaml::Value::String("dir".to_string()),
            serde_yaml::Value::String(self.dir.clone()),
        );
        serde_yaml::Value::Mapping(mapping)
    }
}

pub fn default_current_sort_state() -> CurrentSortState {
    CurrentSortState {
        column: DEFAULT_CURRENT_SORT_COLUMN,
        dir: DEFAULT_CURRENT_SORT_DIR.to_string(),
    }
}

/// `server_setting` マップから `current_sort` キーを取り出して正規化する。
pub fn current_sort_from_server_setting(
    server_setting: &serde_yaml::Value,
) -> Option<CurrentSortState> {
    server_setting
        .as_mapping()?
        .get(serde_yaml::Value::String(CURRENT_SORT_KEY.to_string()))
        .and_then(normalize_current_sort_value)
}

/// リクエスト JSON 本体をそのままソート状態として正規化する。
pub fn normalize_current_sort_request(body: &serde_json::Value) -> Option<CurrentSortState> {
    let value = serde_yaml::to_value(body).ok()?;
    normalize_current_sort_value(&value)
}

/// 受理形式: `column`/`dir` キー (Ruby シンボル由来の `:column`/`:dir` も
/// 許容)、列は番号または数字文字列、方向は `asc`/`desc` (先頭 `:` は剥がす)。
/// `column` は `sort_keys()` の既知のインデックスだけを受理する。
pub fn normalize_current_sort_value(sort_state: &serde_yaml::Value) -> Option<CurrentSortState> {
    let sort_state = sort_state.as_mapping()?;
    let column = sort_state
        .get(serde_yaml::Value::String("column".to_string()))
        .or_else(|| sort_state.get(serde_yaml::Value::String(":column".to_string())))
        .and_then(normalize_sort_column)?;
    let dir = sort_state
        .get(serde_yaml::Value::String("dir".to_string()))
        .or_else(|| sort_state.get(serde_yaml::Value::String(":dir".to_string())))
        .and_then(normalize_sort_dir)?;
    Some(CurrentSortState { column, dir })
}

fn normalize_sort_column(value: &serde_yaml::Value) -> Option<usize> {
    let column = match value {
        serde_yaml::Value::Number(number) => number.as_u64().map(|value| value as usize)?,
        serde_yaml::Value::String(text) if text.chars().all(|ch| ch.is_ascii_digit()) => {
            text.parse::<usize>().ok()?
        }
        _ => return None,
    };
    sort_keys().get(column).map(|_| column)
}

fn normalize_sort_dir(value: &serde_yaml::Value) -> Option<String> {
    let text = match value {
        serde_yaml::Value::String(text) => text.as_str(),
        _ => return None,
    };
    let text = text.trim_start_matches(':');
    match text {
        "asc" | "desc" => Some(text.to_string()),
        _ => None,
    }
}

/// ソート状態の列インデックスを `sort_keys()` のキー名へ写す。
pub fn sort_column_key(sort_state: &CurrentSortState) -> Option<&'static str> {
    sort_keys().get(sort_state.column).copied()
}

/// ソート状態の列インデックスを表示ラベルへ写す。
pub fn sort_column_label(sort_state: &CurrentSortState) -> Option<&'static str> {
    crate::application::settings_view::SORT_COLUMN_LABELS
        .get(sort_state.column)
        .copied()
}

/// 主キーが Equal のとき id で安定化し、`desc` なら反転する。未知キーは
/// `compare_records_by_key` と同じく id 比較へフォールバックする。
pub fn sort_records(records: &mut [NovelRecord], sort_state: &CurrentSortState) {
    let sort_key = sort_column_key(sort_state).unwrap_or("id");
    let reverse = sort_state.dir == "desc";
    records.sort_by(|a, b| {
        // 安定ソート: 同じ general_lastup / length / タグなどを共有する
        // レコード間の順序が db::sort_by と一致するよう id で決着する。
        let ordering = sort_record_ordering(a, b, sort_key).then_with(|| a.id.cmp(&b.id));
        if reverse {
            ordering.reverse()
        } else {
            ordering
        }
    });
}

/// `db::compare_records_by_key` の薄いラッパ。BUG-9 で導入された型付き +
/// None 安定順のセマンティクス (`compare_optional` を経由) をそのまま使う。
pub fn sort_record_ordering(
    a: &NovelRecord,
    b: &NovelRecord,
    sort_key: &str,
) -> std::cmp::Ordering {
    compare_records_by_key(a, b, sort_key)
}

/// 選択された id を `sort_state` で並べ直す。選択に無い id (= 存在しない
/// レコード) は捨てる。
pub fn sort_ids_from_records(
    ids: &[i64],
    records: &[NovelRecord],
    sort_state: &CurrentSortState,
) -> Vec<i64> {
    let selected = ids.iter().copied().collect::<std::collections::HashSet<_>>();
    let mut records = records
        .iter()
        .filter(|record| selected.contains(&record.id))
        .cloned()
        .collect::<Vec<_>>();
    sort_records(&mut records, sort_state);
    records.into_iter().map(|record| record.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn record(id: i64, general_lastup_ts: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: format!("author-{id}"),
            title: format!("title-{id}"),
            file_title: format!("file-{id}"),
            toc_url: format!("https://example.com/{id}/"),
            sitename: "site".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.timestamp_opt(1_700_000_000 + id, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: Some(Utc.timestamp_opt(general_lastup_ts, 0).unwrap()),
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: Some(id),
            length: Some(id),
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    // -- validate_web_target_value ------------------------------------------

    #[test]
    fn target_value_trims_and_accepts() {
        assert_eq!(validate_web_target_value("  n1234ab  "), Ok("n1234ab".to_string()));
        assert_eq!(validate_web_target_value("42"), Ok("42".to_string()));
    }

    #[test]
    fn target_value_rejects_with_native_messages() {
        assert_eq!(validate_web_target_value(""), Err("target is required".to_string()));
        assert_eq!(
            validate_web_target_value("   "),
            Err("target is required".to_string())
        );
        assert_eq!(
            validate_web_target_value(&"x".repeat(MAX_WEB_TARGET_LENGTH + 1)),
            Err("target is too long".to_string())
        );
        assert_eq!(
            validate_web_target_value("-f"),
            Err("invalid target".to_string())
        );
        assert_eq!(
            validate_web_target_value("bad\ttarget"),
            Err("target contains invalid characters".to_string())
        );
        // 長さ判定はフラグ判定より先: 長すぎる `-` 始まりは "too long"。
        assert_eq!(
            validate_web_target_value(&format!("-{}", "x".repeat(MAX_WEB_TARGET_LENGTH))),
            Err("target is too long".to_string())
        );
    }

    // -- validate_web_tag_name / normalize_web_tag_name ---------------------

    #[test]
    fn tag_name_rejects_with_native_messages() {
        assert_eq!(validate_web_tag_name(""), Err("tag is required".to_string()));
        assert_eq!(
            validate_web_tag_name("  "),
            Err("tag is required".to_string())
        );
        // `-` 始まりは長さ判定より先に拒否される。
        assert_eq!(
            validate_web_tag_name(&format!("-{}", "x".repeat(MAX_WEB_TAG_LENGTH))),
            Err("tag contains invalid characters".to_string())
        );
        assert_eq!(
            validate_web_tag_name(&"x".repeat(MAX_WEB_TAG_LENGTH + 1)),
            Err("tag is too long".to_string())
        );
        assert_eq!(
            validate_web_tag_name("bad\ntag"),
            Err("tag contains invalid characters".to_string())
        );
        assert_eq!(validate_web_tag_name(" 旅行 "), Ok("旅行".to_string()));
    }

    #[test]
    fn normalize_tag_name_strips_tag_prefix() {
        assert_eq!(normalize_web_tag_name("tag:旅行"), Ok("旅行".to_string()));
        assert_eq!(normalize_web_tag_name(" tag:旅行 "), Ok("旅行".to_string()));
        assert_eq!(normalize_web_tag_name("旅行"), Ok("旅行".to_string()));
        assert_eq!(
            normalize_web_tag_name("tag:"),
            Err("tag is required".to_string())
        );
    }

    // -- normalize_web_device_override --------------------------------------

    #[test]
    fn device_override_normalizes() {
        assert_eq!(normalize_web_device_override(None), Ok(None));
        assert_eq!(normalize_web_device_override(Some("")), Ok(None));
        assert_eq!(normalize_web_device_override(Some("  ")), Ok(None));
        assert_eq!(
            normalize_web_device_override(Some("Kindle")),
            Ok(Some("kindle".to_string()))
        );
        for device in ["text", "kobo", "epub", "ibunko", "reader", "ibooks"] {
            assert_eq!(
                normalize_web_device_override(Some(device)),
                Ok(Some(device.to_string()))
            );
        }
        assert_eq!(
            normalize_web_device_override(Some("ipad")),
            Err("invalid device".to_string())
        );
    }

    // -- targets_to_strings ---------------------------------------------------

    #[test]
    fn targets_to_strings_converts_mixed_values() {
        let targets = vec![
            serde_json::json!(42),
            serde_json::json!("n1234ab"),
            serde_json::json!(true),
            serde_json::json!(null),
        ];
        assert_eq!(
            targets_to_strings(&targets),
            vec![
                "42".to_string(),
                "n1234ab".to_string(),
                "true".to_string(),
                "null".to_string()
            ]
        );
    }

    // -- json_error_body ------------------------------------------------------

    #[test]
    fn error_body_shape_is_fixed() {
        assert_eq!(
            json_error_body("not_found", Some("missing")),
            serde_json::json!({ "error": { "code": "not_found", "message": "missing" } })
        );
        assert_eq!(
            json_error_body("not_found", None),
            serde_json::json!({ "error": { "code": "not_found" } })
        );
    }

    // -- html_escape / tag_color_class ----------------------------------------

    #[test]
    fn html_escape_covers_all_entities() {
        assert_eq!(html_escape("<a & \"b\">"), "&lt;a &amp; &quot;b&quot;&gt;");
    }

    #[test]
    fn tag_color_class_maps_known_colors() {
        assert_eq!(tag_color_class("green"), "tag-green");
        assert_eq!(tag_color_class("white"), "tag-white");
        assert_eq!(tag_color_class("unknown"), "tag-default");
    }

    // -- current sort state ----------------------------------------------------

    #[test]
    fn default_sort_state_is_general_lastup_desc() {
        let state = default_current_sort_state();
        assert_eq!(state.column, 2);
        assert_eq!(state.dir, "desc");
        assert_eq!(sort_column_key(&state), Some("general_lastup"));
        assert_eq!(sort_column_label(&state), Some("最新話掲載日"));
    }

    #[test]
    fn sort_state_serializes_to_json_and_yaml() {
        let state = CurrentSortState {
            column: 4,
            dir: "asc".to_string(),
        };
        assert_eq!(
            state.to_json_value(),
            serde_json::json!({ "column": 4, "dir": "asc" })
        );
        let yaml = state.to_yaml_value();
        assert_eq!(
            yaml.get(serde_yaml::Value::String("column".to_string())),
            Some(&serde_yaml::Value::Number(4.into()))
        );
        assert_eq!(
            yaml.get(serde_yaml::Value::String("dir".to_string())),
            Some(&serde_yaml::Value::String("asc".to_string()))
        );
    }

    #[test]
    fn normalize_sort_value_accepts_canonical_and_ruby_symbol_forms() {
        let yaml: serde_yaml::Value =
            serde_yaml::from_str("column: 2\ndir: desc\n").unwrap();
        assert_eq!(
            normalize_current_sort_value(&yaml),
            Some(CurrentSortState {
                column: 2,
                dir: "desc".to_string()
            })
        );

        // Ruby シンボル由来の `:column` / `:dir` と、方向の先頭 `:`。
        let symbol_form: serde_yaml::Value =
            serde_yaml::from_str("\":column\": \"4\"\n\":dir\": \":asc\"\n").unwrap();
        assert_eq!(
            normalize_current_sort_value(&symbol_form),
            Some(CurrentSortState {
                column: 4,
                dir: "asc".to_string()
            })
        );
    }

    #[test]
    fn normalize_sort_value_rejects_invalid_inputs() {
        for yaml in [
            serde_yaml::Value::Null,
            serde_yaml::from_str("[]").unwrap(),
            serde_yaml::from_str("column: 2").unwrap(),          // dir 無し
            serde_yaml::from_str("dir: asc").unwrap(),           // column 無し
            serde_yaml::from_str("column: -1\ndir: asc").unwrap(), // 負数
            serde_yaml::from_str("column: \"a\"\ndir: asc").unwrap(), // 非数字文字列
            serde_yaml::from_str("column: 999\ndir: asc").unwrap(),  // 範囲外
            serde_yaml::from_str("column: 2\ndir: sideways").unwrap(), // 不正な向き
            serde_yaml::from_str("column: 2\ndir: 1").unwrap(),        // dir が非文字列
        ] {
            assert_eq!(normalize_current_sort_value(&yaml), None, "yaml: {yaml:?}");
        }
    }

    #[test]
    fn normalize_sort_request_parses_json_body() {
        assert_eq!(
            normalize_current_sort_request(&serde_json::json!({"column": 2, "dir": "desc"})),
            Some(CurrentSortState {
                column: 2,
                dir: "desc".to_string()
            })
        );
        assert_eq!(
            normalize_current_sort_request(&serde_json::json!({"column": "4", "dir": "asc"})),
            Some(CurrentSortState {
                column: 4,
                dir: "asc".to_string()
            })
        );
        assert_eq!(
            normalize_current_sort_request(&serde_json::json!({"column": 999, "dir": "asc"})),
            None
        );
        assert_eq!(normalize_current_sort_request(&serde_json::json!(null)), None);
    }

    #[test]
    fn current_sort_from_server_setting_reads_current_sort_key() {
        let setting: serde_yaml::Value =
            serde_yaml::from_str("current_sort:\n  column: 4\n  dir: asc\n").unwrap();
        assert_eq!(
            current_sort_from_server_setting(&setting),
            Some(CurrentSortState {
                column: 4,
                dir: "asc".to_string()
            })
        );
        let empty: serde_yaml::Value = serde_yaml::from_str("other: 1\n").unwrap();
        assert_eq!(current_sort_from_server_setting(&empty), None);
    }

    // -- sort_records / sort_ids_from_records --------------------------------

    #[test]
    fn sort_records_orders_by_column_then_id() {
        // column 2 = general_lastup。同値は id 昇順で安定化。
        let mut records = vec![
            record(3, 1_700_000_200),
            record(1, 1_700_000_300),
            record(2, 1_700_000_200),
        ];
        sort_records(
            &mut records,
            &CurrentSortState {
                column: 2,
                dir: "asc".to_string(),
            },
        );
        assert_eq!(
            records.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2, 3, 1]
        );
    }

    #[test]
    fn sort_records_desc_reverses_full_order() {
        let mut records = vec![
            record(3, 1_700_000_200),
            record(1, 1_700_000_300),
            record(2, 1_700_000_200),
        ];
        sort_records(
            &mut records,
            &CurrentSortState {
                column: 2,
                dir: "desc".to_string(),
            },
        );
        assert_eq!(
            records.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 3, 2]
        );
    }

    #[test]
    fn sort_records_unknown_column_falls_back_to_id() {
        let state = CurrentSortState {
            column: usize::MAX,
            dir: "asc".to_string(),
        };
        let mut records = vec![record(3, 1), record(1, 2)];
        sort_records(&mut records, &state);
        assert_eq!(
            records.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn sort_ids_from_records_filters_and_orders() {
        let records = vec![
            record(1, 1_700_000_100),
            record(2, 1_700_000_300),
            record(3, 1_700_000_200),
        ];
        let state = CurrentSortState {
            column: 2,
            dir: "asc".to_string(),
        };
        // 選択に無い 99 と、records に無い id は捨てる。
        assert_eq!(sort_ids_from_records(&[3, 1, 99], &records, &state), vec![1, 3]);
        // 既定 desc 状態では新しい順。
        let desc = default_current_sort_state();
        assert_eq!(sort_ids_from_records(&[1, 2, 3], &records, &desc), vec![2, 3, 1]);
    }
}
