//! 非表示設定 (作品 / タグ / 作者 / サイト)。
//!
//! 表示だけを制御するローカル設定で、更新・変換・バックアップなどの
//! 内部処理は非表示作品も対象にする。保存は在庫 (inventory) の `hidden`
//! キー: Native は `.narou/hidden.yaml` / SQLite `app_state('inv','hidden')`、
//! Worker は D1 の `app_state('inv','hidden')`。
//!
//! 一覧は [`HiddenState::apply_to_filter`] で `NovelFilter` の除外条件に
//! 変換する。SQL 側 (native SQLite / D1) は共通の
//! `db::novel_query::build_where` が除外条件を解釈するので、保存形式を
//! テーブルへ投影する必要はない。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::db::novel_codec::fold;
use crate::platform::NovelFilter;

/// 非表示にする作品・タグ・作者・サイト。
///
/// 作者は表示名 (作品レコードの `author`)、サイトはドメインまたは
/// サイト名で指定する。どちらも比較時に fold される。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenState {
    #[serde(default)]
    pub novels: BTreeSet<i64>,
    #[serde(default)]
    pub tags: BTreeSet<String>,
    #[serde(default)]
    pub authors: BTreeSet<String>,
    #[serde(default)]
    pub sites: BTreeSet<String>,
}

impl HiddenState {
    pub fn is_empty(&self) -> bool {
        self.novels.is_empty()
            && self.tags.is_empty()
            && self.authors.is_empty()
            && self.sites.is_empty()
    }

    /// この作品が非表示か。`domain` は作品レコードの `domain` (無い場合は
    /// サイト名で照合する)。
    pub fn matches(
        &self,
        id: i64,
        author: &str,
        tags: &[String],
        domain: Option<&str>,
        sitename: &str,
    ) -> bool {
        if self.novels.contains(&id) {
            return true;
        }
        if self
            .tags
            .iter()
            .any(|hidden| tags.iter().any(|tag| tag == hidden))
        {
            return true;
        }
        if !self.authors.is_empty() {
            let author = fold(author);
            if self
                .authors
                .iter()
                .any(|hidden| fold(hidden) == author)
            {
                return true;
            }
        }
        if !self.sites.is_empty() {
            let domain = domain.map(fold);
            let sitename = fold(sitename);
            if self.sites.iter().any(|hidden| {
                let hidden = fold(hidden);
                hidden == sitename || domain.as_deref() == Some(hidden.as_str())
            }) {
                return true;
            }
        }
        false
    }

    /// 一覧クエリに非表示条件を足す。呼び出し側は表示用の一覧だけに使う。
    pub fn apply_to_filter(&self, filter: &mut NovelFilter) {
        if !self.novels.is_empty() {
            filter.exclude_ids = self.novels.iter().copied().collect();
        }
        if !self.tags.is_empty() {
            filter.exclude_tags = self.tags.iter().cloned().collect();
        }
        if !self.authors.is_empty() {
            filter.exclude_authors = self.authors.iter().map(|author| fold(author)).collect();
        }
        if !self.sites.is_empty() {
            filter.exclude_sites = self.sites.iter().map(|site| fold(site)).collect();
        }
    }

    pub fn add_novel(&mut self, id: i64) -> bool {
        self.novels.insert(id)
    }

    pub fn remove_novel(&mut self, id: i64) -> bool {
        self.novels.remove(&id)
    }

    pub fn add_tag(&mut self, tag: &str) -> bool {
        self.tags.insert(tag.to_string())
    }

    pub fn remove_tag(&mut self, tag: &str) -> bool {
        self.tags.remove(tag)
    }

    pub fn add_author(&mut self, author: &str) -> bool {
        self.authors.insert(author.to_string())
    }

    pub fn remove_author(&mut self, author: &str) -> bool {
        self.authors.remove(author)
    }

    pub fn add_site(&mut self, site: &str) -> bool {
        self.sites.insert(site.to_string())
    }

    pub fn remove_site(&mut self, site: &str) -> bool {
        self.sites.remove(site)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_each_unit_with_folded_author_and_site() {
        let mut hidden = HiddenState::default();
        hidden.add_novel(7);
        hidden.add_tag("秘密");
        hidden.add_author("作者A");
        hidden.add_site("kakuyomu.jp");

        assert!(hidden.matches(7, "", &[], None, ""));
        assert!(hidden.matches(1, "", &["秘密".to_string()], None, ""));
        assert!(hidden.matches(2, "作者a", &[], None, ""));
        assert!(hidden.matches(3, "別", &[], Some("kakuyomu.jp"), ""));
        assert!(hidden.matches(4, "別", &[], None, "Kakuyomu.JP"));
        assert!(!hidden.matches(5, "別", &["公開".to_string()], None, "なろう"));
    }

    #[test]
    fn apply_to_filter_folds_author_and_site() {
        let mut hidden = HiddenState::default();
        hidden.add_author("作者Ａ");
        hidden.add_site("カクヨム");
        hidden.add_tag("秘密");
        hidden.add_novel(9);
        let mut filter = NovelFilter::all();
        hidden.apply_to_filter(&mut filter);
        assert_eq!(filter.exclude_ids, vec![9]);
        assert_eq!(filter.exclude_tags, vec!["秘密".to_string()]);
        assert!(filter.exclude_authors.iter().any(|value| value == &fold("作者Ａ")));
        assert!(filter.exclude_sites.iter().any(|value| value == &fold("カクヨム")));
    }
}
