use std::collections::HashMap;
#[cfg(feature = "native-runtime")]
use std::path::PathBuf;
use std::path::Path;

use crate::db::NovelRecord;
use crate::downloader::TocObject;

use super::settings::NovelSettings;

/// 出力ファイル名の決定に必要な、環境由来の設定値。
///
/// native はローカル設定 (`local_setting.yaml`) から同期的に、Worker は
/// D1 (`app_state` スコープ `local`) から読んだマップを渡す。解決規則は
/// [`OutputNamingEnv::from_local_map`] に一本化してあるので、両経路で
/// 同じ名前になる。
#[derive(Debug, Default, Clone, Copy)]
pub struct OutputNamingEnv {
    /// `convert.filename-to-ncode` (未設定は `false`)。
    pub filename_to_ncode: bool,
    /// `ebook-filename-length-limit` (未設定は `None` = 無制限)。
    pub filename_length_limit: Option<usize>,
}

impl OutputNamingEnv {
    /// 保存済みのローカル設定マップから命名に使う値だけを取り出す。
    /// bool/int の強制変換は native `db::settings::bool_value` /
    /// `output_filename_length_limit` と同じ規則 (文字列の "true"/"yes"/
    /// "on"/"1"、数値文字列も受け付ける)。
    pub fn from_local_map(map: &HashMap<String, serde_yaml::Value>) -> Self {
        Self {
            filename_to_ncode: map
                .get("convert.filename-to-ncode")
                .and_then(yaml_bool)
                .unwrap_or(false),
            filename_length_limit: map
                .get("ebook-filename-length-limit")
                .and_then(|value| match value {
                    serde_yaml::Value::Number(number) => number.as_i64(),
                    serde_yaml::Value::String(raw) => raw.parse::<i64>().ok(),
                    _ => None,
                })
                .map(|limit| limit.max(0) as usize),
        }
    }
}

