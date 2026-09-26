//! Web UI の設定ページが読む JSON の組み立て。
//!
//! native の Web サーバー (`src/web/global_settings.rs`) と Worker
//! (`worker_entry/src/global_settings.rs`) が同じ JSON を返すために、ページが
//! 必要とする形（タブ・設定項目のメタデータ・置換設定）をここに集約する。
//! 依存は `SettingsService`（`SettingsStore` port の上）と `setting_core` /
//! `setting_info` の純関数だけで、Web フレームワークもファイルシステムも
//! 持ち込まない。

use crate::application::settings::{SettingEntry, SettingsEffect, SettingsService};
use crate::converter::device::Device;
use crate::setting_core::SettingScope;
use crate::setting_info::{
    original_setting_var_infos, setting_variables, tab_for_setting, webui_help_override, VarInfo,
    VarType,
};

/// 置換設定など、Web から受け取るテキスト入力の上限。
pub const MAX_WEB_TEXT_INPUT_BYTES: usize = 1024 * 1024;

/// 大きすぎる入力は拒否する（`replace.txt` など）。
pub fn validate_web_text_size(content: &str, limit: usize, label: &str) -> Result<(), String> {
    if content.len() > limit {
        return Err(format!("{label} is too large"));
    }
    Ok(())
}

/// Tab metadata matching narou.rb SETTING_TAB_NAMES / SETTING_TAB_INFO.
pub const TABS: &[(&str, &str, &str)] = &[
    ("general", "一般", ""),
    ("detail", "詳細", ""),
    ("webui", "WEB UI", "WEB UI 専用の設定です"),
    (
        "global",
        "Global",
        "Global な設定はユーザープロファイルに保存され、OSに関わらず適用されます",
    ),
    (
        "default",
        "default.*",
        "default.* 系の設定は個別の変換設定で未設定の項目の挙動を決めます",
    ),
    (
        "force",
        "force.*",
        "force.* 系の設定は個別設定、default.* 等の設定を無視して強制適用されます",
    ),
    (
        "command",
        "コマンド",
        "default_args.* 系の設定はコマンド実行時のオプションを省略した場合のデフォルト値を指定します",
    ),
    ("replace", "置換設定", ""),
    (
        "login",
        "ログイン",
        "ブラウザのある端末で取得したログイン情報を取り込みます",
    ),
];

/// Web UI の並び順カラムの表示ラベル。`crate::db::sort_keys()` と長さを揃え、
/// 同じインデックスで日本語ラベルを参照できるようにする。
pub const SORT_COLUMN_LABELS: &[&str] = &[
    "ID",             // 0  id
    "最終更新日",     // 1  last_update
    "最新話掲載日",   // 2  general_lastup
    "最終確認日",     // 3  last_check_date
    "タイトル",       // 4  title
    "作者",           // 5  author
    "サイト名",       // 6  sitename
    "小説種別",       // 7  novel_type
    "タグ",           // 8  tags
    "話数",           // 9  general_all_no
    "文字数",         // 10 length
    "状態",           // 11 status
    "URL",            // 12 toc_url
    "新着日",         // 13 new_arrivals_date
];

/// ソートキーの日本語ラベル。
pub fn sort_column_label_for_key(key: &str) -> Option<&'static str> {
    let index = crate::db::sort_keys()
        .iter()
        .position(|candidate| *candidate == key)?;
    SORT_COLUMN_LABELS.get(index).copied()
}

/// 設定ページのタブ一覧。
pub fn tabs_json() -> Vec<serde_json::Value> {
    TABS.iter()
        .map(|(id, label, info)| {
            serde_json::json!({
                "id": id,
                "label": label,
                "info": info,
            })
        })
        .collect()
}

/// 設定一覧の読み込みに失敗したときの応答（ページは `error` を表示する）。
pub fn error_view(message: String) -> serde_json::Value {
    serde_json::json!({
        "tabs": [],
        "settings": [],
        "replace_content": "",
        "error": message,
    })
}

