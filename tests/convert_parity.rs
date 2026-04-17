use std::fs;
use std::path::{Path, PathBuf};

use narou_rs::converter::NovelConverter;
use narou_rs::converter::settings::NovelSettings;
use narou_rs::downloader::{SectionElement, SectionFile, SubtitleInfo, TocFile, TocObject};

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
    let kakuyomu_root = root.join("sample").join("novel").join("小説データ").join("カクヨム");
    let mut checked = 0;

    for entry in fs::read_dir(&kakuyomu_root).expect("kakuyomu sample root") {
        let path = entry.expect("sample entry").path();
        if !path.is_dir() {
            continue;
        }

        let Some(reference) = find_reference_output(&path) else {
            continue;
        };

        let mut converter = NovelConverter::new(NovelSettings::default());
        let output = convert_sample_to_string(&mut converter, &path);
        let reference_bytes = fs::read(&reference).unwrap();
        assert_eq!(
            output.into_bytes(),
            reference_bytes,
            "reference mismatch for {}",
            path.display()
        );
        checked += 1;
    }

    assert!(checked > 0, "reference output fixture");
}

fn convert_sample_to_string(converter: &mut NovelConverter, novel_dir: &Path) -> String {
    let toc: TocFile =
        serde_yaml::from_str(&fs::read_to_string(novel_dir.join("toc.yaml")).expect("toc.yaml"))
            .expect("parse toc.yaml");
    let toc_object = TocObject {
        title: toc.title,
        author: toc.author,
        toc_url: toc.toc_url,
        story: toc.story,
        subtitles: toc.subtitles,
        novel_type: toc.novel_type,
    };
    let sections = toc_object
        .subtitles
        .iter()
        .map(|sub| {
            let path = novel_dir
                .join("本文")
                .join(format!("{} {}.yaml", sub.index, sub.file_subtitle));
            serde_yaml::from_str::<SectionFile>(&fs::read_to_string(path).expect("section yaml"))
                .expect("parse section yaml")
        })
        .collect::<Vec<_>>();

    converter.convert_novel(&toc_object, &sections).unwrap()
}

fn find_reference_output(root: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with("kakuyomu_jp_") && name.ends_with(".txt"))
        {
            return Some(path);
        }
    }
    None
}
