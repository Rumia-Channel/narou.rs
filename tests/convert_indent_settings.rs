//! issue #42: `enable_half_indent_bracket` が端末設定・`default.*` /
//! `force.*` / `setting.ini` のどこから設定しても効くことを、実際の
//! `narou convert` で固定する。
//!
//! 以前は変換時に `device` 設定から `enable_half_indent_bracket` を
//! 上書きしていたため、`device=epub` の環境ではどの設定スコープでも
//! 二分アキが付かなかった。加えて、`setting.ini` の有無を「値が
//! オリジナル設定と同じか」で判定していたため、オリジナル設定と同じ
//! true を明示した場合だけ `default.*` に負けていた。

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_narou_rs"))
        .args(args)
        .current_dir(root)
        .env("USERPROFILE", root.join("home"))
        .env("HOME", root.join("home"))
        .env("TMP", root.join("tmp"))
        .env("TEMP", root.join("tmp"))
        .env("NAROU_RS_LEGACY_YAML", "1")
        .env_remove("NAROU_ENV")
        .env_remove("NAROU_RS_WEB_DEVICE")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn library() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    for relative in [".narou", "home", "tmp"] {
        std::fs::create_dir_all(temp.path().join(relative)).unwrap();
    }
    temp
}

fn assert_success(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

const SAMPLE_TEXT: &str = "「かぎ括弧の行です」\n（丸括弧の行です）\n普通の行です\n";

/// `device=epub` でも `default.enable_half_indent_bracket: true` が効くこと。
/// 変換時に端末由来の既定値で上書きされなくなったことを固定する。
#[test]
fn text_conversion_honors_default_half_indent_with_device_configured() {
    let temp = library();
    let root = temp.path();
    std::fs::write(
        root.join(".narou/local_setting.yaml"),
        "device: epub\ndefault.enable_half_indent_bracket: true\n",
    )
    .unwrap();
    std::fs::write(root.join("sample.txt"), SAMPLE_TEXT).unwrap();

    let output = run(
        root,
        &[
            "convert",
            "sample.txt",
            "--no-epub",
            "--no-open",
            "-o",
            "out.txt",
        ],
    );
    assert_success(&output, "convert sample.txt");
    let text = std::fs::read_to_string(root.join("out.txt")).unwrap();
    assert!(
        text.contains("［＃二分アキ］「かぎ括弧の行です」"),
        "default.enable_half_indent_bracket=true が端末設定に負けている:\n{text}"
    );
    assert!(
        text.contains("［＃二分アキ］（丸括弧の行です）"),
        "全角丸括弧に二分アキが付いていない:\n{text}"
    );
}

const DATABASE_YAML: &str = "1:\n  id: 1\n  title: テスト作品\n  author: テスト作者\n  file_title: \"[テスト作者] テスト作品\"\n  toc_url: https://mock.test/novel/n1234ab/\n  sitename: mock.test\n  novel_type: 1\n  end: false\n  last_update: '2026-01-02T00:00:00Z'\n  general_all_no: 1\n  length: 40\n";
const TOC_YAML: &str = "title: テスト作品\nauthor: テスト作者\ntoc_url: https://mock.test/novel/n1234ab/\nsubtitles:\n- index: '1'\n  href: /novel/n1234ab/1/\n  chapter: ''\n  subchapter: ''\n  subtitle: 第一話\n  file_subtitle: 第一話\n  subdate: '2026-01-01 00:00:00'\nnovel_type: 1\n";
const SECTION_YAML: &str = "index: '1'\nhref: /novel/n1234ab/1/\nchapter: ''\nsubchapter: ''\nsubtitle: 第一話\nfile_subtitle: 第一話\nsubdate: '2026-01-01 00:00:00'\ndownload_time: '2026-01-01 00:00:00 +0900'\nelement:\n  data_type: text\n  introduction: ''\n  postscript: ''\n  body: |\n    「かぎ括弧の行です」\n    普通の行です\n";

fn write_managed_novel(root: &Path) -> std::path::PathBuf {
    std::fs::write(root.join(".narou/database.yaml"), DATABASE_YAML).unwrap();
    let novel = root.join("小説データ/mock.test/[テスト作者] テスト作品");
    std::fs::create_dir_all(novel.join("本文")).unwrap();
    std::fs::write(novel.join("toc.yaml"), TOC_YAML).unwrap();
    std::fs::write(novel.join("本文/1 第一話.yaml"), SECTION_YAML).unwrap();
    novel
}

/// 小説の `setting.ini` にオリジナル設定と同じ値 (`true`) を書いた場合も、
/// `default.enable_half_indent_bracket: false` より優先されること。
#[test]
fn per_novel_setting_ini_overrides_default_scope_for_half_indent() {
    let temp = library();
    let root = temp.path();
    std::fs::write(
        root.join(".narou/local_setting.yaml"),
        "device: epub\ndefault.enable_half_indent_bracket: false\n",
    )
    .unwrap();
    let novel = write_managed_novel(root);
    std::fs::write(
        novel.join("setting.ini"),
        "enable_half_indent_bracket = true\n",
    )
    .unwrap();

    let output = run(
        root,
        &["convert", "1", "--no-epub", "--no-open", "-o", "out.txt"],
    );
    assert_success(&output, "convert 1");
    let text = std::fs::read_to_string(novel.join("out.txt")).unwrap();
    assert!(
        text.contains("［＃二分アキ］「かぎ括弧の行です」"),
        "setting.ini の enable_half_indent_bracket=true が default.* に負けている:\n{text}"
    );

    // setting.ini の項目を外すと default.* = false が使われる。
    std::fs::write(novel.join("setting.ini"), "").unwrap();
    let output = run(
        root,
        &["convert", "1", "--no-epub", "--no-open", "-o", "out.txt"],
    );
    assert_success(&output, "convert 1 (without setting.ini)");
    let text = std::fs::read_to_string(novel.join("out.txt")).unwrap();
    assert!(
        !text.contains("［＃二分アキ］"),
        "default.enable_half_indent_bracket=false が効いていない:\n{text}"
    );
}

/// issue #42: 組み込み EPUB エンジン (GPL 版) でも narou カスタム CSS
/// (`.vrtl .half_em_space`) が EPUB に入り、二分アキが実際に描画されること。
///
/// CSS を同梱しないと `<span class="half_em_space"></span>` は空要素のままで
/// 見た目に何も起きないため、テキストに注記があるだけでは不十分。
#[cfg(feature = "lite")]
#[test]
fn lite_epub_includes_half_indent_css_and_markup() {
    use std::io::Read;

    let temp = library();
    let root = temp.path();
    std::fs::write(
        root.join(".narou/local_setting.yaml"),
        "device: epub\ndefault.enable_half_indent_bracket: true\nconvert.epub-engine: lite\n",
    )
    .unwrap();
    let novel = write_managed_novel(root);

    let output = run(root, &["convert", "1", "--no-open"]);
    assert_success(&output, "convert 1 (epub)");

    let epub_path = std::fs::read_dir(&novel)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "epub"))
        .expect("EPUB output");
    let file = std::fs::File::open(&epub_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut css = String::new();
    let mut xhtml = String::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().to_string();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        if name.ends_with("style/vertical_font.css") {
            css = String::from_utf8(bytes).unwrap();
        } else if name.ends_with(".xhtml") {
            let text = String::from_utf8(bytes).unwrap();
            if text.contains("かぎ括弧") {
                xhtml = text;
            }
        }
    }

    assert!(
        css.contains(".vrtl .half_em_space"),
        "EPUB に二分アキの CSS が同梱されていない:\n{css}"
    );
    assert!(
        xhtml.contains("<span class=\"half_em_space\"></span>「かぎ括弧の行です」"),
        "EPUB に二分アキの span が入っていない:\n{xhtml}"
    );
}
