//! お気に入りタグ。
//!
//! 自動付与されるタグは数百個まで増えるため、「タグの編集」画面から
//! 検索しないとよく使うタグを選べない。お気に入りタグを登録し、その一覧を
//! タグ編集画面の先頭へ表示する。
//!
//! 保存は在庫 (inventory) の `favorite_tags` キー: Native は
//! `.narou/favorite_tags.yaml` / SQLite `app_state('inv','favorite_tags')`、
//! Worker は D1 の `app_state('inv','favorite_tags')`。
//!
//! 登録順は保持する (利用者が登録した順序をそのまま見せる)。タグ名は Web UI
//! のタグ名検証と同じ規則 (`application::webui::validate_web_tag_name`) を
//! 通す。タグ色とは独立した設定で、片方の色分けは他方に影響しない。

use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

/// お気に入りタグの集合。順序は登録順。
///
/// 保存形式は `tags:` を持つマッピング (native の YAML / Worker の JSON)。
/// 読み込みは配列形式も受け付ける (旧データ互換)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteTags {
    #[serde(default)]
    pub tags: Vec<String>,
}

impl FavoriteTags {
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }

    pub fn contains(&self, tag: &str) -> bool {
        self.tags.iter().any(|value| value == tag)
    }

    /// タグを登録する。すでにあれば位置を保つ (先頭には移動しない)。
    pub fn add(&mut self, tag: &str) -> bool {
        let tag = tag.trim();
        if tag.is_empty() || self.contains(tag) {
            return false;
        }
        self.tags.push(tag.to_string());
        true
    }

    /// タグの登録を外す。
    pub fn remove(&mut self, tag: &str) -> bool {
        let before = self.tags.len();
        self.tags.retain(|value| value != tag);
        before != self.tags.len()
    }

    /// 全解除。
    pub fn clear(&mut self) -> bool {
        let changed = !self.tags.is_empty();
        self.tags.clear();
        changed
    }

    /// お気に入りタグを並び替える (ドラッグ＆ドロップの並べ替え)。
    ///
    /// `order` に含まれるタグだけを並べ替える。`order` に無いタグは
    /// 元の位置を保ったまま残るので、部分的な並び替えでも他のタグを
    /// 消さない。`order` に未知のタグが混ざっていても無視する。
    pub fn reorder(&mut self, order: &[String]) -> bool {
        let known: HashSet<&str> = self.tags.iter().map(String::as_str).collect();
        let mut requested: Vec<&str> = Vec::with_capacity(order.len());
        for tag in order {
            let tag = tag.as_str();
            if known.contains(tag) && !requested.contains(&tag) {
                requested.push(tag);
            }
        }
        if requested.is_empty() {
            return false;
        }
        // `order` に載っているタグが占める位置 (元の並び順) を求め、
        // その位置に `order` の順に並べ替える。載っていないタグは動かない。
        let mut slots: Vec<usize> = Vec::with_capacity(requested.len());
        for (index, tag) in self.tags.iter().enumerate() {
            if requested.contains(&tag.as_str()) {
                slots.push(index);
            }
        }
        let before = self.tags.clone();
        for (slot, tag) in slots.into_iter().zip(requested) {
            self.tags[slot] = tag.to_string();
        }
        before != self.tags
    }

    /// 存在しなくなったタグをお気に入りから外す。
    ///
    /// automatic タグが消えた (作品削除・タグ整理) 場合に、お気に入りに
    /// 残ったタグを一覧に出さないため。
    pub fn retain_existing(&mut self, existing: &BTreeSet<String>) -> bool {
        let before = self.tags.len();
        self.tags.retain(|tag| existing.contains(tag));
        before != self.tags.len()
    }

    /// 保存された YAML 値 (マッピング形式) を読む。
    ///
    /// 旧形式 (`tags:` を持つマッピング) も新しい形式 (配列) も読む。
    pub fn from_yaml_value(value: &serde_yaml::Value) -> Self {
        let mut favorites = Self::default();
        match value {
            serde_yaml::Value::Sequence(items) => {
                for item in items {
                    if let Some(tag) = item.as_str() {
                        favorites.add(tag);
                    }
                }
            }
            serde_yaml::Value::Mapping(mapping) => {
                if let Some(items) = mapping.get(serde_yaml::Value::String("tags".to_string()))
                    && let Some(items) = items.as_sequence()
                {
                    for item in items {
                        if let Some(tag) = item.as_str() {
                            favorites.add(tag);
                        }
                    }
                }
            }
            _ => {}
        }
        favorites
    }

    /// 保存用の YAML 値 (`tags:` を持つマッピング)。
    pub fn to_yaml_value(&self) -> serde_yaml::Value {
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(
            serde_yaml::Value::String("tags".to_string()),
            serde_yaml::Value::Sequence(
                self.tags
                    .iter()
                    .map(|tag| serde_yaml::Value::String(tag.clone()))
                    .collect(),
            ),
        );
        serde_yaml::Value::Mapping(mapping)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_insertion_order_and_does_not_duplicate() {
        let mut favorites = FavoriteTags::default();
        assert!(favorites.add("小説"));
        assert!(favorites.add("ゲーム"));
        assert!(!favorites.add("小説"));
        assert_eq!(favorites.tags, vec!["小説".to_string(), "ゲーム".to_string()]);
        assert!(favorites.contains("ゲーム"));
        assert!(!favorites.contains("漫画"));
    }

    #[test]
    fn remove_and_clear_report_whether_they_changed() {
        let mut favorites = FavoriteTags::default();
        favorites.add("A");
        favorites.add("B");
        assert!(favorites.remove("A"));
        assert!(!favorites.remove("A"));
        assert_eq!(favorites.tags, vec!["B".to_string()]);
        assert!(favorites.clear());
        assert!(!favorites.clear());
        assert!(favorites.is_empty());
    }

    #[test]
    fn reorder_moves_tags_to_the_requested_order() {
        let mut favorites = FavoriteTags::default();
        favorites.add("A");
        favorites.add("B");
        favorites.add("C");
        assert!(favorites.reorder(&["C".to_string(), "A".to_string(), "B".to_string()]));
        assert_eq!(favorites.tags, vec!["C".to_string(), "A".to_string(), "B".to_string()]);
    }

    #[test]
    fn reorder_keeps_tags_that_are_not_listed() {
        let mut favorites = FavoriteTags::default();
        favorites.add("A");
        favorites.add("B");
        favorites.add("C");
        // B は order に無いので元の位置 (2 番目) を保つ。
        // order に載った A と C は元の位置同士を入れ替える。
        assert!(favorites.reorder(&["C".to_string(), "A".to_string()]));
        assert_eq!(favorites.tags, vec!["C".to_string(), "B".to_string(), "A".to_string()]);
    }

    #[test]
    fn reorder_ignores_unknown_tags_and_reports_no_change() {
        let mut favorites = FavoriteTags::default();
        favorites.add("A");
        favorites.add("B");
        assert!(!favorites.reorder(&["未知".to_string()]));
        assert_eq!(favorites.tags, vec!["A".to_string(), "B".to_string()]);
        // 同じ順番なら変更なし。
        assert!(!favorites.reorder(&["A".to_string(), "B".to_string()]));
        assert_eq!(favorites.tags, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn reorder_preserves_the_other_tags_after_a_full_swap() {
        let mut favorites = FavoriteTags::default();
        for tag in ["A", "B", "C", "D"] {
            favorites.add(tag);
        }
        assert!(favorites.reorder(&["D".to_string(), "C".to_string(), "B".to_string(), "A".to_string()]));
        assert_eq!(
            favorites.tags,
            vec!["D".to_string(), "C".to_string(), "B".to_string(), "A".to_string()]
        );
    }

    #[test]
    fn drops_tags_that_no_longer_exist() {
        let mut favorites = FavoriteTags::default();
        favorites.add("残す");
        favorites.add("消える");
        let existing = BTreeSet::from(["残す".to_string()]);
        assert!(favorites.retain_existing(&existing));
        assert_eq!(favorites.tags, vec!["残す".to_string()]);
    }

    #[test]
    fn round_trips_through_yaml() {
        let mut favorites = FavoriteTags::default();
        favorites.add("A");
        favorites.add("B");
        let payload = favorites.to_yaml_value();
        assert_eq!(FavoriteTags::from_yaml_value(&payload), favorites);
    }

    #[test]
    fn reads_the_legacy_mapping_shape() {
        let value: serde_yaml::Value =
            serde_yaml::from_str("tags:\n  - A\n  - B\n").unwrap();
        let favorites = FavoriteTags::from_yaml_value(&value);
        assert_eq!(favorites.tags, vec!["A".to_string(), "B".to_string()]);
    }
}
