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

pub fn novel_dir_for_record(archive_root: &Path, record: &NovelRecord) -> PathBuf {
    let mut dir = archive_root.join(&record.sitename);
    if record.use_subdirectory {
        let subdirectory = create_subdirectory_name(&record.file_title);
        if !subdirectory.is_empty() {
            dir.push(subdirectory);
        }
    }
    dir.push(&record.file_title);
    dir
}

pub fn existing_novel_dir_for_record(archive_root: &Path, record: &NovelRecord) -> PathBuf {
    let canonical = novel_dir_for_record(archive_root, record);
    if canonical.exists() {
        return canonical;
    }

    let legacy = archive_root.join(&record.sitename).join(&record.file_title);
    if legacy.exists() {
        return legacy;
    }

    canonical
}

#[cfg(test)]
mod tests {
    use super::create_subdirectory_name;

    #[test]
    fn subdirectory_name_matches_narou_rb_rule() {
        assert_eq!(create_subdirectory_name("n8858hb title"), "88");
        assert_eq!(create_subdirectory_name("２１年版"), "２１");
        assert_eq!(create_subdirectory_name(" n8858hb"), "n");
    }
}
