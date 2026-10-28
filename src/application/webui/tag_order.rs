//! タグ一覧の表示順。
//!
//! メインページのタグ一覧は-automatic タグが数百個になるため並び順を選べると
//! management しやすくなる。お気に入りタグを先頭に置く順を既定とし、
//! アルファベット順・件数順・登録順を選べる。
//!
//! 並び順の設定は `webui.tag-sort-order` (local 設定) に保存する。保存値は
//! [`TagSortOrder`] の名前 (`favorites_first` / `alphabetical` / `count` /
//! `registered`) で、未知の値や未設定は既定 (`favorites_first`) として扱う。
//!
//! native (`src/web/misc.rs`) と Worker (`worker_entry/src/webui/`) は同じ
//! 並び順の規則を使うため、判定と並び替えはこの共有モジュールに置く。

use std::collections::HashMap;

/// タグ一覧の表示順。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TagSortOrder {
    /// お気に入りタグを先頭に、その後にその他のタグを件数順で並べる (既定)。
    #[default]
    FavoritesFirst,
    /// タグ名の昇順 (バイト比較 — SQLite の `COLLATE BINARY` と同じ)。
    Alphabetical,
    /// タグの出現件数の降順、同数はタグ名の昇順。
    Count,
    /// タグの登録順 (タグが付いた最初の小説の ID 順、同数はタグ名の昇順)。
    Registered,
}

/// 設定値 (`webui.tag-sort-order`) の名前。
pub const TAG_SORT_ORDER_SETTING: &str = "webui.tag-sort-order";

impl TagSortOrder {
    /// 設定値から並び順を決める。未知の値・未設定は既定にフォールバックする。
    pub fn from_setting(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("favorites_first") => Self::FavoritesFirst,
            Some("alphabetical") => Self::Alphabetical,
            Some("count") => Self::Count,
            Some("registered") => Self::Registered,
            _ => Self::default(),
        }
    }

    /// 設定値として保存する名前。
    pub fn setting_value(self) -> &'static str {
        match self {
            Self::FavoritesFirst => "favorites_first",
            Self::Alphabetical => "alphabetical",
            Self::Count => "count",
            Self::Registered => "registered",
        }
    }

    /// 選択可能な並び順の一覧 (設定画面の選択肢用)。
    pub fn all() -> &'static [TagSortOrder] {
        &[
            Self::FavoritesFirst,
            Self::Alphabetical,
            Self::Count,
            Self::Registered,
        ]
    }

    /// 画面に出す表示名。
    pub fn label(self) -> &'static str {
        match self {
            Self::FavoritesFirst => "お気に入り優先",
            Self::Alphabetical => "アルファベット順",
            Self::Count => "タグの使用件数順",
            Self::Registered => "登録順",
        }
    }
}

/// タグ名と出現件数の組。
pub type TagCount = (String, usize);

/// タグ一覧を並び替える。
///
/// `tags` は元の並び順 (件数順) のタグ名、`counts` はタグ名ごとの出現件数、
/// `favorites` はお気に入りタグ (登録順)、`registered` はタグ名ごとの登録順
/// (タグが付いた最初の小説の ID)。
///
/// - `FavoritesFirst`: お気に入りタグをお気に入り順で先頭に置き、その後に
///   その他のタグを元の並び順で並べる。
/// - `Alphabetical`: タグ名の昇順。
/// - `Count`: 件数の降順、同数はタグ名の昇順。
/// - `Registered`: 登録順 (未登録のタグは後ろ)。
///
/// お気に入りタグの並びは `favorites` の登録順に従う (ドラッグ＆ドロップで
/// 並べ替えた順序を保つ)。
pub fn sort_tags(
    order: TagSortOrder,
    tags: &[String],
    counts: &HashMap<String, usize>,
    favorites: &[String],
    registered: &HashMap<String, i64>,
) -> Vec<String> {
    match order {
        TagSortOrder::FavoritesFirst => {
            let mut result = Vec::with_capacity(tags.len());
            for favorite in favorites {
                if tags.iter().any(|tag| tag == favorite) {
                    result.push(favorite.clone());
                }
            }
            for tag in tags {
                if !favorites.iter().any(|favorite| favorite == tag) {
                    result.push(tag.clone());
                }
            }
            result
        }
        TagSortOrder::Alphabetical => {
            let mut result = tags.to_vec();
            result.sort();
            result
        }
        TagSortOrder::Count => {
            let mut result = tags.to_vec();
            result.sort_by(|left, right| {
                let left_count = counts.get(left).copied().unwrap_or(0);
                let right_count = counts.get(right).copied().unwrap_or(0);
                right_count
                    .cmp(&left_count)
                    .then_with(|| left.cmp(right))
            });
            result
        }
        TagSortOrder::Registered => {
            let mut result = tags.to_vec();
            result.sort_by(|left, right| {
                let left_order = registered.get(left).copied().unwrap_or(i64::MAX);
                let right_order = registered.get(right).copied().unwrap_or(i64::MAX);
                left_order
                    .cmp(&right_order)
                    .then_with(|| left.cmp(right))
            });
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(pairs: &[(&str, usize)]) -> HashMap<String, usize> {
        pairs
            .iter()
            .map(|(tag, count)| (tag.to_string(), *count))
            .collect()
    }

    fn registered(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs
            .iter()
            .map(|(tag, id)| (tag.to_string(), *id))
            .collect()
    }

    #[test]
    fn unknown_setting_falls_back_to_the_default() {
        assert_eq!(TagSortOrder::from_setting(None), TagSortOrder::FavoritesFirst);
        assert_eq!(
            TagSortOrder::from_setting(Some("nonsense")),
            TagSortOrder::FavoritesFirst
        );
        assert_eq!(
            TagSortOrder::from_setting(Some("alphabetical")),
            TagSortOrder::Alphabetical
        );
    }

    #[test]
    fn setting_values_round_trip() {
        for order in TagSortOrder::all() {
            assert_eq!(TagSortOrder::from_setting(Some(order.setting_value())), *order);
        }
    }

    #[test]
    fn favorites_come_first_in_their_own_order() {
        let tags = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        let sorted = sort_tags(
            TagSortOrder::FavoritesFirst,
            &tags,
            &counts(&[("a", 5)]),
            &["b".to_string(), "a".to_string()],
            &HashMap::new(),
        );
        assert_eq!(sorted, vec!["b", "a", "c"]);
    }

    #[test]
    fn alphabetical_sorts_by_name() {
        let tags = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        let sorted = sort_tags(
            TagSortOrder::Alphabetical,
            &tags,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(sorted, vec!["a", "b", "c"]);
    }

    #[test]
    fn count_sorts_by_usage() {
        let tags = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let sorted = sort_tags(
            TagSortOrder::Count,
            &tags,
            &counts(&[("a", 1), ("b", 9), ("c", 5)]),
            &[],
            &HashMap::new(),
        );
        assert_eq!(sorted, vec!["b", "c", "a"]);
    }

    #[test]
    fn registered_sorts_by_first_use() {
        let tags = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let sorted = sort_tags(
            TagSortOrder::Registered,
            &tags,
            &HashMap::new(),
            &[],
            &registered(&[("c", 30), ("a", 10)]),
        );
        // b は登録順が不明なので後ろに回される。
        assert_eq!(sorted, vec!["a", "c", "b"]);
    }
}
