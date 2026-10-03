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
fn synthetic_html_entities_match_literal_spaces_through_conversion() {
    // Original synthetic prose only; no untracked novel or network dependency.
    let toc = TocObject {
        title: "変換テスト".to_string(),
        author: "テスト作者".to_string(),
        toc_url: String::new(),
        story: Some("<p>甲&nbsp;乙</p>".to_string()),
        subtitles: Vec::new(),
        novel_type: Some(1),
    };
    let section: SectionFile = serde_yaml::from_str(
        r#"
index: '1'
href: '/1/'
subtitle: 第一話
file_subtitle: 第一話
subdate: ''
element:
  data_type: html
  introduction: '<p>前&nbsp;書き</p>'
  body: '<p>&nbsp;本文&nbsp;甲。</p><p>乙&nbsp;丙。</p>'
  postscript: '<p>後&nbsp;書き</p>'
"#,
    )
    .unwrap();
    let mut literal_toc = toc.clone();
    literal_toc.story = toc.story.as_ref().map(|story| story.replace("&nbsp;", " "));
    let mut literal_section = section.clone();
    literal_section.element.introduction = section.element.introduction.replace("&nbsp;", " ");
    literal_section.element.body = section.element.body.replace("&nbsp;", " ");
    literal_section.element.postscript = section.element.postscript.replace("&nbsp;", " ");

    let encoded = NovelConverter::new(NovelSettings::default())
        .convert_novel(&toc, &[section])
        .unwrap();
    let literal = NovelConverter::new(NovelSettings::default())
        .convert_novel(&literal_toc, &[literal_section])
        .unwrap();
    assert_eq!(encoded.as_bytes(), literal.as_bytes());
    assert!(!encoded.contains('\u{00A0}'));
    assert!(encoded.contains("本文"));
}

#[test]
#[ignore = "requires local, untracked narou.rb reference novels; see tests/README.md"]
fn kakuyomu_sample_matches_narou_rb_reference_byte_for_byte() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let kakuyomu_root = root.join("sample").join("novel").join("小説データ").join("カクヨム");
    assert!(
        kakuyomu_root.is_dir(),
        "reference fixture directory is missing: {}",
        kakuyomu_root.display()
    );
    let mut checked = 0;

    for entry in fs::read_dir(&kakuyomu_root).expect("kakuyomu sample root") {
        let path = entry.expect("sample entry").path();
        if !path.is_dir() {
            continue;
        }

        let Some(reference) = find_reference_output(&path) else {
            continue;
        };

        // Reference fixtures were generated with the sample EPUB device defaults.
        let mut settings = NovelSettings::default();
        settings.enable_half_indent_bracket = false;
        let mut converter = NovelConverter::new(settings);
        let output = convert_sample_to_string(&mut converter, &path);
        let reference_bytes = fs::read(&reference).unwrap();
        let output_bytes = output.into_bytes();
        assert_eq!(
            output_bytes,
            reference_bytes,
            "{}",
            first_mismatch_report(&path, &output_bytes, &reference_bytes)
        );
        checked += 1;
    }

    assert!(checked > 0, "reference output fixture");
}

fn first_mismatch_report(path: &Path, output: &[u8], reference: &[u8]) -> String {
    let output_text = String::from_utf8_lossy(output);
    let reference_text = String::from_utf8_lossy(reference);
    let output_lines = output_text.lines().collect::<Vec<_>>();
    let reference_lines = reference_text.lines().collect::<Vec<_>>();
    let max_lines = output_lines.len().max(reference_lines.len());
    for idx in 0..max_lines {
        let out = output_lines.get(idx).copied().unwrap_or("<missing>");
        let expected = reference_lines.get(idx).copied().unwrap_or("<missing>");
        if out != expected {
            return format!(
                "reference mismatch for {}\nline {}\noutput:   {:?}\nexpected: {:?}",
                path.display(),
                idx + 1,
                out,
                expected
            );
        }
    }
    format!(
        "reference mismatch for {}\nbytes differ only outside line text: output={} reference={}",
        path.display(),
        output.len(),
        reference.len()
    )
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
