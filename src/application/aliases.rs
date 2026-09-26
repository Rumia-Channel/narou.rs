//! 小説ターゲットの別名 (alias) 解決 — native の CLI サブプロセスと Worker の
//! Web UI が共有する純関数。
//!
//! `narou alias name=target` が `.narou/alias.yaml` (native) /
//! `app_state('inv','alias')` (Worker D1) に書いた対応表を読み、指定された
//! ターゲット文字列を別名の右辺へ解決する。native は Inventory 経由で
//! `HashMap<String, serde_yaml::Value>` のまま読むので
//! [`alias_map_from_values`]、Worker は行ペイロード (YAML/JSON 文字列) を
//! [`parse_alias_map`] で同じ表にしてから、両者とも同じ解決関数を通す。
//!
//! 既存実装 (`src/commands/mod.rs::resolve_alias_target`,
//! `src/commands/update.rs::alias_to_target`, `src/mail.rs::alias_to_target`)
//! と同じ意味論:
//! - 完全一致の別名だけを解決する (大文字小文字は区別)。
//! - ネストした別名は辿らない (1 段だけ。別名の値が更に別名でもそのまま返す)。
//! - 登録が無い / 値がスカラーでない / 表が空のときは、convert 系は入力を
//!   そのまま返し、update 系は非数値ターゲットを小文字化して返す。
//!   「解決不能」を表す値はこのパススルー文字列そのもの (native は解決不能を
//!   エラーにせず、後段のターゲット解決が存在確認で落とす)。

use std::collections::HashMap;

/// 別名表を保持している `app_state` 行のスコープ / キー (native の
/// `alias.yaml` = Local inventory スコープに相当)。Worker 側はこの行から
/// [`parse_alias_map`] する。
pub const ALIAS_INVENTORY_SCOPE: &str = "inv";
pub const ALIAS_INVENTORY_KEY: &str = "alias";

/// `alias.yaml` / `app_state` ペイロード (YAML または JSON 文字列) を
/// `HashMap<String, String>` の別名表にする。スカラー以外の値は別名として
/// 無効なので捨てる (native が load した表をそのまま解決するのと同じ結果)。
/// パース不能・空のときは空表 (= 別名なし)。
pub fn parse_alias_map(payload: &str) -> HashMap<String, String> {
    let mapping: HashMap<String, serde_yaml::Value> =
        serde_yaml::from_str(payload).unwrap_or_default();
    alias_map_from_values(mapping)
}

/// Inventory (`alias.yaml` / `app_state('inv','alias')`) から読んだ
/// `HashMap<String, serde_yaml::Value>` を文字列の別名表に変換する。
/// スカラー以外の値は捨てる (native `resolve_alias_target` が
/// `yaml_value_to_string` で取りこぼした値と同じく解決対象外)。
pub fn alias_map_from_values(
    mapping: HashMap<String, serde_yaml::Value>,
) -> HashMap<String, String> {
    mapping
        .into_iter()
        .filter_map(|(name, value)| yaml_scalar_to_string(&value).map(|s| (name, s)))
        .collect()
}

/// native `yaml_value_to_string`: 文字列・数値・真偽値スカラーだけを取る
/// (配列やマップは別名の値として無効なので無視 = native 同様フォールバック)。
pub fn yaml_scalar_to_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `commands/mod.rs::resolve_alias_target`: 完全一致の別名名だけ解決し、
/// 無ければ入力をそのまま返す (convert / download / diff 系。小文字化しない)。
pub fn resolve_alias_target(aliases: &HashMap<String, String>, target: &str) -> String {
    aliases
        .get(target)
        .cloned()
        .unwrap_or_else(|| target.to_string())
}

