//! Virtual grouping is metadata over ordinary archive works, never local IDs.
use super::memory::{MetadataBudget, default_limit};
use super::{ArchiveManifest, FORMAT_VERSION, MAX_NOVELS, Result, TransferError};

/// A virtual parent's complete portable input set; bodies stay with members.
pub const VIRTUAL_INPUT_FILES: [&str; 6] = [
    "setting.ini", "replace.txt", "converter.yaml", "cover.jpg", "cover.png", "cover.jpeg",
];

pub fn is_virtual_input_file(path: &str) -> bool {
    VIRTUAL_INPUT_FILES.contains(&path)
}

pub(super) fn version(version: u32) -> Result<()> {
    if version == 1 || version == FORMAT_VERSION { Ok(()) }
    else { Err(TransferError::UnsupportedVersion(version)) }
}

pub(super) fn validate(manifest: &ArchiveManifest) -> Result<()> {
    let mut memory = MetadataBudget { used: 0, limit: default_limit() };
    memory.adopt_manifest(manifest)?;
    validate_with_budget(manifest, &mut memory)
}

pub(super) fn validate_with_budget(manifest: &ArchiveManifest, memory: &mut MetadataBudget) -> Result<()> {
    version(manifest.format_version)?;
    if manifest.novels.len() > MAX_NOVELS {
        return Err(TransferError::TooLarge("too many novels".into()));
    }
    let mut has_parents = false;
    for novel in &manifest.novels {
        if novel.virtual_members.is_some() {
            has_parents = true;
            if manifest.format_version < 2 {
                return Err(invalid("virtual structure requires archive format 2"));
            }
            if !crate::virtual_novel::is_portable_toc_url(&novel.toc_url) {
                return Err(invalid("virtual parent needs a portable identity URL"));
            }
            if novel.ncode.is_some() || novel.tags.iter().any(|tag| crate::virtual_novel::is_internal_tag(tag)) {
                return Err(invalid("virtual parent must not carry a library-local ncode or tag"));
            }
            if novel.files.iter().any(|file| !is_virtual_input_file(&file.path)) {
                return Err(invalid("virtual parent contains non-input files"));
            }
        } else if novel.ncode.as_deref().is_some_and(crate::virtual_novel::is_virtual_ncode)
            || novel.toc_url.starts_with("https://narou.rs/virtual/")
        {
            return Err(invalid("virtual parent is missing its member structure"));
        }
    }
    if !has_parents { return Ok(()); }

    // Borrow existing URLs; charge both scratch vectors before allocation. A
    // malicious reference list cannot turn validation into an unbounded map.
    let mut by_url = Vec::new();
    memory.reserve(&mut by_url, manifest.novels.len())?;
    by_url.extend(0..manifest.novels.len());
    by_url.sort_unstable_by(|left, right| manifest.novels[*left].toc_url.cmp(&manifest.novels[*right].toc_url));
    let mut owners = Vec::new();
    memory.reserve(&mut owners, manifest.novels.len())?;
    owners.resize(manifest.novels.len(), usize::MAX);
    for (parent_index, novel) in manifest.novels.iter().enumerate() {
        let Some(members) = &novel.virtual_members else { continue; };
        if members.is_empty() || members.len() > MAX_NOVELS {
            return Err(invalid("virtual parent needs a nonempty bounded member list"));
        }
        let matching_parent = by_url.partition_point(|index| manifest.novels[*index].toc_url < novel.toc_url);
        if by_url.get(matching_parent + 1).is_some_and(|index| manifest.novels[*index].toc_url == novel.toc_url) {
            return Err(invalid("ambiguous virtual parent URL"));
        }
        for url in members {
            let position = by_url.partition_point(|index| manifest.novels[*index].toc_url.as_str() < url.as_str());
            let Some(index) = by_url.get(position).copied().filter(|index| manifest.novels[*index].toc_url == *url) else {
                return Err(invalid("virtual member is missing from the archive"));
            };
            if by_url.get(position + 1).is_some_and(|index| manifest.novels[*index].toc_url == *url) {
                return Err(invalid("ambiguous virtual member URL"));
            }
            let member = &manifest.novels[index];
            if member.virtual_members.is_some() {
                return Err(invalid("virtual parents cannot be members"));
            }
            if !member.files.iter().any(|file| file.path == "toc.yaml") {
                return Err(invalid("virtual member is missing toc.yaml"));
            }
            if owners[index] != usize::MAX {
                return Err(invalid(if owners[index] == parent_index {
                    "duplicate virtual member"
                } else {
                    "member belongs to multiple virtual parents"
                }));
            }
            owners[index] = parent_index;
        }
    }
    memory.release(&owners);
    memory.release(&by_url);
    Ok(())
}

