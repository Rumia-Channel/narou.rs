use std::fs;
use std::path::{Component, Path, PathBuf};

use super::novel_record::NovelRecord;
use crate::error::{NarouError, Result};

pub fn create_subdirectory_name(file_title: &str) -> String {
    let chars: String = if file_title.starts_with('n') {
        file_title.chars().skip(1).take(2).collect()
    } else {
        file_title.chars().take(2).collect()
    };
    chars.trim().to_string()
}

pub fn novel_dir_from_components(
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

pub fn novel_dir_for_record(archive_root: &Path, record: &NovelRecord) -> PathBuf {
    novel_dir_from_components(
        archive_root,
        &record.sitename,
        &record.file_title,
        record.use_subdirectory,
    )
}

pub fn existing_novel_dir_for_record(archive_root: &Path, record: &NovelRecord) -> PathBuf {
    let canonical = novel_dir_for_record(archive_root, record);
    if canonical.exists() {
        return canonical;
    }

    let legacy = novel_dir_from_components(archive_root, &record.sitename, &record.file_title, false);
    if legacy.exists() {
        return legacy;
    }

    canonical
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
    let mut candidate = sanitized.trim_end_matches([' ', '.']).to_string();
    if candidate.is_empty() || candidate == "." || candidate == ".." {
        candidate = fallback.to_string();
    }
    if is_windows_reserved_name(&candidate) {
        candidate.insert(0, '_');
    }
    if let Some(limit) = limit {
        candidate = candidate.chars().take(limit).collect::<String>();
        candidate = candidate.trim_end_matches([' ', '.']).to_string();
    }
    if candidate.is_empty() || candidate == "." || candidate == ".." {
        fallback.to_string()
    } else {
        candidate
    }
}

pub fn sanitize_path_component(value: &str) -> String {
    sanitize_windows_filename_component_with_limit(value, None, Some('_'), "_")
}

pub fn ensure_within_archive_root(path: &Path, root: &Path) -> Result<PathBuf> {
    let absolute_root = absolute_normalized_path(root)?;
    let canonical_root = canonical_existing_path(root)?;
    let absolute_path = absolute_normalized_path(path)?;
    if !absolute_path.starts_with(&absolute_root) {
        return Err(NarouError::Database(format!(
            "path escapes archive root: {}",
            path.display()
        )));
    }
    reject_escaping_reparse_points(&absolute_path, &absolute_root, &canonical_root)?;
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

fn absolute_normalized_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(normalize_path(&strip_windows_verbatim_prefix(&absolute)))
}

fn canonical_existing_path(path: &Path) -> Result<PathBuf> {
    Ok(normalize_path(&strip_windows_verbatim_prefix(&fs::canonicalize(path)?)))
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if current.exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

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

#[cfg(windows)]
fn is_symlink_like(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_symlink_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

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

    #[test]
    fn ensure_within_archive_root_rejects_lexical_escape() {
        let temp = tempfile::tempdir().unwrap();
        let archive_root = temp.path().join("archive");
        std::fs::create_dir_all(&archive_root).unwrap();

        let err = ensure_within_archive_root(&archive_root.join("..").join("escape"), &archive_root)
            .unwrap_err();

        assert!(err.to_string().contains("archive root"));
    }
}