/// `GET /api/global_setting` の応答を組み立てる。
pub async fn load_view(settings: &SettingsService) -> serde_json::Value {
    let entries = match settings.list().await {
        Ok(entries) => entries,
        Err(error) => return error_view(error.to_string()),
    };
    let mut items = Vec::new();
    let variables = setting_variables();
    let novel_vars = original_setting_var_infos();

    for entry in entries {
        let dynamic_prefix = entry
            .name
            .strip_prefix("default.")
            .map(|_| "default")
            .or_else(|| entry.name.strip_prefix("force.").map(|_| "force"));
        let tab = if let Some(prefix) = dynamic_prefix {
            Some(prefix)
        } else if entry.name.starts_with("default_args.") {
            Some("command")
        } else {
            tab_for_setting(&entry.name)
        };
        let Some(tab) = tab else {
            continue;
        };
        let scope = match entry.scope {
            SettingScope::Local => "local",
            SettingScope::Global => "global",
        };
        let info = if let Some(prefix) = dynamic_prefix {
            let base_name = entry
                .name
                .strip_prefix(&format!("{prefix}."))
                .unwrap_or(&entry.name);
            novel_vars
                .iter()
                .find(|(name, _)| *name == base_name)
                .map(|(_, info)| info.clone())
        } else {
            variables
                .local
                .iter()
                .find(|(name, _)| *name == entry.name)
                .or_else(|| {
                    variables
                        .global
                        .iter()
                        .find(|(name, _)| *name == entry.name)
                })
                .map(|(_, info)| info.clone())
        };
        let mut item = if let Some(info) = info {
            build_setting_entry(&entry.name, &info, scope, tab, entry.value.clone())
        } else {
            unknown_setting_entry(&entry, scope, tab)
        };
        if dynamic_prefix.is_some() && matches!(entry.var_type, VarType::Boolean) {
            item["three_way"] = serde_json::json!(true);
            item["invisible"] = serde_json::json!(false);
        }
        items.push(item);
    }

    let replace_content = settings.load_replace_content().await.unwrap_or_default();
    serde_json::json!({
        "tabs": tabs_json(),
        "settings": items,
        "replace_content": replace_content,
    })
}

/// メタデータが見つからない設定項目（`setting_info` に無い動的な名前）。
fn unknown_setting_entry(entry: &SettingEntry, scope: &str, tab: &str) -> serde_json::Value {
    serde_json::json!({
        "name": entry.name,
        "scope": scope,
        "tab": tab,
        "var_type": entry.var_type,
        "help": entry.help,
        "value": yaml_to_json(entry.value.clone()),
        "select_keys": entry.select_keys,
        "invisible": false,
    })
}

/// `POST /api/global_setting` の本文から保存対象を取り出す。
pub fn parse_changes(body: &serde_json::Value) -> Option<Vec<(String, serde_json::Value)>> {
    let entries = body["settings"].as_object()?;
    Some(
        entries
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    )
}

