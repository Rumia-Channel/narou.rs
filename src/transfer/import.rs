use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{ParsedArchive, ParsedNovel};

/// How an existing novel (same `toc_url`) should be handled.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Keep the library copy and skip this archive entry.
    #[default]
    Skip,
    /// Replace the library copy with the archive entry.
    Overwrite,
}

/// Action selected for one archived novel.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportAction {
    /// The novel is not in the library yet.
    Create,
    /// The novel exists and the plan overwrites it.
    Overwrite,
    /// The novel exists and the plan leaves it alone.
    Skip,
}

/// One decision in an [`ImportPlan`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ImportPlanEntry {
    /// Index into [`ParsedArchive::manifest`] / [`ParsedArchive::novels`].
    pub index: usize,
    pub action: ImportAction,
    /// Human-readable explanation of the decision.
    pub reason: String,
}

/// Pure create / overwrite / skip plan for an archive.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportPlan {
    pub entries: Vec<ImportPlanEntry>,
}

impl ImportPlan {
    /// Number of entries with `action`.
    pub fn count(&self, action: ImportAction) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.action == action)
            .count()
    }
}

/// Minimal view of a novel already in the library.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ExistingNovel {
    /// Canonical table-of-contents URL, used as the conflict key.
    pub toc_url: String,
    /// Current title, used only in the decision reason.
    pub title: String,
}

/// Decides what to do with every novel in a parsed archive.
///
/// Conflicts are resolved by exact `toc_url` comparison. The first existing
/// novel with a given `toc_url` wins; callers should pass a de-duplicated
/// library index. `title` is informational only.
pub fn plan_import(
    parsed: &ParsedArchive,
    existing: &[ExistingNovel],
    conflict: ConflictPolicy,
) -> ImportPlan {
    let mut existing_by_url: HashMap<&str, &ExistingNovel> = HashMap::with_capacity(existing.len());
    for novel in existing {
        // The first entry for a toc_url wins; a duplicated library index must
        // not change the decision.
        existing_by_url
            .entry(novel.toc_url.as_str())
            .or_insert(novel);
    }

    let entries = parsed
        .novels
        .iter()
        .map(|novel| plan_entry(novel.index, novel, &existing_by_url, conflict))
        .collect();

    ImportPlan { entries }
}

fn plan_entry(
    index: usize,
    novel: &ParsedNovel,
    existing_by_url: &HashMap<&str, &ExistingNovel>,
    conflict: ConflictPolicy,
) -> ImportPlanEntry {
    let Some(existing) = existing_by_url.get(novel.toc_url.as_str()) else {
        return ImportPlanEntry {
            index,
            action: ImportAction::Create,
            reason: "not present in the library".into(),
        };
    };

    let reason = if existing.title == novel.title {
        format!("already in the library as {:?}", existing.title)
    } else {
        format!(
            "already in the library as {:?} (archive title is {:?})",
            existing.title, novel.title
        )
    };
    let action = match conflict {
        ConflictPolicy::Skip => ImportAction::Skip,
        ConflictPolicy::Overwrite => ImportAction::Overwrite,
    };
    ImportPlanEntry {
        index,
        action,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::ArchiveManifest;

    fn parsed(toc_urls: &[(&str, &str)]) -> ParsedArchive {
        let manifest = ArchiveManifest {
            novels: toc_urls
                .iter()
                .map(|(toc_url, title)| crate::transfer::ArchiveNovel {
                    toc_url: (*toc_url).into(),
                    title: (*title).into(),
                    ..Default::default()
                })
                .collect(),
            ..ArchiveManifest::default()
        };
        ParsedArchive {
            novels: manifest
                .novels
                .iter()
                .enumerate()
                .map(|(index, novel)| ParsedNovel {
                    index,
                    toc_url: novel.toc_url.clone(),
                    title: novel.title.clone(),
                    files: Vec::new(),
                })
                .collect(),
            manifest,
        }
    }

    #[test]
    fn creates_missing_novels_and_keeps_manifest_order() {
        let archive = parsed(&[
            ("https://example.com/1/", "One"),
            ("https://example.com/2/", "Two"),
        ]);
        let plan = plan_import(&archive, &[], ConflictPolicy::Skip);
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].index, 0);
        assert_eq!(plan.entries[0].action, ImportAction::Create);
        assert_eq!(plan.entries[1].index, 1);
        assert_eq!(plan.entries[1].action, ImportAction::Create);
        assert_eq!(plan.count(ImportAction::Create), 2);
        assert!(plan.entries.iter().all(|entry| !entry.reason.is_empty()));
    }

    #[test]
    fn skips_existing_novels_by_toc_url() {
        let archive = parsed(&[
            ("https://example.com/1/", "One"),
            ("https://example.com/2/", "Two"),
        ]);
        let existing = [ExistingNovel {
            toc_url: "https://example.com/1/".into(),
            title: "One".into(),
        }];
        let plan = plan_import(&archive, &existing, ConflictPolicy::Skip);
        assert_eq!(plan.entries[0].action, ImportAction::Skip);
        assert_eq!(plan.entries[1].action, ImportAction::Create);
        assert_eq!(plan.count(ImportAction::Skip), 1);
        assert_eq!(plan.count(ImportAction::Create), 1);
    }

    #[test]
    fn overwrites_existing_novels_when_requested() {
        let archive = parsed(&[("https://example.com/1/", "One")]);
        let existing = [ExistingNovel {
            toc_url: "https://example.com/1/".into(),
            title: "Old title".into(),
        }];
        let plan = plan_import(&archive, &existing, ConflictPolicy::Overwrite);
        assert_eq!(plan.entries[0].action, ImportAction::Overwrite);
        assert!(plan.entries[0].reason.contains("Old title"));
    }

    #[test]
    fn title_is_not_a_conflict_key() {
        let archive = parsed(&[("https://example.com/1/", "Same title")]);
        let existing = [ExistingNovel {
            toc_url: "https://example.com/2/".into(),
            title: "Same title".into(),
        }];
        let plan = plan_import(&archive, &existing, ConflictPolicy::Skip);
        assert_eq!(plan.entries[0].action, ImportAction::Create);
    }

    #[test]
    fn duplicate_existing_entries_are_harmless() {
        let archive = parsed(&[("https://example.com/1/", "One")]);
        let existing = [
            ExistingNovel {
                toc_url: "https://example.com/1/".into(),
                title: "One".into(),
            },
            ExistingNovel {
                toc_url: "https://example.com/1/".into(),
                title: "Another copy".into(),
            },
        ];
        let plan = plan_import(&archive, &existing, ConflictPolicy::Overwrite);
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].action, ImportAction::Overwrite);
        // The first entry for a toc_url wins.
        assert!(plan.entries[0].reason.contains("One"));
    }
}
