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

/// issue #42 (追記): グローバル設定に `aozoraepub3dir` があり、ローカル設定で
/// `convert.epub-engine: lite` を指定した場合は、ローカルの lite を優先する。
///
/// 以前は `convert.epub-engine` をグローバルからしか読んでいなかったため、
/// 外部 AozoraEpub3 の場所が設定されていると lite 指定が無視されていた。
#[cfg(feature = "lite")]
#[test]
fn local_lite_engine_wins_over_global_aozora_install() {
    let temp = library();
    let root = temp.path();
    std::fs::create_dir_all(root.join("home/.narousetting")).unwrap();
    let aozora = root.join("AozoraEpub3");
    std::fs::create_dir_all(&aozora).unwrap();
    std::fs::write(aozora.join("AozoraEpub3.jar"), b"not a jar").unwrap();
    std::fs::write(
        root.join("home/.narousetting/global_setting.yaml"),
        format!(
            "aozoraepub3dir: {}\nconvert.epub-engine: external\n",
            aozora.display()
        ),
    )
    .unwrap();
    std::fs::write(
        root.join(".narou/local_setting.yaml"),
        "device: epub\nconvert.epub-engine: lite\n",
    )
    .unwrap();
    let novel = write_managed_novel(root);

    let output = run(root, &["convert", "1", "--no-open"]);
    assert_success(&output, "convert 1 (local lite)");
    assert!(
        std::fs::read_dir(&novel)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "epub")),
        "ローカルの convert.epub-engine: lite が外部設定に負けている"
    );
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
        let name = entry.name().unwrap().to_string();
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

#[cfg(feature = "lite")]
fn epub_entry_names(novel: &Path) -> Vec<String> {
    use std::io::Read;
    let epub = std::fs::read_dir(novel)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "epub"))
        .expect("EPUB output");
    let mut archive = zip::ZipArchive::new(std::fs::File::open(epub).unwrap()).unwrap();
    let mut names = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        names.push(entry.name().unwrap().to_string());
    }
    names.sort();
    names
}

/// issue #45: 利用者が作る `preset/AozoraEpub3.ini` を置くと、組み込み
/// エンジンの実際の EPUB 出力が変わること。`TitlePage=0` は扉ページを
/// 生成しないため、ZIP のエントリ構成で確認できる。
#[cfg(feature = "lite")]
#[test]
fn lite_preset_ini_is_read_by_the_real_binary() {
    let temp = library();
    let root = temp.path();
    std::fs::write(
        root.join(".narou/local_setting.yaml"),
        "device: epub\nconvert.epub-engine: lite\n",
    )
    .unwrap();
    let novel = write_managed_novel(root);

    let first = run(root, &["convert", "1", "--no-open"]);
    assert_success(&first, "convert 1 (no preset)");
    let without_preset = epub_entry_names(&novel);

    std::fs::create_dir_all(root.join("preset")).unwrap();
    std::fs::write(
        root.join("preset/AozoraEpub3.ini"),
        "TitlePage=0\n",
    )
    .unwrap();
    let second = run(root, &["convert", "1", "--no-open"]);
    assert_success(&second, "convert 1 (preset TitlePage=0)");
    let with_preset = epub_entry_names(&novel);

    assert_ne!(
        without_preset, with_preset,
        "preset/AozoraEpub3.ini が組み込みエンジンに反映されていない"
    );
}

/// issue #45: `preset/` の設定は「利用者の設定 (`AozoraEpub3.ini`) → 同梱の
/// 既定値 (`AozoraEpub3.ini.default`)」の順で解決される。利用者の設定は
/// 配布物に含まれないため、セルフアップデートで内容が消えない。
#[cfg(feature = "lite")]
#[test]
fn lite_preset_user_ini_wins_over_the_bundled_default_ini() {
    let temp = library();
    let root = temp.path();
    std::fs::write(
        root.join(".narou/local_setting.yaml"),
        "device: epub\nconvert.epub-engine: lite\n",
    )
    .unwrap();
    let novel = write_managed_novel(root);

    std::fs::create_dir_all(root.join("preset")).unwrap();
    // `TitlePageWrite` も指定する (INI の既定値だけでは扉ページが生成されない)。
    std::fs::write(
        root.join("preset/AozoraEpub3.ini.default"),
        "TitlePage=0\nTitlePageWrite=1\n",
    )
    .unwrap();
    let default_run = run(root, &["convert", "1", "--no-open"]);
    assert_success(&default_run, "convert 1 (bundled default ini)");
    let with_default = epub_entry_names(&novel);

    std::fs::write(
        root.join("preset/AozoraEpub3.ini"),
        "TitlePage=2\nTitlePageWrite=1\n",
    )
    .unwrap();
    let user_run = run(root, &["convert", "1", "--no-open"]);
    assert_success(&user_run, "convert 1 (user ini)");
    let with_user = epub_entry_names(&novel);

    assert_ne!(
        with_default, with_user,
        "preset/AozoraEpub3.ini が同梱の既定値より優先されていない"
    );
}
