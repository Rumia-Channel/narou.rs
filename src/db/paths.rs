#[cfg(feature = "native-runtime")]
use std::fs;
#[cfg(feature = "native-runtime")]
use std::path::Component;
use std::path::{Path, PathBuf};

use super::novel_record::NovelRecord;
#[cfg(feature = "native-runtime")]
use crate::error::{NarouError, Result};

pub fn create_subdirectory_name(file_title: &str) -> String {
    let chars: String = if file_title.starts_with('n') {
        file_title.chars().skip(1).take(2).collect()
    } else {
        file_title.chars().take(2).collect()
    };
    chars.trim().to_string()
}

fn novel_dir_from_components_canonical(
    archive_root: &Path,
    sitename: &str,
    file_title: &str,
    use_subdirectory: bool,
) -> PathBuf {
    let safe_sitename = sanitize_path_component(sitename);
    let safe_file_title = sanitize_path_component(file_title);
    let mut dir = archive_root.join(&safe_sitename);
    if use_subdirectory {
        let subdirectory = sanitize_path_component(&create_subdirectory_name(&safe_file_title));
        if !subdirectory.is_empty() {
            dir.push(subdirectory);
        }
    }
    dir.push(safe_file_title);
    dir
}

pub fn novel_dir_from_components(
    archive_root: &Path,
    sitename: &str,
    file_title: &str,
    use_subdirectory: bool,
) -> PathBuf {
    resolve_existing_path_components(
        archive_root,
        &novel_dir_from_components_canonical(
            archive_root,
            sitename,
            file_title,
            use_subdirectory,
        ),
    )
}

pub fn novel_dir_for_record(archive_root: &Path, record: &NovelRecord) -> PathBuf {
    novel_dir_from_components(
        archive_root,
        &record.sitename,
        &record.file_title,
        record.use_subdirectory,
    )
}

/// Native-only: probes the filesystem for the canonical vs legacy directory
/// layout. Worker builds have no filesystem and always use the canonical
/// logical path from [`novel_dir_for_record`].
///
/// [`novel_dir_for_record`] already resolves historical `‛` (U+201B) escaped
/// directories component by component. This helper additionally checks the
/// legacy flat layout (without a subdirectory). Nothing is renamed and
/// `database.yaml` is never rewritten.
#[cfg(feature = "native-runtime")]
pub fn existing_novel_dir_for_record(archive_root: &Path, record: &NovelRecord) -> PathBuf {
    let canonical = novel_dir_for_record(archive_root, record);
    if canonical.exists() {
        return canonical;
    }

    let legacy =
        novel_dir_from_components(archive_root, &record.sitename, &record.file_title, false);
    if legacy.exists() {
        return legacy;
    }

    canonical
}

/// Native-only: resolves `candidate` under `root`, walking each missing
/// component toward an existing directory written under the historical
/// `‛`-escape naming rule. Every component is first tried verbatim; when it
/// does not exist, each deterministic legacy encoding variant is tried in
/// order. The first missing component wins the canonical name and every
/// deeper component is appended verbatim, so new directories always get the
/// canonical spelling and reads/writes land inside the directory that
/// already holds the novel's data. Candidates outside `root` are returned
/// unchanged.
#[cfg(feature = "native-runtime")]
pub(crate) fn resolve_existing_path_components(root: &Path, candidate: &Path) -> PathBuf {
    let Ok(remainder) = candidate.strip_prefix(root) else {
        return candidate.to_path_buf();
    };
    if candidate.exists() {
        return candidate.to_path_buf();
    }
    let mut components = remainder.components();
    let mut resolved = root.to_path_buf();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            resolved.push(component.as_os_str());
            continue;
        };
        let next = resolved.join(name);
        if next.exists() {
            resolved = next;
            continue;
        }
        let Some(name) = name.to_str() else {
            resolved.push(component.as_os_str());
            continue;
        };
        let mut found = None;
        for encoded in legacy_escaped_component_variants(name) {
            let alternate = resolved.join(&encoded);
            if alternate.exists() {
                found = Some(alternate);
                break;
            }
        }
        if let Some(found) = found {
            resolved = found;
            continue;
        }
        // First missing component: everything below is a new path that must
        // use the canonical spelling.
        resolved.push(name);
        resolved.push(components.as_path());
        break;
    }
    resolved
}

