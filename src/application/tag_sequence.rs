//! タグの登録順 (どのタグが先にライブラリへ登録されたか) を管理する。
//!
//! タグ一覧の「登録順」並び替えは、タグがライブラリへ入った順を保つ。
//! 小説 ID 順や小説内の位置順では再現できない:
//!
//! - 小説 ID は登録順ではなく、ライブラリへ追加された順に振られる ID であり、
//!   古い小説へ後からタグを付けても ID 順は変わらない。
//! - 同じ小説に付けたタグも、小説内の位置だけでは「いつ足されたか」が
//!   分からない。
//!
//! そこで、タグごとに **グローバルな連番** を振り、その連番を永続化する。
//! 連番は最初にタグが付いたときに決まり、その後は変わらない。
//!
//! 永続化は在庫 (`Inventory`) の `tag_sequence` キー:
//! Native は `.narou/tag_sequence.yaml` / SQLite 管理時は
//! `app_state(scope='inv', key='tag_sequence')`、
//! Worker は D1 の `app_state(scope='inv', key='tag_sequence')`。
//!
//! 既存ライブラリには連番が無いので、**最初の書き込み** で
//! 書き込み前のライブラリを最小小説 ID → タグ名の順に登録する
//! (決定的なバックフィル)。その後の新しいタグは末尾に足すので、
//! バックフィルの順序が後からぶれることは無い。

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// ライブラリのタグと、そのタグが現れる最小小説 ID。
///
/// `(タグ名, 小説 ID)` の組。バックフィルと表示順の計算で同じ形を使う。
pub type LibraryTags = Vec<(String, i64)>;

/// タグの登録順。
///
/// `order` は登録順に並べたタグ名。連番は配列の添字がそのまま使うので、
/// 連番を別に持つ必要はない。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSequence {
    /// 登録順に並べたタグ名。
    #[serde(default)]
    order: Vec<String>,
}

impl TagSequence {
    /// 登録順に並べたタグ名。
    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// 登録順が確定しているか (1 件でも登録済みなら true)。
    ///
    /// 既存ライブラリは登録順が空なので、この値が false のあいだは
    /// バックフィルを行う。
    pub fn is_initialized(&self) -> bool {
        !self.order.is_empty()
    }

    /// タグが登録済みか (連番が振られているか)。
    pub fn contains(&self, tag: &str) -> bool {
        self.order.iter().any(|value| value == tag)
    }

    /// タグの登録順 (0 始まり)。未登録なら `None`。
    pub fn position(&self, tag: &str) -> Option<usize> {
        self.order.iter().position(|value| value == tag)
    }

    /// 未登録のタグを登録順の末尾に足す。
    ///
    /// すでに登録済みのタグは動かさない (登録順は最初に決まったときから
    /// 変わらない)。返り値は登録順が変化したかどうか。
    pub fn register(&mut self, tags: impl IntoIterator<Item = impl AsRef<str>>) -> bool {
        let mut known: HashSet<String> = self.order.iter().cloned().collect();
        let mut added: Vec<String> = Vec::new();
        for tag in tags {
            let tag = tag.as_ref().to_string();
            if tag.is_empty() || !known.insert(tag.clone()) {
                continue;
            }
            added.push(tag);
        }
        if added.is_empty() {
            return false;
        }
        self.order.extend(added);
        true
    }

    /// 登録済みのタグ名集合。
    ///
    /// 線形探索 (`contains`) はタグ数 × タグ数になるため、まとめて判定する
    /// ときはこの集合を使う。
    pub fn registered_set(&self) -> HashSet<&str> {
        self.order.iter().map(String::as_str).collect()
    }

    /// 書き込みで入ったタグを登録順に反映する。
    ///
    /// `before` は書き込み前のライブラリのタグ。登録順が未初期化なら
    /// バックフィルに使う (既存タグを最小小説 ID → タグ名の順で先に登録する)。
    /// `written` は書き込み対象の小説に付いたタグ。
    ///
    /// 返り値は登録順が変わったかどうか。
    pub fn advance<'a>(
        &mut self,
        before: Option<&LibraryTags>,
        written: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        let mut changed = false;
        if !self.is_initialized()
            && let Some(before) = before
        {
            changed |= self.backfill(before);
        }
        let known = self.registered_set();
        let fresh: Vec<String> = written
            .into_iter()
            .filter(|tag| !known.contains(tag))
            .map(|tag| tag.to_string())
            .collect();
        changed |= self.register(fresh);
        changed
    }

    /// 既存ライブラリをバックフィルする (最小小説 ID → タグ名の順)。
    ///
    /// 既に登録済みのタグは動かさない。返り値は登録順が変わったかどうか。
    pub fn backfill(&mut self, library: &LibraryTags) -> bool {
        let mut sorted: Vec<&(String, i64)> = library.iter().collect();
        sorted.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
        self.register(sorted.into_iter().map(|(tag, _)| tag))
    }
}

/// タグ一覧の表示順 (連番) を作る。
///
/// 登録済みのタグは登録順で先に置き、未登録のタグは後ろに置く
/// (未登録どうしは最小小説 ID → タグ名の順)。未登録のタグは
/// バックフィル前のライブラリで、既存タグの順序は最小小説 ID 順のまま。
pub fn registered_ranks(sequence: &TagSequence, library: &LibraryTags) -> HashMap<String, i64> {
    let present: HashSet<&str> = library.iter().map(|(tag, _)| tag.as_str()).collect();
    let mut ranks: HashMap<String, i64> = HashMap::new();
    let mut next = 0i64;
    for tag in sequence.order() {
        if present.contains(tag.as_str()) {
            ranks.insert(tag.clone(), next);
            next += 1;
        }
    }
    let mut rest: Vec<&(String, i64)> = library.iter().collect();
    rest.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    for (tag, _) in rest {
        if !ranks.contains_key(tag) {
            ranks.insert(tag.clone(), next);
            next += 1;
        }
    }
    ranks
}

