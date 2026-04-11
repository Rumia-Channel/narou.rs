use std::path::PathBuf;

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

pub fn section_needs_update(
    section_dir: &PathBuf,
    subtitle: &SubtitleInfo,
    new_section: &SectionElement,
) -> bool {
    let filename = format!("{} {}.yaml", subtitle.index, subtitle.file_subtitle);
    let path = section_dir.join(&filename);
    if !path.exists() {
        return true;
    }
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(existing) = serde_yaml::from_str::<SectionElement>(&content) {
            let old_hash = compute_section_hash(&existing);
            let new_hash = compute_section_hash(new_section);
            return old_hash != new_hash;
        }
    }
    true
}

pub fn save_section_file(
    section_dir: &PathBuf,
    subtitle: &SubtitleInfo,
    section: &SectionElement,
) -> Result<()> {
    let filename = format!("{} {}.yaml", subtitle.index, subtitle.file_subtitle);
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
    let filename = format!("{} {}.html", subtitle.index, subtitle.file_subtitle);
    let path = raw_dir.join(filename);
    std::fs::write(&path, raw_html)?;
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