/// Portable (non-native) build: there is no filesystem to probe, so the
/// canonical path is always the resolved path.
#[cfg(not(feature = "native-runtime"))]
fn resolve_existing_path_components(_root: &Path, candidate: &Path) -> PathBuf {
    candidate.to_path_buf()
}

/// Deterministic legacy spellings for one path component, ordered by
/// likelihood. Two historical encodings were observed in narou.rb-era
/// archives:
///
/// * Marked: `‛` (U+201B) before every fullwidth form of a Windows-invalid
///   character (`：` `＊` `？` `＜` `＞` `＂` `｜` `＼`), `‛‛％` for `％`,
///   bare `％` for ASCII `%`, `‛．` for a name-final `．`/`.`, unmarked `．`
///   for a mid-name `.`, `‛‛` for a literal `‛`.
/// * Question-bare: identical except `？`/`?` keep the bare fullwidth `？`
///   without a marker (some directories mix marked colons with bare
///   question marks).
///
/// Only encodings that differ from `name` are returned.
#[cfg(feature = "native-runtime")]
fn legacy_escaped_component_variants(name: &str) -> Vec<String> {
    let mut variants = Vec::new();
    for keep_question_bare in [false, true] {
        let encoded = legacy_escaped_path_component(name, keep_question_bare);
        if encoded != name && !variants.contains(&encoded) {
            variants.push(encoded);
        }
    }
    variants
}

/// Encodes one path component under the historical `‛` (U+201B) marker
/// rule. Only used to probe for directories that already exist; new
/// directories always get the canonical spelling.
#[cfg(feature = "native-runtime")]
fn legacy_escaped_path_component(name: &str, keep_question_bare: bool) -> String {
    let mut escaped = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{201b}' => escaped.push_str("\u{201b}\u{201b}"),
            '％' => escaped.push_str("\u{201b}\u{201b}％"),
            '%' => escaped.push('％'),
            '?' if keep_question_bare => escaped.push('？'),
            '？' if keep_question_bare => escaped.push('？'),
            ':' | '：' => escaped.push_str("\u{201b}："),
            '*' | '＊' => escaped.push_str("\u{201b}＊"),
            '?' | '？' => escaped.push_str("\u{201b}？"),
            '"' | '＂' | '”' => escaped.push_str("\u{201b}＂"),
            '<' | '＜' => escaped.push_str("\u{201b}＜"),
            '>' | '＞' => escaped.push_str("\u{201b}＞"),
            '|' | '｜' => escaped.push_str("\u{201b}｜"),
            '\\' | '＼' => escaped.push_str("\u{201b}＼"),
            '.' | '．' if chars.peek().is_none() => escaped.push_str("\u{201b}．"),
            '.' => escaped.push('．'),
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub fn sanitize_windows_filename_component_with_limit(
    value: &str,
    limit: Option<usize>,
    invalid_replacement: Option<char>,
    fallback: &str,
) -> String {
    let sanitized = value
        .chars()
        .filter_map(|ch| {
            if ch.is_control() {
                None
            } else if matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                invalid_replacement
            } else {
                Some(ch)
            }
        })
        .collect::<String>();
    let limited = limit
        .map(|limit| sanitized.chars().take(limit).collect::<String>())
        .unwrap_or(sanitized);
    let mut candidate = limited.trim_end_matches([' ', '.']).to_string();
    if candidate.is_empty() || candidate == "." || candidate == ".." {
        candidate = fallback.to_string();
    }
    if is_windows_reserved_name(&candidate) {
        candidate.insert(0, '_');
    }
    candidate
}