fn yaml_bool(value: &serde_yaml::Value) -> Option<bool> {
    match value {
        serde_yaml::Value::Bool(value) => Some(*value),
        serde_yaml::Value::Number(value) => value.as_i64().map(|value| value != 0),
        serde_yaml::Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}
#[cfg(feature = "native-runtime")]
pub(crate) fn create_output_text_path(
    settings: &NovelSettings,
    id: i64,
    novel_dir: &Path,
    toc: &TocObject,
    record: Option<&NovelRecord>,
) -> PathBuf {
    novel_dir.join(create_output_text_filename(settings, id, toc, record))
}

#[cfg(feature = "native-runtime")]
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

#[cfg(feature = "native-runtime")]
pub(crate) fn create_output_text_filename(
    settings: &NovelSettings,
    _id: i64,
    toc: &TocObject,
    record: Option<&NovelRecord>,
) -> String {
    create_output_filename(settings, toc, record, &local_output_naming_env())
}

/// 保存済み EPUB がその名で返される命名規則: 変換時の txt と同じ basename に
/// 拡張子 `.epub` を付けたもの (native `device.rs` の `{file_stem(txt)}.epub`
/// と同じ)。Worker (`download.epub`) も同じ規則で Content-Disposition 名を
/// 決めるため、fs を読まない pure な実装として共有する。
pub fn epub_output_filename(
    settings: &NovelSettings,
    toc: &TocObject,
    record: Option<&NovelRecord>,
    env: &OutputNamingEnv,
) -> String {
    let txt_name = create_output_filename(settings, toc, record, env);
    let stem = Path::new(&txt_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(txt_name.as_str());
    format!("{stem}.epub")
}

/// `create_output_text_filename` / `epub_output_filename` の共通コア。
/// 変換時の txt のファイル名を返す。環境依存の 2 設定
/// (`convert.filename-to-ncode` と `ebook-filename-length-limit`) だけを
/// `env` として受け取るので、native (Inventory) と Worker (D1) のどちらでも
/// 同じ規則で名前が決まる。
fn create_output_filename(
    settings: &NovelSettings,
    toc: &TocObject,
    record: Option<&NovelRecord>,
    env: &OutputNamingEnv,
) -> String {
    if !settings.output_filename.trim().is_empty() {
        return ensure_extension(
            &sanitize_filename_for_output(&settings.output_filename, env),
            ".txt",
        );
    }

    if env.filename_to_ncode {
        let domain = record
            .and_then(|r| r.domain.clone())
            .or_else(|| extract_domain(&toc.toc_url))
            .unwrap_or_else(|| "unknown".to_string());
        let ncode = record
            .and_then(|r| r.ncode.clone())
            .or_else(|| extract_ncode_like(&toc.toc_url))
            .unwrap_or_else(|| sanitize_filename_for_output(&toc.title, env));
        return format!("{}_{}.txt", domain.replace('.', "_"), ncode);
    }

    let author = if settings.novel_author.is_empty() {
        &toc.author
    } else {
        &settings.novel_author
    };
    let title = settings.title_for_output(&toc.title);
    ensure_extension(&default_output_basename_in(author, &title, env), ".txt")
}

#[cfg(feature = "native-runtime")]
pub(crate) fn local_output_naming_env() -> OutputNamingEnv {
    OutputNamingEnv::from_local_map(
        &crate::db::settings::load(crate::setting_core::SettingScope::Local).unwrap_or_default(),
    )
}

fn sanitize_filename_for_output(name: &str, env: &OutputNamingEnv) -> String {
    crate::db::paths::sanitize_windows_filename_component_with_limit(
        name,
        env.filename_length_limit,
        None,
        "output",
    )
}

#[cfg(feature = "native-runtime")]
pub(crate) fn default_output_basename(author: &str, title: &str) -> String {
    default_output_basename_in(author, title, &local_output_naming_env())
}

fn default_output_basename_in(author: &str, title: &str, env: &OutputNamingEnv) -> String {
    sanitize_filename_for_output(&format!("[{author}] {title}"), env)
}

fn ensure_extension(filename: &str, extension: &str) -> String {
    if filename.to_lowercase().ends_with(extension) {
        filename.to_string()
    } else {
        format!("{filename}{extension}")
    }
}

#[cfg(feature = "native-runtime")]
fn create_output_text_filename_for_textfile(
    settings: &NovelSettings,
    converted_text: &str,
) -> String {
    let env = local_output_naming_env();
    if !settings.output_filename.trim().is_empty() {
        return ensure_extension(
            &sanitize_filename_for_output(&settings.output_filename, &env),
            ".txt",
        );
    }

    let (title, author) = extract_title_and_author_from_text(converted_text);
    if env.filename_to_ncode {
        return ensure_extension(
            &sanitize_filename_for_output(&format!("text_{}", title), &env),
            ".txt",
        );
    }

    ensure_extension(
        &sanitize_filename_for_output(&format!("[{}] {}", author, title), &env),
        ".txt",
    )
}

#[cfg(feature = "native-runtime")]
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
    use super::{
        OutputNamingEnv, create_output_text_filename, create_output_text_path_for_textfile,
        epub_output_filename, sanitize_filename_for_output,
    };
    use crate::converter::settings::NovelSettings;
    use crate::db::NovelRecord;
    use crate::downloader::TocObject;

    fn test_env() -> OutputNamingEnv {
        OutputNamingEnv::default()
    }

    fn test_toc() -> TocObject {
        TocObject {
            title: "タイトル".to_string(),
            author: "作者".to_string(),
            toc_url: "https://ncode.syosetu.com/n9669bk/".to_string(),
            story: None,
            subtitles: Vec::new(),
            novel_type: Some(1),
        }
    }

    /// 既定 (convert.filename 未設定 / filename-to-ncode 無効) は
    /// `[作者] タイトル.epub`。native が保存済み EPUB を返すときの
    /// `<txt stem>.epub` と同じ名。
    #[test]
    fn epub_output_filename_defaults_to_author_title() {
        let toc = test_toc();
        assert_eq!(
            epub_output_filename(&NovelSettings::default(), &toc, None, &test_env()),
            "[作者] タイトル.epub"
        );
    }

    /// convert.filename (output_filename) 指定時はその名を sanitize して
    /// `.epub` を付ける。filename-to-ncode より優先される。
    #[test]
    fn epub_output_filename_uses_output_filename_setting() {
        let settings = NovelSettings {
            output_filename: "カスタム名".to_string(),
            ..NovelSettings::default()
        };
        let toc = test_toc();
        let env = OutputNamingEnv {
            filename_to_ncode: true,
            ..test_env()
        };
        assert_eq!(
            epub_output_filename(&settings, &toc, None, &env),
            "カスタム名.epub"
        );
    }

    /// convert.filename-to-ncode 有効時は `ドメイン_Nコード.epub`。
    /// record の ncode/domain が優先され、無ければ toc_url から拾う。
    #[test]
    fn epub_output_filename_uses_ncode_when_enabled() {
        let toc = test_toc();
        let env = OutputNamingEnv {
            filename_to_ncode: true,
            ..test_env()
        };
        assert_eq!(
            epub_output_filename(&NovelSettings::default(), &toc, None, &env),
            "ncode_syosetu_com_n9669bk.epub"
        );

        let record = NovelRecord {
            id: 1,
            author: "作者".into(),
            title: "タイトル".into(),
            file_title: "title".into(),
            toc_url: toc.toc_url.clone(),
            sitename: "小説家になろう".into(),
            novel_type: 1,
            end: false,
            last_update: chrono::Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: Some("n1234ab".to_string()),
            domain: Some("novel18.syosetu.com".to_string()),
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        };
        assert_eq!(
            epub_output_filename(&NovelSettings::default(), &toc, Some(&record), &env),
            "novel18_syosetu_com_n1234ab.epub"
        );
    }

    #[test]
    fn output_filename_strips_title_prefix_when_enabled() {
        let settings = NovelSettings {
            enable_strip_title_prefix: true,
            ..NovelSettings::default()
        };
        let toc = TocObject {
            title: "【3/17第1巻発売】《コミカライズ企画進行中》悪役令息が破滅フラグ".to_string(),
            author: "作者".to_string(),
            toc_url: "https://example.com/works/1".to_string(),
            story: None,
            subtitles: Vec::new(),
            novel_type: Some(1),
        };
        let raw_title = toc.title.clone();

        assert_eq!(
            create_output_text_filename(&settings, 1, &toc, None),
            "[作者] 悪役令息が破滅フラグ.txt"
        );
        assert_eq!(toc.title, raw_title);
    }
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

        let settings = NovelSettings {
            archive_path: root.clone(),
            ..NovelSettings::default()
        };

        let path = create_output_text_path_for_textfile(&settings, "タイトル\n作者\n本文");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("[作者] タイトル.txt")
        );

        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn sanitize_filename_for_output_handles_reserved_names_and_controls() {
        assert_eq!(
            sanitize_filename_for_output("CON.txt", &test_env()),
            "_CON.txt"
        );
        assert_eq!(
            sanitize_filename_for_output("bad\0name\x1F", &test_env()),
            "badname"
        );
        assert_eq!(sanitize_filename_for_output("trail. ", &test_env()), "trail");
        assert_eq!(
            sanitize_filename_for_output("　全角 title ", &test_env()),
            "　全角 title"
        );
        assert_eq!(sanitize_filename_for_output("", &test_env()), "output");
    }
}