/// `commands/update.rs::alias_to_target`: convert 側と同じだが、別名が無い
/// 非数値ターゲットは小文字化してから返す (update 系)。
pub fn alias_to_target_for_update(aliases: &HashMap<String, String>, target: &str) -> String {
    match aliases.get(target) {
        Some(alias) => alias.clone(),
        None => {
            if target.chars().all(|c| c.is_ascii_digit()) {
                target.to_string()
            } else {
                target.to_lowercase()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases() -> HashMap<String, String> {
        HashMap::from([
            ("sample".to_string(), "n9669bk".to_string()),
            ("chain".to_string(), "sample".to_string()),
            ("Empty".to_string(), String::new()),
            ("CaseName".to_string(), "n0000aa".to_string()),
        ])
    }

    #[test]
    fn simple_alias_resolves_to_its_target() {
        assert_eq!(resolve_alias_target(&aliases(), "sample"), "n9669bk");
    }

    #[test]
    fn nested_alias_is_not_followed() {
        // 別名の値が更に別名でも 1 段しか解決しない (native と同じ挙動)。
        assert_eq!(resolve_alias_target(&aliases(), "chain"), "sample");
    }

    #[test]
    fn alias_lookup_is_case_sensitive() {
        let map = aliases();
        // 別名 "CaseName" は完全一致でのみ解決される。
        assert_eq!(resolve_alias_target(&map, "CaseName"), "n0000aa");
        // convert 側: 未登録のままなので入力をそのまま返す。
        assert_eq!(resolve_alias_target(&map, "casename"), "casename");
        // update 側: 未登録の非数値は小文字化される (ncode が小文字なので)。
        assert_eq!(alias_to_target_for_update(&map, "casename"), "casename");
        assert_eq!(alias_to_target_for_update(&map, "N9669BK"), "n9669bk");
        // 数値ターゲットは update 側でもそのまま。
        assert_eq!(alias_to_target_for_update(&map, "123"), "123");
    }

    #[test]
    fn unregistered_target_passes_through() {
        let map = aliases();
        assert_eq!(resolve_alias_target(&map, "n1234zz"), "n1234zz");
        assert_eq!(resolve_alias_target(&map, ""), "");
        assert_eq!(alias_to_target_for_update(&map, "n1234zz"), "n1234zz");
        assert_eq!(alias_to_target_for_update(&map, ""), "");
    }

    #[test]
    fn alias_with_empty_value_resolves_to_empty() {
        // `name=` で登録された空文字は登録済み扱い (update 側でも小文字化しない)。
        let map = aliases();
        assert_eq!(resolve_alias_target(&map, "Empty"), "");
        assert_eq!(alias_to_target_for_update(&map, "Empty"), "");
    }

    #[test]
    fn alias_map_from_values_keeps_only_scalars() {
        let values: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(
            "num: 42\nflag: true\nname: n9669bk\nseq: [a, b]\nnested: {x: 1}\n",
        )
        .unwrap();
        let map = alias_map_from_values(values);
        assert_eq!(map.get("num").map(String::as_str), Some("42"));
        assert_eq!(map.get("flag").map(String::as_str), Some("true"));
        assert_eq!(map.get("name").map(String::as_str), Some("n9669bk"));
        assert!(!map.contains_key("seq"));
        assert!(!map.contains_key("nested"));
    }

    #[test]
    fn parse_alias_map_matches_value_map_resolution() {
        // native (alias_map_from_values) と Worker (parse_alias_map) が
        // 同じ入力から同じ別名表を組み立て、同じ解決結果を返す。
        let payload = "sample: n9669bk\nnum: 7\n";
        let values: HashMap<String, serde_yaml::Value> =
            serde_yaml::from_str(payload).unwrap();

        let from_values = alias_map_from_values(values);
        let from_payload = parse_alias_map(payload);
        assert_eq!(from_values, from_payload);

        for target in ["sample", "num", "missing", ""] {
            assert_eq!(
                resolve_alias_target(&from_values, target),
                resolve_alias_target(&from_payload, target),
                "target={target:?}"
            );
            assert_eq!(
                alias_to_target_for_update(&from_values, target),
                alias_to_target_for_update(&from_payload, target),
                "target={target:?}"
            );
        }
    }

    #[test]
    fn parse_alias_map_tolerates_empty_and_broken_payloads() {
        assert!(parse_alias_map("").is_empty());
        assert!(parse_alias_map("{}").is_empty());
        assert!(parse_alias_map("not: [valid\n").is_empty());
    }
}
