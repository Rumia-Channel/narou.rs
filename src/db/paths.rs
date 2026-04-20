use std::path::{Path, PathBuf};

use super::novel_record::NovelRecord;

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

fn sanitize_path_component(value: &str) -> String {
    let invalid = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    let sanitized = value
        .chars()
        .map(|ch| {
            if invalid.contains(&ch) || ch.is_control() {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_end_matches([' ', '.']).trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "_".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{create_subdirectory_name, novel_dir_from_components};

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
}
