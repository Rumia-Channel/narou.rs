//! 更新操作が他のクライアントへ配るイベントの選択 (native `PushServer` /
//! Worker `PushHub` で共有)。
//!
//! native の各ハンドラに散っていた `table.reload` / `tag.updateCanvas` /
//! `notepad.change` の選択をここに集約する。イベントの形は
//! `application::push_events`、送信 (broadcast / best-effort) は各ランタイムの
//! トランスポートに残す。凍結・タグ操作・削除は一覧とタグ表示を更新する。

use serde_json::Value;

use crate::application::push_events::{event, table_reload, tag_update_canvas};

use super::notepad::notepad_response_value;

/// 変更内容に対応するイベントの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationNotice<'a> {
    /// 一覧の表示内容だけが変わった。
    TableReload,
    /// 一覧とタグ表示の両方が変わった (凍結・タグ操作・削除)。
    TableAndTags,
    /// メモ帳が保存された。`content` は保存後の本文。
    Notepad { content: &'a str },
}

/// 選択された種類に対応するイベント列。native の `broadcast_raw` と Worker の
/// `PushHubClient::broadcast_best_effort` はこの列をそのまま送る。
pub fn mutation_events(notice: MutationNotice<'_>) -> Vec<Value> {
    match notice {
        MutationNotice::TableReload => vec![table_reload()],
        MutationNotice::TableAndTags => vec![table_reload(), tag_update_canvas()],
        MutationNotice::Notepad { content } => {
            vec![event("notepad.change", notepad_response_value(content))]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn tag_changes_reload_both_table_and_tag_canvas() {
        assert_eq!(
            mutation_events(MutationNotice::TableAndTags),
            vec![
                serde_json::json!({ "type": "table.reload", "data": "" }),
                serde_json::json!({ "type": "tag.updateCanvas", "data": "" }),
            ]
        );
    }

    #[test]
    fn notepad_changes_push_the_saved_snapshot_only() {
        let events = mutation_events(MutationNotice::Notepad { content: "memo" });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "notepad.change");
        assert_eq!(events[0]["data"]["content"], "memo");
        assert_eq!(events[0]["data"]["text"], "memo");
        assert_eq!(
            events[0]["data"]["object_id"],
            super::notepad_response_value("memo")["object_id"]
        );
    }
}
