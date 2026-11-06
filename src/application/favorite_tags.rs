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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FavoriteTags {
    #[serde(default)]
    pub tags: Vec<String>,
}

impl<'de> Deserialize<'de> for FavoriteTags {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Mapping { tags: Vec<String> },
            Sequence(Vec<String>),
            Empty(()),
        }
        let tags = match Stored::deserialize(deserializer)? {
            Stored::Mapping { tags } | Stored::Sequence(tags) => tags,
            Stored::Empty(()) => return Ok(Self::default()),
        };
        let mut state = Self::default();
        for tag in tags {
            state.add(&tag);
        }
        Ok(state)
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct FavoriteTagsRequest {
    pub action: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

pub enum FavoriteTagsMutation {
    Add(Vec<String>),
    Remove(Vec<String>),
    Clear,
    Reorder(Vec<String>),
}

impl FavoriteTagsRequest {
    pub fn validate(self) -> Result<FavoriteTagsMutation, String> {
        let action = self.action.as_deref().unwrap_or("add");
        if !matches!(action, "add" | "remove" | "clear" | "reorder") {
            return Err("unknown favorite tags action".to_string());
        }
        let tags = self.tags.iter()
            .map(|tag| crate::application::webui::validate_web_tag_name(tag))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(match action {
            "remove" => FavoriteTagsMutation::Remove(tags),
            "clear" => FavoriteTagsMutation::Clear,
            "reorder" => FavoriteTagsMutation::Reorder(tags),
            _ => FavoriteTagsMutation::Add(tags),
        })
    }
}

impl FavoriteTagsMutation {
    pub fn apply(&self, state: &mut FavoriteTags) -> bool {
        match self {
            Self::Add(tags) => {
                let mut changed = false;
                for tag in tags { changed |= state.add(tag); }
                changed
            }
            Self::Remove(tags) => {
                let mut changed = false;
                for tag in tags { changed |= state.remove(tag); }
                changed
            }
            Self::Clear => state.clear(),
            Self::Reorder(tags) => state.reorder(tags),
        }
    }
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

    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({"success": true, "tags": self.tags})
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
    fn legacy_and_current_formats_preserve_numeric_names_and_order() {
        for source in [
            "- '1023'\n- Zulu\n- '2'\n- Zulu\n",
            "tags: ['1023', Zulu, '2', Zulu]\n",
        ] {
            let mut favorites: FavoriteTags = serde_yaml::from_str(source).unwrap();
            assert_eq!(favorites.tags, ["1023", "Zulu", "2"]);
            favorites.add("New");
            let saved = serde_yaml::to_string(&favorites).unwrap();
            assert_eq!(serde_yaml::from_str::<FavoriteTags>(&saved).unwrap(), favorites);
            let json = serde_json::to_string(&favorites).unwrap();
            assert_eq!(serde_json::from_str::<FavoriteTags>(&json).unwrap(), favorites);
        }
    }

    #[test]
    fn malformed_favorites_are_not_replaced_with_an_empty_state() {
        for source in ["tags: invalid", "unexpected: [A]", "- {tag: A}"] {
            assert!(serde_yaml::from_str::<FavoriteTags>(source).is_err());
        }
        let legacy: FavoriteTags = serde_json::from_str(r#"["1023","Zulu","2"]"#).unwrap();
        assert_eq!(legacy.tags, ["1023", "Zulu", "2"]);
    }

    #[test]
    fn invalid_actions_and_tags_do_not_reach_mutation() {
        for source in [r#"{"action":"unknown","tags":["New"]}"#, r#"{"tags":["-invalid"]}"#] {
            let request: FavoriteTagsRequest = serde_json::from_str(source).unwrap();
            assert!(request.validate().is_err());
        }
    }
}