fn invalid(message: &str) -> TransferError { TransferError::InvalidArchive(message.into()) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::{ArchiveNovel, ArchiveFile, ConflictPolicy, build_archive, parse_archive, plan_manifest};

    fn manifest() -> ArchiveManifest {
        let members: Vec<_> = ["second", "first"].into_iter().map(|name| ArchiveNovel {
            toc_url: format!("https://example.test/{name}/"), title: name.into(),
            files: vec![ArchiveFile::new("toc.yaml", format!("title: {name}\n").into_bytes())],
            ..Default::default()
        }).collect();
        let parent = ArchiveNovel {
            toc_url: "https://narou.rs/virtual/0123456789abcdef0123456789abcdef".into(),
            title: "combined".into(),
            virtual_members: Some(members.iter().map(|member| member.toc_url.clone()).collect()),
            files: vec![ArchiveFile::new("setting.ini", b"enable_yokogaki=true\n".to_vec())],
            ..Default::default()
        };
        ArchiveManifest { novels: std::iter::once(parent).chain(members).collect(), ..Default::default() }
    }

    #[test]
    fn virtual_roundtrip_keeps_member_order_and_plans_members_before_parent() {
        let input = manifest();
        let plan = plan_manifest(&input, &[], ConflictPolicy::Skip);
        assert_eq!(plan.entries.iter().map(|entry| entry.index).collect::<Vec<_>>(), vec![1, 2, 0]);
        let parsed = parse_archive(&build_archive(&input).unwrap()).unwrap();
        assert!(parsed.manifest.novels[..2].iter().all(|novel| novel.virtual_members.is_none()));
        let parent = &parsed.manifest.novels[2];
        assert_eq!(parent.virtual_members, input.novels[0].virtual_members);
        assert_eq!(parsed.novels[2].files[0].0, "setting.ini");
    }

    #[test]
    fn malformed_virtual_graphs_are_rejected_before_archive_generation() {
        let input = manifest();
        let cases: &[fn(&mut ArchiveManifest)] = &[
            |m| m.novels[0].virtual_members = Some(vec![]),
            |m| m.novels[0].virtual_members.as_mut().unwrap().push("missing".into()),
            |m| { let url=m.novels[0].virtual_members.as_ref().unwrap()[0].clone(); m.novels[0].virtual_members.as_mut().unwrap().push(url); },
            |m| m.novels.push(m.novels[1].clone()),
            |m| m.novels.push(m.novels[0].clone()),
            |m| { let url=m.novels[0].toc_url.clone(); m.novels[0].virtual_members=Some(vec![url]); },
            |m| m.novels[0].files.push(ArchiveFile::new("toc.yaml", Vec::new())),
            |m| m.novels[0].ncode=Some("vs_1".into()),
            |m| m.novels[0].tags.push("vs_1".into()),
            |m| m.novels[0].toc_url="https://narou.rs/virtual/vs_1".into(),
            |m| m.novels[1].files.clear(),
            |m| m.format_version=1,
            |m| { let mut parent=m.novels[0].clone(); parent.toc_url="https://narou.rs/virtual/fedcba9876543210fedcba9876543210".into(); m.novels.push(parent); },
        ];
        for mutate in cases {
            let mut changed = input.clone();
            mutate(&mut changed);
            assert!(build_archive(&changed).is_err(), "{changed:?}");
        }
        let mut legacy = ArchiveManifest { format_version: 1, novels: vec![input.novels[1].clone()], ..Default::default() };
        assert!(parse_archive(&build_archive(&legacy).unwrap()).is_ok());
        legacy.novels[0].ncode=Some("vs_1".into());
        assert!(build_archive(&legacy).is_err());
    }

    #[test]
    fn member_reference_scratch_is_admitted_before_allocation() {
        let input=manifest();
        let mut memory=MetadataBudget { used:0, limit:usize::MAX };
        memory.adopt_manifest(&input).unwrap();
        memory.limit=memory.used;
        assert!(matches!(validate_with_budget(&input,&mut memory),Err(TransferError::TooLarge(_))));
    }
}