pub fn sanitize_path_component(value: &str) -> String {
    sanitize_windows_filename_component_with_limit(value, None, Some('_'), "_")
}

/// Native-only: canonicalization and reparse-point containment checks require
/// the filesystem. Worker storage adapters enforce their own key containment.
#[cfg(feature = "native-runtime")]
pub fn ensure_within_archive_root(path: &Path, root: &Path) -> Result<PathBuf> {
    let absolute_root = absolute_normalized_path(root)?;
    let canonical_root = canonical_existing_path(root)?;
    let absolute_path = absolute_normalized_path(path)?;
    let lexical_root = if absolute_path.starts_with(&absolute_root) {
        &absolute_root
    } else if absolute_path.starts_with(&canonical_root) {
        // Callers may pass the canonical path returned by an earlier check.
        // Keep component and reparse-point checks; do not case-fold paths.
        &canonical_root
    } else {
        return Err(NarouError::Database(format!(
            "path escapes archive root: {}",
            path.display()
        )));
    };
    reject_escaping_reparse_points(&absolute_path, lexical_root, &canonical_root)?;
    if absolute_path.exists() {
        let canonical_path = canonical_existing_path(&absolute_path)?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(NarouError::Database(format!(
                "path escapes archive root after canonicalization: {}",
                path.display()
            )));
        }
        return Ok(canonical_path);
    }
    let nearest_existing = nearest_existing_ancestor(&absolute_path).ok_or_else(|| {
        NarouError::Database(format!(
            "path has no existing ancestor under archive root: {}",
            path.display()
        ))
    })?;
    let canonical_ancestor = canonical_existing_path(&nearest_existing)?;
    if !canonical_ancestor.starts_with(&canonical_root) {
        return Err(NarouError::Database(format!(
            "path escapes archive root after canonicalization: {}",
            path.display()
        )));
    }
    let remainder = absolute_path
        .strip_prefix(&nearest_existing)
        .unwrap_or_else(|_| Path::new(""));
    Ok(normalize_path(&canonical_ancestor.join(remainder)))
}

fn is_windows_reserved_name(value: &str) -> bool {
    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .trim_end_matches([' ', '.']);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CONIN$"
            | "CONOUT$"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(feature = "native-runtime")]
fn absolute_normalized_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(normalize_path(&strip_windows_verbatim_prefix(&absolute)))
}

#[cfg(feature = "native-runtime")]
fn canonical_existing_path(path: &Path) -> Result<PathBuf> {
    Ok(normalize_path(&strip_windows_verbatim_prefix(&fs::canonicalize(path)?)))
}

#[cfg(feature = "native-runtime")]
fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if current.exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

