//! メモ帳 (`/api/notepad/read` / `/api/notepad/save`) の共有規則。
//!
//! 本文の保存先 (native: StateDb / 旧ファイル、Worker: D1 `app_state`) と
//! 競合検知は各保存先の原子的な更新境界で行う。本文の復元、
//! `object_id` の計算・応答 payload・競合文言はここで共有する。

use std::borrow::Cow;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// 他の画面の更新で保存が競合したときの応答メッセージ (native/Worker 同一)。
pub const NOTEPAD_CONFLICT_MESSAGE: &str = "他の画面でメモ帳が更新されました。編集中の内容は保持しています。保存済みの内容と比較してからもう一度保存してください";

/// 本文の SHA-256 hex。楽観ロックの比較に使う `object_id`。
pub fn notepad_object_id(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

/// Restore legacy JSON-string notes without treating canonical raw text as YAML.
pub fn notepad_content_from_columns<'a>(raw: Option<&'a str>, json: Option<&str>) -> Cow<'a, str> {
    if raw.map(str::trim).is_none_or(|value| value.is_empty() || value == "{}")
        && let Some(json) = json
        && json.trim_start().starts_with('"')
        && let Ok(legacy) = serde_json::from_str::<String>(&json)
    {
        return Cow::Owned(legacy);
    }
    raw.map(Cow::Borrowed).unwrap_or_default()
}

/// `{content, text, object_id}` — 読み取り応答と保存成功/競合応答の共通部分。
pub fn notepad_response_value(content: &str) -> Value {
    serde_json::json!({
        "content": content,
        "text": content,
        "object_id": notepad_object_id(content),
    })
}

/// 保存成功の応答。
pub fn notepad_saved_payload(content: &str) -> Value {
    serde_json::json!({
        "success": true,
        "message": "Saved",
        "content": content,
        "text": content,
        "object_id": notepad_object_id(content),
    })
}

/// 競合の応答。`current_content` は保存されていた (競合相手の) 本文。
pub fn notepad_conflict_payload(current_content: &str) -> Value {
    serde_json::json!({
        "success": false,
        "conflict": true,
        "message": NOTEPAD_CONFLICT_MESSAGE,
        "content": current_content,
        "text": current_content,
        "object_id": notepad_object_id(current_content),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_id_is_sha256_hex_and_changes_with_content() {
        assert_eq!(
            notepad_object_id("memo"),
            "9c225a950b92172f8c2afe8b682b7b86ce8f835578b546f9b8070cba309ad314"
        );
        assert_ne!(notepad_object_id(""), notepad_object_id("memo"));
        assert_eq!(notepad_object_id("memo").len(), 64);
    }

    #[test]
    fn canonical_raw_notes_and_legacy_json_strings_remain_distinct() {
        assert_eq!(notepad_content_from_columns(Some("{}".into()), Some("{}".into())), "{}");
        assert_eq!(notepad_content_from_columns(Some(" \t\r\n".into()), Some("{}".into())), " \t\r\n");
        assert_eq!(notepad_content_from_columns(Some("{}".into()), Some(r#""old note""#.into())), "old note");
        assert_eq!(notepad_content_from_columns(Some(""), Some("{}")), "");
    }

}
