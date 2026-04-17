use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use crate::downloader::TocObject;

use super::settings::NovelSettings;

pub(crate) fn create_output_text_path(
    settings: &NovelSettings,
    id: i64,
    novel_dir: &Path,
    toc: &TocObject,
) -> PathBuf {
    novel_dir.join(create_output_text_filename(settings, id, toc))
}

pub(crate) fn create_output_text_path_for_textfile(
    settings: &NovelSettings,
    converted_text: &str,
) -> PathBuf {
    settings
        .archive_path
        .join(create_output_text_filename_for_textfile(
            settings,
            converted_text,
        ))
}

pub(crate) fn create_output_text_filename(
    settings: &NovelSettings,
    id: i64,
    toc: &TocObject,
) -> String {
    if !settings.output_filename.trim().is_empty() {
        return ensure_txt_extension(&sanitize_filename_for_output(&settings.output_filename));
    }

    if convert_filename_to_ncode() {
        let record = crate::db::with_database(|db| Ok(db.get(id).cloned()))
            .ok()
            .flatten();
        let domain = record
            .as_ref()
            .and_then(|r| r.domain.clone())
            .or_else(|| extract_domain(&toc.toc_url))
            .unwrap_or_else(|| "unknown".to_string());
        let ncode = record
            .as_ref()
            .and_then(|r| r.ncode.clone())
            .or_else(|| extract_ncode_like(&toc.toc_url))
            .unwrap_or_else(|| sanitize_filename_for_output(&toc.title));
        return format!("{}_{}.txt", domain.replace('.', "_"), ncode);
    }

    let author = if settings.novel_author.is_empty() {
        &toc.author
    } else {
        &settings.novel_author
    };
    let title = if settings.novel_title.is_empty() {
        &toc.title
    } else {
        &settings.novel_title
    };
    ensure_txt_extension(&sanitize_filename_for_output(&format!(
        "[{}] {}",
        author, title
    )))
}

fn convert_filename_to_ncode() -> bool {
    crate::db::with_database(|db| {
        let settings: HashMap<String, serde_yaml::Value> = db
            .inventory()
            .load("local_setting", crate::db::inventory::InventoryScope::Local)?;
        Ok(settings
            .get("convert.filename-to-ncode")
            .and_then(|value| value.as_bool())
            .unwrap_or(false))
    })
    .unwrap_or(false)
}

fn sanitize_filename_for_output(name: &str) -> String {
    let invalid = ['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'];
    let cleaned: String = name.chars().filter(|c| !invalid.contains(c)).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "output".to_string()
    } else {
        match output_filename_length_limit() {
            Some(limit) => trimmed.chars().take(limit).collect(),
            None => trimmed.to_string(),
        }
    }
}

fn output_filename_length_limit() -> Option<usize> {
    crate::compat::load_local_setting_value("ebook-filename-length-limit")
        .and_then(|value| match value {
            serde_yaml::Value::Number(number) => number.as_i64(),
            serde_yaml::Value::String(raw) => raw.parse::<i64>().ok(),
            _ => None,
        })
        .map(|limit| limit.max(0) as usize)
}

fn ensure_txt_extension(filename: &str) -> String {
    if filename.to_lowercase().ends_with(".txt") {
        filename.to_string()
    } else {
        format!("{filename}.txt")
    }
}

fn create_output_text_filename_for_textfile(
    settings: &NovelSettings,
    converted_text: &str,
) -> String {
    if !settings.output_filename.trim().is_empty() {
        return ensure_txt_extension(&sanitize_filename_for_output(&settings.output_filename));
    }

    let (title, author) = extract_title_and_author_from_text(converted_text);
    if convert_filename_to_ncode() {
        return ensure_txt_extension(&sanitize_filename_for_output(&format!("text_{}", title)));
    }

    ensure_txt_extension(&sanitize_filename_for_output(&format!(
        "[{}] {}",
        author, title
    )))
}

fn extract_title_and_author_from_text(text: &str) -> (String, String) {
    let mut lines = text.lines();
    let title = lines.next().unwrap_or("").to_string();
    let author = lines.next().unwrap_or("").to_string();
    (title, author)
}

fn extract_domain(url: &str) -> Option<String> {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    without_scheme
        .split('/')
        .next()
        .filter(|domain| !domain.is_empty())
        .map(str::to_string)
}

fn extract_ncode_like(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .find(|part| !part.is_empty() && *part != "works")
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::create_output_text_path_for_textfile;
    use crate::converter::settings::NovelSettings;

    #[test]
    fn textfile_output_path_uses_title_and_author_from_text() {
        let root = std::env::temp_dir().join(format!(
            "narou-rs-textfile-output-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();

        let mut settings = NovelSettings::default();
        settings.archive_path = root.clone();

        let path = create_output_text_path_for_textfile(&settings, "タイトル\n作者\n本文");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("[作者] タイトル.txt")
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