/// 保存を適用し、呼び出し側が処理すべき副作用を返す。
///
/// 失敗時は表示用のメッセージを返す（呼び出し側はそのまま応答に載せる）。
pub async fn apply_save(
    settings: &SettingsService,
    body: &serde_json::Value,
) -> Result<Vec<SettingsEffect>, String> {
    let Some(changes) = parse_changes(body) else {
        return Err("settings object required".to_string());
    };
    let effects = settings.apply_json(&changes).await.map_err(|e| e.to_string())?;

    if let Some(content) = body["replace_content"].as_str() {
        validate_web_text_size(content, MAX_WEB_TEXT_INPUT_BYTES, "replace.txt")?;
        settings
            .save_replace_content(content)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(effects)
}

/// 保存成功時のメッセージ（native / Worker で同じ文言）。
pub const SAVE_MESSAGE: &str = "設定を保存しました";

fn build_setting_entry(
    name: &str,
    info: &VarInfo,
    scope: &str,
    tab: &str,
    value: Option<serde_yaml::Value>,
) -> serde_json::Value {
    let help = webui_help_override(name, info.help).unwrap_or_else(|| info.help.to_string());
    serde_json::json!({
        "name": name,
        "scope": scope,
        "tab": tab,
        "var_type": info.var_type,
        "help": help,
        "value": yaml_to_json(value),
        "select_keys": info.select_keys,
        "select_summaries": select_summaries_for_setting(name, info),
        "invisible": false,
    })
}

fn select_summaries_for_setting(name: &str, info: &VarInfo) -> Option<Vec<String>> {
    let keys = info.select_keys.as_ref()?;
    let base_name = name
        .strip_prefix("default.")
        .or_else(|| name.strip_prefix("force."))
        .unwrap_or(name);
    Some(match base_name {
        "device" | "convert.multi-device" => keys
            .iter()
            .map(|key| key.parse::<Device>().unwrap_or(Device::Text).display_name().to_string())
            .collect(),
        "update.sort-by" => keys
            .iter()
            .map(|key| {
                sort_column_label_for_key(key)
                    .map(str::to_string)
                    .or_else(|| (key == "new_arrivals_date").then(|| "新着日".to_string()))
                    .unwrap_or_else(|| key.clone())
            })
            .collect(),
        "convert.copy-to-grouping" => vec![
            "端末毎にまとめる".to_string(),
            "掲載サイト毎にまとめる".to_string(),
        ],
        "economy" => vec![
            "変換後に作業ファイルを削除".to_string(),
            "送信後に書籍ファイルを削除".to_string(),
            "差分ファイルを保存しない".to_string(),
            "rawデータを保存しない".to_string(),
        ],
        "webui.table.reload-timing" => {
            vec!["１作品ごとに更新".to_string(), "キューごとに更新".to_string()]
        }
        "webui.performance-mode" => vec![
            "自動判定".to_string(),
            "常に有効".to_string(),
            "常に無効".to_string(),
        ],
        "convert.epub-font" => vec![
            "自動 (濁点注記のある小説だけ)".to_string(),
            "常に埋め込む".to_string(),
        ],
        "self-update.variant" => vec![
            "GPL版（AozoraEpub3_Lite 組込み）".to_string(),
            "通常版（外部 AozoraEpub3）".to_string(),
        ],
        "webui.new-tag-color" => vec![
            "自動 (巡回)".to_string(),
            "緑".to_string(),
            "黄".to_string(),
            "青".to_string(),
            "紫".to_string(),
            "水色".to_string(),
            "赤".to_string(),
            "白".to_string(),
        ],
        _ => keys.clone(),
    })
}

fn yaml_to_json(value: Option<serde_yaml::Value>) -> serde_json::Value {
    match value {
        None => serde_json::Value::Null,
        Some(v) => match v {
            serde_yaml::Value::Null => serde_json::Value::Null,
            serde_yaml::Value::Bool(b) => serde_json::Value::Bool(b),
            serde_yaml::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    serde_json::json!(i)
                } else if let Some(f) = n.as_f64() {
                    serde_json::json!(f)
                } else {
                    serde_json::Value::Null
                }
            }
            serde_yaml::Value::String(s) => serde_json::Value::String(s),
            serde_yaml::Value::Sequence(seq) => {
                // 各要素をそのまま JSON 化する。数値・bool・null を文字列以外と
                // して捨てると設定画面で選択が消える (例: `[1, 2]` → `[]`)。
                serde_json::Value::Array(
                    seq.into_iter().map(|v| yaml_to_json(Some(v))).collect(),
                )
            }
            _ => serde_json::Value::Null,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::setting_core::{apply_device_related_settings, coerce_json_setting_value};
    use std::collections::HashMap;

    #[test]
    fn coerce_float_setting_from_string() {
        let value =
            coerce_json_setting_value("update.interval", &serde_json::json!("1.5")).unwrap();
        assert_eq!(value, serde_yaml::Value::Number(serde_yaml::Number::from(1.5)));
    }

    #[test]
    fn coerce_select_setting_rejects_unknown_value() {
        assert!(
            coerce_json_setting_value("webui.table.reload-timing", &serde_json::json!("invalid"))
                .is_err()
        );
    }

    #[test]
    fn apply_device_related_settings_updates_half_indent() {
        let mut settings = HashMap::from([(
            "device".to_string(),
            serde_yaml::Value::String("kobo".to_string()),
        )]);
        let _ = apply_device_related_settings(&mut settings);
        assert_eq!(
            settings.get("default.enable_half_indent_bracket"),
            Some(&serde_yaml::Value::Bool(false))
        );
    }

    #[test]
    fn select_summaries_use_display_labels() {
        let vars = setting_variables();
        let info = vars
            .get("webui.performance-mode")
            .expect("webui.performance-mode metadata");
        assert_eq!(
            select_summaries_for_setting("webui.performance-mode", info),
            Some(vec![
                "自動判定".to_string(),
                "常に有効".to_string(),
                "常に無効".to_string(),
            ])
        );
    }

    #[test]
    fn select_summaries_include_new_tag_color_labels() {
        let vars = setting_variables();
        let info = vars
            .get("webui.new-tag-color")
            .expect("webui.new-tag-color metadata");
        assert_eq!(
            select_summaries_for_setting("webui.new-tag-color", info),
            Some(vec![
                "自動 (巡回)".to_string(),
                "緑".to_string(),
                "黄".to_string(),
                "青".to_string(),
                "紫".to_string(),
                "水色".to_string(),
                "赤".to_string(),
                "白".to_string(),
            ])
        );
    }

    #[test]
    fn select_summaries_support_default_prefixed_settings() {
        let vars = setting_variables();
        let info = vars.get("device").expect("device metadata");
        assert_eq!(
            select_summaries_for_setting("default.device", info),
            Some(vec![
                "Kindle".to_string(),
                "Kobo".to_string(),
                "EPUB".to_string(),
                "i文庫".to_string(),
                "SonyReader".to_string(),
                "iBooks".to_string(),
            ])
        );
    }

    #[test]
    fn tabbed_invisible_settings_are_visible_on_web_settings_page() {
        let vars = setting_variables();
        let info = vars.get("webui.theme").expect("webui.theme metadata");
        assert!(info.invisible);

        let entry = build_setting_entry("webui.theme", info, "local", "webui", None);

        assert_eq!(entry["tab"], "webui");
        assert_eq!(entry["invisible"], false);
    }

    #[test]
    fn sort_labels_align_with_the_sort_keys() {
        // ラベルの並びは `db::sort_keys()` と同順でなければならない。
        assert_eq!(
            sort_column_label_for_key("general_lastup"),
            Some("最新話掲載日")
        );
        assert_eq!(sort_column_label_for_key("new_arrivals_date"), Some("新着日"));
        assert_eq!(sort_column_label_for_key("unknown-key"), None);
    }

    #[test]
    fn yaml_to_json_keeps_non_string_sequence_elements() {
        // シーケンス内の数値・bool・null を捨てると設定画面の選択が消える。
        let yaml: serde_yaml::Value = serde_yaml::from_str("[kindle, 1, true, ~]").unwrap();
        assert_eq!(
            yaml_to_json(Some(yaml)),
            serde_json::json!(["kindle", 1, true, null])
        );
        let numbers: serde_yaml::Value = serde_yaml::from_str("[1, 2]").unwrap();
        assert_eq!(yaml_to_json(Some(numbers)), serde_json::json!([1, 2]));
    }
}