#[cfg(feature = "native-runtime")]
fn reject_escaping_reparse_points(
    path: &Path,
    root: &Path,
    canonical_root: &Path,
) -> Result<()> {
    let mut current = root.to_path_buf();
    if path == root {
        return Ok(());
    }
    let remainder = path.strip_prefix(root).map_err(|_| {
        NarouError::Database(format!(
            "path escapes archive root: {}",
            path.display()
        ))
    })?;
    for component in remainder.components() {
        current.push(component.as_os_str());
        if !current.exists() {
            break;
        }
        let metadata = fs::symlink_metadata(&current)?;
        if is_symlink_like(&metadata) {
            let canonical = canonical_existing_path(&current)?;
            if !canonical.starts_with(canonical_root) {
                return Err(NarouError::Database(format!(
                    "symlink or junction escapes archive root: {}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(feature = "native-runtime")]
#[cfg(windows)]
fn is_symlink_like(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(feature = "native-runtime")]
#[cfg(not(windows))]
fn is_symlink_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(feature = "native-runtime")]
fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

#[cfg(feature = "native-runtime")]
fn strip_windows_verbatim_prefix(path: &Path) -> PathBuf {
    if cfg!(windows) {
        let raw = path.to_string_lossy();
        if raw.starts_with("\\\\?\\") {
            return PathBuf::from(raw.trim_start_matches("\\\\?\\"));
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        create_subdirectory_name, ensure_within_archive_root, novel_dir_from_components,
        sanitize_path_component, sanitize_windows_filename_component_with_limit,
    };

    #[test]
    fn subdirectory_name_matches_narou_rb_rule() {
        assert_eq!(create_subdirectory_name("n8858hb title"), "88");
        assert_eq!(create_subdirectory_name("２１年版"), "２１");
        assert_eq!(create_subdirectory_name(" n8858hb"), "n");
    }

    #[test]
    fn novel_dir_from_components_sanitizes_path_traversal_sequences() {
        let archive_root = Path::new("archive");
        let path = novel_dir_from_components(archive_root, "..\\evil", "..", false);

        assert_eq!(path, PathBuf::from("archive").join(".._evil").join("_"));
    }

    #[test]
    fn novel_dir_from_components_sanitizes_absolute_path_markers() {
        let archive_root = Path::new("archive");
        let path = novel_dir_from_components(archive_root, "C:\\windows", "/etc/passwd", false);

        assert_eq!(
            path,
            PathBuf::from("archive")
                .join("C__windows")
                .join("_etc_passwd")
        );
    }

    #[test]
    fn sanitize_path_component_handles_reserved_names_and_controls() {
        assert_eq!(sanitize_path_component("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_path_component("aux"), "_aux");
        assert_eq!(sanitize_path_component("abc\0\x1F\x7Fdef"), "abcdef");
        assert_eq!(sanitize_path_component("trail. "), "trail");
        assert_eq!(sanitize_path_component("全角 名称 "), "全角 名称");
        assert_eq!(sanitize_path_component(""), "_");
    }

    #[test]
    fn sanitize_windows_filename_component_with_limit_respects_existing_behavior() {
        assert_eq!(
            sanitize_windows_filename_component_with_limit("ab/cd", Some(4), Some('_'), "_"),
            "ab_c"
        );
        assert_eq!(
            sanitize_windows_filename_component_with_limit("CON.txt", None, None, "output"),
            "_CON.txt"
        );
        assert_eq!(
            sanitize_windows_filename_component_with_limit("name\twith\ncontrols", None, None, "output"),
            "namewithcontrols"
        );
        assert_eq!(
            sanitize_windows_filename_component_with_limit("   ", None, None, "output"),
            "output"
        );
    }

    /// 予約名判定は limit 適用と末尾の空白/ドット除去の**後**に行う。
    /// 先に判定すると、切り詰めで予約名になる入力 (例: `"auxiliary"` →
    /// `"aux"`) を素通ししてしまう。`platform::object_store` の
    /// `sanitize_key_component_with_limit` と順序を一致させる。
    #[test]
    fn sanitize_windows_filename_component_checks_reserved_after_limit() {
        assert_eq!(
            sanitize_windows_filename_component_with_limit("auxiliary", Some(3), Some('_'), "_"),
            "_aux"
        );
        assert_eq!(
            crate::platform::object_store::sanitize_key_component_with_limit("auxiliary", Some(3)),
            "_aux"
        );
    }

    #[test]
    fn ensure_within_archive_root_returns_canonical_absolute_path() {
        let temp = tempfile::tempdir().unwrap();
        let archive_root = temp.path().join("archive");
        let novel_dir = archive_root.join("site").join("title");
        std::fs::create_dir_all(&novel_dir).unwrap();

        let resolved = ensure_within_archive_root(&novel_dir, &archive_root).unwrap();
        let canonical_root = super::canonical_existing_path(&archive_root).unwrap();

        assert!(resolved.is_absolute());
        assert!(resolved.starts_with(&canonical_root));
    }

    #[cfg(windows)]
    #[test]
    fn canonical_paths_remain_valid_under_a_case_aliased_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("ArchiveRoot")).unwrap();
        let root = temp.path().join("archiveroot");
        for existing in [false, true] {
            let candidate = root.join("novel").join("toc.yaml");
            if existing {
                std::fs::create_dir_all(candidate.parent().unwrap()).unwrap();
                std::fs::write(&candidate, b"title: fixture").unwrap();
            }
            let canonical = ensure_within_archive_root(&candidate, &root).unwrap();
            assert_eq!(ensure_within_archive_root(&canonical, &root).unwrap(), canonical);
        }
    }

    #[test]
    fn ensure_within_archive_root_rejects_lexical_escape() {
        let temp = tempfile::tempdir().unwrap();
        let archive_root = temp.path().join("archive");
        std::fs::create_dir_all(&archive_root).unwrap();

        let err = ensure_within_archive_root(&archive_root.join("..").join("escape"), &archive_root)
            .unwrap_err();

        assert!(err.to_string().contains("archive root"));
    }

    #[cfg(feature = "native-runtime")]
    mod native {
        use chrono::Utc;

        use super::super::{
            existing_novel_dir_for_record, legacy_escaped_component_variants,
            legacy_escaped_path_component, resolve_existing_path_components,
        };
        use crate::db::novel_record::NovelRecord;

        fn record(file_title: &str, use_subdirectory: bool) -> NovelRecord {
            NovelRecord {
                id: 1,
                author: "作者".into(),
                title: "作品".into(),
                file_title: file_title.into(),
                toc_url: "https://example.test/work/1".into(),
                sitename: "小説家になろう".into(),
                novel_type: 1,
                end: false,
                last_update: Utc::now(),
                new_arrivals_date: None,
                use_subdirectory,
                general_firstup: None,
                novelupdated_at: None,
                general_lastup: None,
                last_mail_date: None,
                tags: vec![],
                ncode: None,
                domain: None,
                general_all_no: Some(1),
                length: None,
                suspend: false,
                is_narou: false,
                last_check_date: None,
                convert_failure: false,
                requires_login: false,
                login_session: None,
                extra_fields: Default::default(),
            }
        }

        #[test]
        fn legacy_escape_marks_invalid_fullwidths_and_specials() {
            assert_eq!(
                legacy_escaped_path_component("n6247ex 旧題：異世界", false),
                "n6247ex 旧題\u{201b}：異世界"
            );
            assert_eq!(
                legacy_escaped_path_component("Re：Creator？", false),
                "Re\u{201b}：Creator\u{201b}？"
            );
            // The question-bare era keeps `？` unmarked.
            assert_eq!(
                legacy_escaped_path_component("Re：Creator？", true),
                "Re\u{201b}：Creator？"
            );
            assert_eq!(
                legacy_escaped_path_component("やっと100%", false),
                "やっと100％"
            );
            assert_eq!(
                legacy_escaped_path_component("予想していない1％", false),
                "予想していない1\u{201b}\u{201b}％"
            );
            assert_eq!(
                legacy_escaped_path_component("修正ver．", false),
                "修正ver\u{201b}．"
            );
            assert_eq!(
                legacy_escaped_path_component("mid．name", false),
                "mid．name"
            );
        }

        #[test]
        fn escaped_variants_cover_both_question_mark_eras() {
            let variants = legacy_escaped_component_variants("なぜ？ 旧題：話");
            assert_eq!(
                variants,
                vec![
                    "なぜ\u{201b}？ 旧題\u{201b}：話".to_string(),
                    "なぜ？ 旧題\u{201b}：話".to_string(),
                ]
            );
            // A name without escapable characters yields no extra candidates.
            assert!(legacy_escaped_component_variants("plain title").is_empty());
        }

        #[test]
        fn resolver_walks_missing_components_to_ancestors() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("archive");
            let legacy_dir = root.join("site").join("n6247ex 旧題\u{201b}：異世界");
            std::fs::create_dir_all(legacy_dir.join("本文")).unwrap();

            // A missing leaf inside an existing escaped novel directory
            // resolves into that directory verbatim.
            let candidate = root
                .join("site")
                .join("n6247ex 旧題：異世界")
                .join("本文")
                .join("2 第二話.yaml");
            assert_eq!(
                resolve_existing_path_components(&root, &candidate),
                legacy_dir.join("本文").join("2 第二話.yaml")
            );

            // An entirely missing leaf keeps the canonical spelling.
            let missing = root.join("site").join("new title").join("toc.yaml");
            assert_eq!(
                resolve_existing_path_components(&root, &missing),
                missing
            );
        }

        #[test]
        fn resolver_prefers_canonical_over_escaped() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("archive");
            std::fs::create_dir_all(root.join("site").join("n1 旧題：x")).unwrap();
            std::fs::create_dir_all(root.join("site").join("n1 旧題\u{201b}：x")).unwrap();

            let candidate = root.join("site").join("n1 旧題：x").join("toc.yaml");
            assert_eq!(
                resolve_existing_path_components(&root, &candidate),
                root.join("site").join("n1 旧題：x").join("toc.yaml")
            );
        }

        #[test]
        fn existing_dir_resolves_escaped_flat_layout() {
            let temp = tempfile::tempdir().unwrap();
            let archive_root = temp.path().join("小説データ");
            let escaped = archive_root
                .join("小説家になろう")
                .join("n6247ex 旧題\u{201b}：異世界");
            std::fs::create_dir_all(&escaped).unwrap();

            let record = record("n6247ex 旧題：異世界", false);
            assert_eq!(existing_novel_dir_for_record(&archive_root, &record), escaped);
        }

        #[test]
        fn existing_dir_resolves_escaped_subdirectory_layout() {
            let temp = tempfile::tempdir().unwrap();
            let archive_root = temp.path().join("小説データ");
            let escaped = archive_root
                .join("小説家になろう")
                .join("62")
                .join("n6247ex 旧題\u{201b}：異世界");
            std::fs::create_dir_all(&escaped).unwrap();

            let record = record("n6247ex 旧題：異世界", true);
            assert_eq!(existing_novel_dir_for_record(&archive_root, &record), escaped);
        }

        #[test]
        fn existing_dir_prefers_canonical_over_escaped() {
            let temp = tempfile::tempdir().unwrap();
            let archive_root = temp.path().join("小説データ");
            let canonical = archive_root
                .join("小説家になろう")
                .join("n6247ex 旧題：異世界");
            let escaped = archive_root
                .join("小説家になろう")
                .join("n6247ex 旧題\u{201b}：異世界");
            std::fs::create_dir_all(&canonical).unwrap();
            std::fs::create_dir_all(&escaped).unwrap();

            let record = record("n6247ex 旧題：異世界", false);
            assert_eq!(existing_novel_dir_for_record(&archive_root, &record), canonical);
        }

        #[test]
        fn existing_dir_resolves_question_bare_era() {
            let temp = tempfile::tempdir().unwrap();
            let archive_root = temp.path().join("小説データ");
            // `？` bare, `：` marked — the observed mixed question era.
            let mixed = archive_root
                .join("小説家になろう")
                .join("n0000aa 妹の親友？ 旧題\u{201b}：話");
            std::fs::create_dir_all(&mixed).unwrap();

            let record = record("n0000aa 妹の親友？ 旧題：話", false);
            assert_eq!(existing_novel_dir_for_record(&archive_root, &record), mixed);
        }

        #[test]
        fn existing_dir_returns_canonical_when_nothing_matches() {
            let temp = tempfile::tempdir().unwrap();
            let archive_root = temp.path().join("小説データ");
            std::fs::create_dir_all(archive_root.join("小説家になろう")).unwrap();

            let record = record("n6247ex 旧題：異世界", false);
            assert_eq!(
                existing_novel_dir_for_record(&archive_root, &record),
                archive_root
                    .join("小説家になろう")
                    .join("n6247ex 旧題：異世界")
            );
        }
    }
}
