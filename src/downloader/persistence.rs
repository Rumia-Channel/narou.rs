use std::path::{Path, PathBuf};

use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::error::Result;

use super::types::{SectionElement, SectionFile, SubtitleInfo, TocFile};

pub fn compute_section_hash(section: &SectionElement) -> String {
    let mut hasher = Sha256::new();
    hasher.update(section.body.as_bytes());
    hasher.update(section.introduction.as_bytes());
    hasher.update(section.postscript.as_bytes());
    hex::encode(hasher.finalize())
}

fn section_filename(subtitle: &SubtitleInfo) -> String {
    let safe_subtitle = crate::downloader::util::sanitize_filename(&subtitle.file_subtitle);
    format!("{} {}.yaml", subtitle.index, safe_subtitle)
}

pub fn section_needs_update(
    section_dir: &PathBuf,
    subtitle: &SubtitleInfo,
    new_section: &SectionElement,
) -> bool {
    let path = section_dir.join(section_filename(subtitle));
    if !path.exists() {
        return true;
    }
    if let Some(existing) = load_section_file(&path) {
        let old_hash = compute_section_hash(&existing.element);
        let new_hash = compute_section_hash(new_section);
        return old_hash != new_hash;
    }
    true
}

pub fn resolve_section_file_path(
    section_dir: &Path,
    subtitle: &SubtitleInfo,
) -> Option<PathBuf> {
    let exact = section_dir.join(section_filename(subtitle));
    if exact.exists() {
        return Some(exact);
    }
    find_section_file_by_index(section_dir, &subtitle.index)
}

pub fn find_section_file_by_index(section_dir: &Path, index: &str) -> Option<PathBuf> {
    let prefix = format!("{} ", index);
    let entries = std::fs::read_dir(section_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".yaml") {
            continue;
        }
        let stem = &name[..name.len() - 5];
        if stem == index || stem.starts_with(&prefix) {
            return Some(entry.path());
        }
    }
    None
}

pub fn load_section_file(path: &PathBuf) -> Option<SectionFile> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&content).ok()
}

pub fn save_section_file(
    section_dir: &PathBuf,
    subtitle: &SubtitleInfo,
    section: &SectionElement,
) -> Result<()> {
    let safe_subtitle = crate::downloader::util::sanitize_filename(&subtitle.file_subtitle);
    let filename = format!("{} {}.yaml", subtitle.index, safe_subtitle);
    let path = section_dir.join(filename);
    let file_data = SectionFile {
        index: subtitle.index.clone(),
        href: subtitle.href.clone(),
        chapter: subtitle.chapter.clone(),
        subchapter: subtitle.subchapter.clone(),
        subtitle: subtitle.subtitle.clone(),
        file_subtitle: subtitle.file_subtitle.clone(),
        subdate: subtitle.subdate.clone(),
        subupdate: subtitle.subupdate.clone(),
        download_time: Some(Utc::now().format("%Y-%m-%d %H:%M:%S%.6f %z").to_string()),
        element: section.clone(),
    };
    let yaml_body = serde_yaml::to_string(&file_data)?;
    let content = format!("---\n{}\n", yaml_body);
    std::fs::write(&path, content)?;
    Ok(())
}

pub fn save_raw_file(raw_dir: &PathBuf, subtitle: &SubtitleInfo, raw_html: &str) -> Result<()> {
    let safe_subtitle = crate::downloader::util::sanitize_filename(&subtitle.file_subtitle);
    let filename = format!("{} {}.html", subtitle.index, safe_subtitle);
    let path = raw_dir.join(filename);
    std::fs::write(&path, raw_html)?;
    Ok(())
}

pub fn move_file_to_dir(path: &Path, dst_dir: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dst_dir)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| crate::error::NarouError::Io(std::io::Error::other("invalid file name")))?;
    std::fs::rename(path, dst_dir.join(file_name))?;
    Ok(())
}

pub fn remove_dir_if_empty(path: &Path) -> Result<()> {
    if path.is_dir() && std::fs::read_dir(path)?.next().is_none() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

pub fn load_toc_file(novel_dir: &PathBuf) -> Option<TocFile> {
    let path = novel_dir.join("toc.yaml");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_yaml::from_str(&content).ok()
}

pub fn fix_yaml_block_scalar(yaml: &str) -> String {
    let re = regex::Regex::new(r"(?m)^story:\s*\|[-+]?\s*$").unwrap();
    let result = re.replace_all(yaml, "story: |-").to_string();
    result
}

pub fn save_toc_file(novel_dir: &PathBuf, toc: &TocFile) -> Result<()> {
    let path = novel_dir.join("toc.yaml");
    let yaml_body = serde_yaml::to_string(toc)?;
    let content = format!("---\n{}\n", fix_yaml_block_scalar(&yaml_body));
    std::fs::write(&path, content)?;
    Ok(())
}

pub fn ensure_default_files(novel_dir: &PathBuf, title: &str, author: &str, toc_url: &str) {
    let setting_path = novel_dir.join("setting.ini");
    if !setting_path.exists() {
        let default_ini = crate::converter::ini::IniData::new();
        let content = default_ini.to_ini_string();
        let _ = std::fs::write(&setting_path, content);
    }

    let replace_path = novel_dir.join("replace.txt");
    if !replace_path.exists() {
        let content = format!(
            "; 単純置換用ファイル\n;\n; 対象小説情報\n; タイトル: {}\n; 作者: {}\n; URL: {}\n;\n; 書式\n; 置換対象<tab>置換文字\n;\n; サンプル\n; 一〇歳\t十歳\n; 第一章\t［＃ゴシック体］第一章［＃ゴシック体終わり］\n;\n; 正規表現での置換などは converter.yaml で対応して下さい\n",
            title, author, toc_url
        );
        let _ = std::fs::write(&replace_path, content);
    }
}
