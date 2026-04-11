use std::fs;
use std::path::{Path, PathBuf};

use narou_rs::converter::NovelConverter;
use narou_rs::converter::settings::NovelSettings;
use narou_rs::downloader::{SectionElement, SectionFile, SubtitleInfo, TocObject};

#[test]
fn story_html_is_converted_before_text_pipeline() {
    let toc = TocObject {
        title: "title".to_string(),
        author: "author".to_string(),
        toc_url: String::new(),
        story: Some("甲<br />\n乙".to_string()),
        subtitles: vec![SubtitleInfo {
            index: "1".to_string(),
            href: "/1/".to_string(),
            chapter: String::new(),
            subchapter: String::new(),
            subtitle: "第一話".to_string(),
            file_subtitle: "第一話".to_string(),
            subdate: String::new(),
            subupdate: None,
            download_time: None,
        }],
        novel_type: Some(1),
    };
    let sections = vec![SectionFile {
        index: "1".to_string(),
        href: "/1/".to_string(),
        chapter: String::new(),
        subchapter: String::new(),
        subtitle: "第一話".to_string(),
        file_subtitle: "第一話".to_string(),
        subdate: String::new(),
        subupdate: None,
        download_time: None,
        element: SectionElement {
            data_type: "text".to_string(),
            introduction: String::new(),
            postscript: String::new(),
            body: "本文".to_string(),
        },
    }];

    let mut converter = NovelConverter::new(NovelSettings::default());
    let output = converter.convert_novel(&toc, &sections).unwrap();

    assert!(!output.contains("<br"));
    assert!(!output.contains("ｂｒ"));
    assert!(output.contains("甲\n乙"));
}

#[test]
fn kakuyomu_sample_matches_narou_rb_reference_byte_for_byte() {
    let root = std::env::current_dir().unwrap();
    let sample_root = root.join("sample");
    let reference = find_file_named(&sample_root, "kakuyomu_jp_1177354055617350769.txt")
        .expect("reference output fixture");
    let novel_dir = find_dir_starting_with(&sample_root.join("novel"), "1177354055617350769")
        .expect("downloaded kakuyomu sample");

    let mut converter = NovelConverter::new(NovelSettings::default());
    let output = converter.convert_novel_by_id(2, &novel_dir).unwrap();

    let reference_bytes = fs::read(reference).unwrap();
    let output_bytes = fs::read(output).unwrap();
    assert_eq!(output_bytes, reference_bytes);
}

fn find_file_named(root: &Path, name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_file_named(&path, name) {
                return Some(found);
            }
        }
    }
    None
}

fn find_dir_starting_with(root: &Path, prefix: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with(prefix))
            {
                return Some(path);
            }
            if let Some(found) = find_dir_starting_with(&path, prefix) {
                return Some(found);
            }
        }
    }
    None
}
