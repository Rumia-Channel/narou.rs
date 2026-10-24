//! バーチャル小説 (v0.5.0)。
//!
//! 「XXX その1」「XXX その2」のように分かれた実小説を 1 作品として一覧に
//! 表示し、EPUB だけを登録順に結合する。仮想小説自体も `novels` テーブルの
//! 通常行で、`ncode = vs_<ID>` を持つ。実体 (本文・挿絵) は各メンバーのまま。
//!
//! メンバー構成と順序は在庫 `virtual_novel_members` に
//! `{ virtual_id: [member_id, ...] }` で保存する。仮想小説のタグには
//! `vs_<ID>` を必ず含めるので、既存の hidden tag 機構で非表示にできる。

use std::collections::{BTreeMap, BTreeSet, HashSet};

/// 仮想小説の ncode 接頭辞。
pub const VIRTUAL_NCODE_PREFIX: &str = "vs_";

/// 仮想小説 ID から ncode を作る。
pub fn ncode(id: i64) -> String {
    format!("{VIRTUAL_NCODE_PREFIX}{id}")
}

/// `vs_<数字>` 形式の ncode か。
pub fn is_virtual_ncode(ncode: &str) -> bool {
    ncode
        .strip_prefix(VIRTUAL_NCODE_PREFIX)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

/// `vs_<数字>` から ID を取り出す。
pub fn id_from_ncode(ncode: &str) -> Option<i64> {
    ncode
        .strip_prefix(VIRTUAL_NCODE_PREFIX)?
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
}

/// 仮想小説に必ず付与するタグ。
pub fn tag(id: i64) -> String {
    ncode(id)
}

/// 仮想小説 ID → 登録順のメンバー ID。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct VirtualNovelMembers {
    pub groups: BTreeMap<i64, Vec<i64>>,
}

impl VirtualNovelMembers {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn get(&self, virtual_id: i64) -> Option<&[i64]> {
        self.groups.get(&virtual_id).map(Vec::as_slice)
    }

    /// すべてのメンバー ID（表示一覧から除外する用）。
    pub fn all_member_ids(&self) -> HashSet<i64> {
        self.groups
            .values()
            .flat_map(|members| members.iter().copied())
            .collect()
    }

    /// 仮想小説に既に属しているメンバー ID。
    pub fn owner_of(&self, member_id: i64) -> Option<i64> {
        self.groups.iter().find_map(|(virtual_id, members)| {
            members.contains(&member_id).then_some(*virtual_id)
        })
    }

    /// 1 つの仮想小説を登録する。メンバーは空でない・重複なし・実小説のみ。
    pub fn insert(&mut self, virtual_id: i64, members: Vec<i64>) -> Result<(), String> {
        validate_members(&members)?;
        if self.groups.contains_key(&virtual_id) {
            return Err(format!("仮想小説 vs_{virtual_id} は既に登録されています"));
        }
        if let Some((owner, member)) = members.iter().find_map(|member| {
            self.owner_of(*member).map(|owner| (owner, *member))
        }) {
            return Err(format!(
                "小説 {member} は既に仮想小説 vs_{owner} に属しています"
            ));
        }
        self.groups.insert(virtual_id, members);
        Ok(())
    }

    /// 登録順を全件指定で並べ替える。
    pub fn reorder(&mut self, virtual_id: i64, ordered: Vec<i64>) -> Result<(), String> {
        validate_members(&ordered)?;
        let Some(current) = self.groups.get(&virtual_id) else {
            return Err(format!("仮想小説 vs_{virtual_id} は登録されていません"));
        };
        let current_set: BTreeSet<i64> = current.iter().copied().collect();
        let ordered_set: BTreeSet<i64> = ordered.iter().copied().collect();
        if current_set != ordered_set {
            return Err("並べ替えには現在のメンバーを全件指定してください".to_string());
        }
        self.groups.insert(virtual_id, ordered);
        Ok(())
    }

    pub fn remove(&mut self, virtual_id: i64) -> Option<Vec<i64>> {
        self.groups.remove(&virtual_id)
    }
}

fn validate_members(members: &[i64]) -> Result<(), String> {
    if members.is_empty() {
        return Err("メンバーを 1 件以上指定してください".to_string());
    }
    if members.iter().any(|id| *id <= 0) {
        return Err("メンバー ID は正の整数で指定してください".to_string());
    }
    let unique: BTreeSet<i64> = members.iter().copied().collect();
    if unique.len() != members.len() {
        return Err("同じ小説を複数回登録できません".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_roundtrips_as_a_bare_yaml_map() {
        let mut members = VirtualNovelMembers::default();
        members.insert(3, vec![1, 2]).unwrap();
        let yaml = serde_yaml::to_string(&members).unwrap();
        let back: VirtualNovelMembers = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.get(3), Some([1, 2].as_slice()));
    }

    #[test]
    fn ncode_roundtrip_and_detection() {
        assert_eq!(ncode(12), "vs_12");
        assert_eq!(id_from_ncode("vs_12"), Some(12));
        assert!(is_virtual_ncode("vs_1"));
        assert!(!is_virtual_ncode("vs_"));
        assert!(!is_virtual_ncode("n1234ab"));
    }

    #[test]
    fn insert_rejects_duplicates_and_existing_owner() {
        let mut members = VirtualNovelMembers::default();
        members.insert(1, vec![10, 11]).unwrap();
        assert!(members.insert(2, vec![11, 12]).is_err());
        assert!(members.insert(3, vec![10, 10]).is_err());
        assert_eq!(members.owner_of(11), Some(1));
        assert_eq!(members.all_member_ids().len(), 2);
    }

    #[test]
    fn reorder_requires_the_same_set() {
        let mut members = VirtualNovelMembers::default();
        members.insert(1, vec![10, 11, 12]).unwrap();
        members.reorder(1, vec![12, 10, 11]).unwrap();
        assert_eq!(members.get(1), Some([12, 10, 11].as_slice()));
        assert!(members.reorder(1, vec![12, 10]).is_err());
        assert!(members.reorder(1, vec![12, 10, 99]).is_err());
    }
}