/// ライブラリのタグを (最小小説 ID, タグ名) 順の library tags にする。
///
/// 小説 ID が小さいほど先、同じ小説 ID の中ではタグ名の順。
/// `include` が `false` を返すタグは集めない (内部タグなど)。
pub fn library_tags<'a>(
    entries: impl IntoIterator<Item = (i64, &'a [String])>,
    include: impl Fn(&str) -> bool,
) -> LibraryTags {
    let mut tags: HashMap<String, i64> = HashMap::new();
    for (id, record_tags) in entries {
        for tag in record_tags {
            if !include(tag) {
                continue;
            }
            tags
                .entry(tag.clone())
                .and_modify(|current| {
                    if id < *current {
                        *current = id;
                    }
                })
                .or_insert(id);
        }
    }
    let mut result: LibraryTags = tags.into_iter().collect();
    result.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library(pairs: &[(&str, i64)]) -> LibraryTags {
        pairs
            .iter()
            .map(|(tag, id)| (tag.to_string(), *id))
            .collect()
    }

    #[test]
    fn registers_unknown_tags_in_order_and_keeps_known_ones() {
        let mut sequence = TagSequence::default();
        assert!(sequence.register(["b", "a"]));
        assert_eq!(sequence.order(), ["b".to_string(), "a".to_string()]);
        // 登録済みのタグは位置が変わらない。新しいタグだけ足される。
        assert!(sequence.register(["a", "c"]));
        assert_eq!(sequence.order(), ["b".to_string(), "a".to_string(), "c".to_string()]);
        // すべて登録済みなら登録順は変わらない。
        assert!(!sequence.register(["a", "b"]));
        assert_eq!(sequence.order(), ["b".to_string(), "a".to_string(), "c".to_string()]);
        assert_eq!(sequence.position("a"), Some(1));
        assert_eq!(sequence.position("zzz"), None);
        assert!(sequence.contains("c"));
    }

    #[test]
    fn empty_tags_are_ignored() {
        let mut sequence = TagSequence::default();
        assert!(!sequence.register([""]));
        assert!(sequence.order().is_empty());
        assert!(!sequence.is_initialized());
    }

    #[test]
    fn backfill_uses_the_minimal_novel_id_then_the_tag_name() {
        let mut sequence = TagSequence::default();
        assert!(sequence.backfill(&library(&[("b", 2), ("a", 1), ("c", 1)])));
        assert_eq!(sequence.order(), ["a".to_string(), "c".to_string(), "b".to_string()]);
        assert!(sequence.is_initialized());
        // 2 度目のバックフィルで登録済みのタグは動かない。
        assert!(!sequence.backfill(&library(&[("a", 1)])));
        assert_eq!(sequence.order(), ["a".to_string(), "c".to_string(), "b".to_string()]);
    }

    #[test]
    fn ranks_put_registered_tags_first_and_unregistered_last() {
        let mut sequence = TagSequence::default();
        sequence.register(["b"]);
        let ranks = registered_ranks(&sequence, &library(&[("a", 1), ("b", 2)]));
        assert_eq!(ranks.get("b"), Some(&0));
        assert_eq!(ranks.get("a"), Some(&1));
    }

    #[test]
    fn ranks_of_an_uninitialized_sequence_follow_the_legacy_order() {
        let sequence = TagSequence::default();
        let ranks = registered_ranks(&sequence, &library(&[("b", 2), ("a", 1)]));
        assert_eq!(ranks.get("a"), Some(&0));
        assert_eq!(ranks.get("b"), Some(&1));
    }

    #[test]
    fn library_tags_keeps_the_minimal_novel_id() {
        let first = vec!["a".to_string()];
        let second = vec!["b".to_string(), "a".to_string()];
        let tags = library_tags(
            [(1, first.as_slice()), (3, second.as_slice())],
            |_| true,
        );
        assert_eq!(tags, vec![("a".to_string(), 1), ("b".to_string(), 3)]);
    }

    #[test]
    fn library_tags_skips_the_filtered_tags() {
        let tags = vec!["a".to_string(), "vs_1".to_string()];
        let library = library_tags([(1, tags.as_slice())], |tag| !tag.starts_with("vs_"));
        assert_eq!(library, vec![("a".to_string(), 1)]);
    }

    #[test]
    fn yaml_round_trip_keeps_the_order() {
        let mut sequence = TagSequence::default();
        sequence.register(["ゲーム", "小説"]);
        let yaml = serde_yaml::to_string(&sequence).unwrap();
        let restored: TagSequence = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(restored.order(), sequence.order());
        assert_eq!(restored.position("ゲーム"), sequence.position("ゲーム"));
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let restored: TagSequence = serde_yaml::from_str("order:\n  - a\n").unwrap();
        assert_eq!(restored.order(), ["a".to_string()]);
        assert!(restored.is_initialized());
        // 値が無い JSON (Worker 側の value_json) も同じように読める。
        let restored: TagSequence = serde_json::from_str("{}").unwrap();
        assert!(restored.order().is_empty());
        assert!(!restored.is_initialized());
    }
}
