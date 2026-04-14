use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use encoding_rs::{Encoding, UTF_8};
use narou_rs::converter::NovelConverter;
use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::db::inventory::{Inventory, InventoryScope};
use narou_rs::progress::CliProgress;
use regex::Regex;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::resolve_target_to_id;

pub fn cmd_convert(
    targets: &[String],
    output: Option<&str>,
    encoding: Option<&str>,
    no_epub: bool,
    no_mobi: bool,
    inspect: bool,
    no_open: bool,
    verbose: bool,
    ignore_default: bool,
    ignore_force: bool,
) {
    if let Err(e) = narou_rs::db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    let multi = CliProgress::multi();
    let multi_clone = multi.clone();
    let mut first_output_dir = None;
    let output_parts = output.map(split_output_name);
    let selected_device = narou_rs::compat::current_device();
    let output_device = effective_convert_device(selected_device, no_epub, no_mobi);
    let encoding = match normalize_text_file_encoding_name(encoding) {
        Ok(encoding) => encoding,
        Err(message) => {
            let _ = multi_clone.println(message);
            return;
        }
    };

    if let Some(device) = selected_device {
        let _ = multi_clone.println(&format!(">> {}用に変換します", device.display_name()));
    }

    for (index, target) in targets.iter().enumerate() {
        let output_filename = output_parts.as_ref().map(|(basename, ext)| {
            build_output_filename(
                basename,
                ext,
                if targets.len() > 1 {
                    Some(index + 1)
                } else {
                    None
                },
            )
        });

        let target_path = Path::new(target);
        if target_path.is_file() {
            convert_text_target(
                target,
                target_path,
                output_filename.as_deref(),
                encoding.as_deref(),
                inspect,
                ignore_default,
                ignore_force,
                output_device,
                selected_device,
                verbose,
                &multi_clone,
                &mut first_output_dir,
            );
            continue;
        }

        let Some(id) = resolve_target_to_id(target) else {
            let _ = multi_clone.println(&format!("{} は存在しません", target));
            continue;
        };

        let dc_subjects = match load_dc_subjects_for_novel(id) {
            Ok(subjects) => subjects,
            Err(err) => {
                let _ = multi_clone.println(&err);
                None
            }
        };

        let (novel_dir, title, author) = match narou_rs::db::with_database(|db| {
            let record = db
                .get(id)
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
            let dir = narou_rs::db::existing_novel_dir_for_record(db.archive_root(), record);
            Ok::<(std::path::PathBuf, String, String), narou_rs::error::NarouError>((
                dir,
                record.title.clone(),
                record.author.clone(),
            ))
        }) {
            Ok(data) => data,
            Err(e) => {
                let _ = multi_clone.println(&format!("  Error: {}", e));
                continue;
            }
        };

        let progress = CliProgress::with_multi(&format!("Convert {}", title), multi_clone.clone());

        let mut settings = NovelSettings::load_for_novel_with_options(
            id,
            &title,
            &author,
            &novel_dir,
            ignore_force,
            ignore_default,
        );
        if let Some(output_filename) = &output_filename {
            settings.output_filename = output_filename.clone();
        }
        let mut converter =
            if let Some(user_converter) = UserConverter::load_with_title(&novel_dir, &title) {
                NovelConverter::with_user_converter(settings, user_converter)
            } else {
                NovelConverter::new(settings)
            };
        converter.set_progress(Box::new(progress));
        converter.set_display_inspector(inspect);

        let result = match output_device {
            Some(device) => converter
                .convert_novel_by_id_with_device(id, &novel_dir, device, verbose)
                .map(|path| path.display().to_string()),
            None => converter.convert_novel_by_id(id, &novel_dir),
        };

        match result {
            Ok(output_path) => {
                let _ = multi_clone.println(&format!("  Output: {}", output_path));
                if first_output_dir.is_none() {
                    first_output_dir = std::path::Path::new(&output_path)
                        .parent()
                        .map(|path| path.to_path_buf());
                }
                if let Err(err) =
                    print_copy_to_result(&output_path, selected_device, id, &multi_clone)
                {
                    let _ = multi_clone.println(&err);
                }
                if let Some(device) = selected_device {
                    if let Err(err) =
                        print_send_result(&output_path, device, &multi_clone)
                    {
                        let _ = multi_clone.println(&err);
                    }
                }
                apply_dc_subjects_if_needed(
                    Path::new(&output_path),
                    dc_subjects.as_deref(),
                    &multi_clone,
                );
                if let Some(inspection) = converter.take_inspection_output() {
                    for line in inspection.split('\n') {
                        let _ = multi_clone.println(line);
                    }
                }
            }
            Err(e) => {
                let _ = multi_clone.println(&format!("  Error: {}", e));
            }
        }
    }

    drop(multi);

    if !no_open && !narou_rs::compat::load_local_setting_bool("convert.no-open") {
        if let Some(dir) = first_output_dir {
            narou_rs::compat::open_directory(&dir, Some("小説の保存フォルダを開きますか"));
        }
    }
}

fn convert_text_target(
    target: &str,
    target_path: &Path,
    output_filename: Option<&str>,
    encoding: Option<&str>,
    inspect: bool,
    ignore_default: bool,
    ignore_force: bool,
    output_device: Option<narou_rs::converter::device::Device>,
    copy_device: Option<narou_rs::converter::device::Device>,
    verbose: bool,
    multi_clone: &indicatif::MultiProgress,
    first_output_dir: &mut Option<PathBuf>,
) {
    let text = match read_text_file(target_path, encoding) {
        Ok(text) => text,
        Err(message) => {
            for line in message.lines() {
                let _ = multi_clone.println(line);
            }
            return;
        }
    };

    let archive_path = text_output_archive_path(target_path, output_filename);
    let source_name = target_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(target)
        .to_string();
    let mut settings = NovelSettings::create_for_text_file_with_options(
        &archive_path,
        &source_name,
        ignore_force,
        ignore_default,
    );
    if let Some(output_filename) = output_filename {
        settings.output_filename = output_filename.to_string();
    }

    let mut converter =
        if let Some(user_converter) = UserConverter::load_with_title(&archive_path, &source_name) {
            NovelConverter::with_user_converter(settings, user_converter)
        } else {
            NovelConverter::new(settings)
        };
    converter.set_display_inspector(inspect);

    let result = match output_device {
        Some(device) => converter.convert_text_file_with_device(&text, device, verbose),
        None => converter.convert_text_file(&text),
    };

    match result {
        Ok(output_path) => {
            let _ = multi_clone.println(&format!("  Output: {}", output_path));
            if first_output_dir.is_none() {
                *first_output_dir = Path::new(&output_path)
                    .parent()
                    .map(|path| path.to_path_buf());
            }
            if let Err(err) = print_copy_to_result(&output_path, copy_device, 0, multi_clone) {
                let _ = multi_clone.println(&err);
            }
            if let Some(device) = copy_device {
                if let Err(err) = print_send_result(&output_path, device, multi_clone) {
                    let _ = multi_clone.println(&err);
                }
            }
            print_inspection_output(&mut converter, multi_clone);
        }
        Err(e) => {
            let _ = multi_clone.println(&format!("  Error: {}", e));
        }
    }
}

fn effective_convert_device(
    selected_device: Option<narou_rs::converter::device::Device>,
    no_epub: bool,
    no_mobi: bool,
) -> Option<narou_rs::converter::device::Device> {
    if no_epub {
        return None;
    }
    match selected_device {
        Some(narou_rs::converter::device::Device::Mobi) if no_mobi => {
            Some(narou_rs::converter::device::Device::Epub)
        }
        other => other,
    }
}

fn load_dc_subjects_for_novel(novel_id: i64) -> std::result::Result<Option<Vec<String>>, String> {
    if novel_id <= 0 || !narou_rs::compat::load_local_setting_bool("convert.add-dc-subject-to-epub")
    {
        return Ok(None);
    }

    let record = narou_rs::db::with_database(|db| Ok(db.get(novel_id).cloned()))
        .map_err(|e| e.to_string())?;
    let Some(record) = record else {
        return Ok(None);
    };
    if record.tags.is_empty() {
        return Ok(None);
    }

    let excluded_tags = load_dc_subject_exclude_tags()?;
    let subjects: Vec<String> = record
        .tags
        .iter()
        .map(|tag| tag.trim())
        .filter(|tag| !tag.is_empty() && !excluded_tags.iter().any(|excluded| excluded == tag))
        .map(ToString::to_string)
        .collect();
    if subjects.is_empty() {
        return Ok(None);
    }
    Ok(Some(subjects))
}

fn load_dc_subject_exclude_tags() -> std::result::Result<Vec<String>, String> {
    if let Some(raw) = narou_rs::compat::load_local_setting_string("convert.dc-subject-exclude-tags")
    {
        return Ok(raw
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect());
    }

    let default_value = "404,end".to_string();
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let mut settings: HashMap<String, serde_yaml::Value> = inventory
        .load("local_setting", InventoryScope::Local)
        .map_err(|e| e.to_string())?;
    settings.insert(
        "convert.dc-subject-exclude-tags".to_string(),
        serde_yaml::Value::String(default_value.clone()),
    );
    inventory
        .save("local_setting", InventoryScope::Local, &settings)
        .map_err(|e| e.to_string())?;

    Ok(default_value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn apply_dc_subjects_if_needed(
    output_path: &Path,
    subjects: Option<&[String]>,
    multi_clone: &indicatif::MultiProgress,
) {
    let Some(subjects) = subjects else {
        return;
    };
    if subjects.is_empty() || !is_epub_output(output_path) {
        return;
    }

    match add_dc_subject_to_epub(output_path, subjects) {
        Ok(()) => {
            let _ = multi_clone.println(&format!("dc:subjectを追加しました: {}", subjects.join(", ")));
        }
        Err(err) => {
            let _ = multi_clone.println(&format!("dc:subject追加中にエラーが発生しました: {}", err));
            let _ = multi_clone.println("dc:subject埋め込み処理に失敗しましたが、変換を続行します");
        }
    }
}

fn is_epub_output(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase().ends_with(".epub"))
        .unwrap_or(false)
}

fn add_dc_subject_to_epub(
    epub_path: &Path,
    subjects: &[String],
) -> std::result::Result<(), String> {
    if subjects.is_empty() {
        return Ok(());
    }

    let mut archive =
        ZipArchive::new(File::open(epub_path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
        let mut body = Vec::new();
        entry.read_to_end(&mut body).map_err(|e| e.to_string())?;
        entries.push((entry.name().to_string(), body));
    }

    let Some(opf_index) = entries
        .iter()
        .position(|(name, _)| name.ends_with("standard.opf"))
    else {
        return Err("standard.opfファイルが見つかりませんでした".to_string());
    };

    let mut content = String::from_utf8(entries[opf_index].1.clone()).map_err(|e| e.to_string())?;
    let subject_re = Regex::new(r"(?ms)<dc:subject>.*?</dc:subject>\s*\n?\s*")
        .map_err(|e| e.to_string())?;
    content = subject_re.replace_all(&content, "").into_owned();

    let subject_lines: Vec<String> = subjects
        .iter()
        .map(|subject| subject.trim())
        .filter(|subject| !subject.is_empty())
        .map(|subject| format!("\t\t<dc:subject>{}</dc:subject>", escape_xml(subject)))
        .collect();
    if !subject_lines.is_empty() {
        let metadata_re = Regex::new(r"(?s)(\s*)</metadata>").map_err(|e| e.to_string())?;
        if !metadata_re.is_match(&content) {
            return Err("</metadata> が見つかりませんでした".to_string());
        }
        content = metadata_re
            .replace(&content, format!("\n{}\n$1</metadata>", subject_lines.join("\n")))
            .into_owned();
    }
    entries[opf_index].1 = content.into_bytes();

    let Some(mimetype_index) = entries.iter().position(|(name, _)| name == "mimetype") else {
        return Err("mimetypeファイルが見つかりません".to_string());
    };

    let temp_path = epub_path.with_file_name(format!(
        "{}.tmp",
        epub_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("output.epub")
    ));
    {
        let file = File::create(&temp_path).map_err(|e| e.to_string())?;
        let mut writer = ZipWriter::new(file);
        let stored =
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer
            .start_file("mimetype", stored)
            .map_err(|e| e.to_string())?;
        writer
            .write_all(&entries[mimetype_index].1)
            .map_err(|e| e.to_string())?;

        let deflated = SimpleFileOptions::default();
        for (name, body) in &entries {
            if name == "mimetype" {
                continue;
            }
            writer
                .start_file(name, deflated)
                .map_err(|e| e.to_string())?;
            writer.write_all(body).map_err(|e| e.to_string())?;
        }
        writer.finish().map_err(|e| e.to_string())?;
    }

    std::fs::remove_file(epub_path).map_err(|e| e.to_string())?;
    std::fs::rename(&temp_path, epub_path).map_err(|e| e.to_string())?;
    Ok(())
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn text_output_archive_path(target_path: &Path, output_filename: Option<&str>) -> PathBuf {
    if output_filename.is_some() {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    }

    target_path
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn print_inspection_output(converter: &mut NovelConverter, multi_clone: &indicatif::MultiProgress) {
    if let Some(inspection) = converter.take_inspection_output() {
        for line in inspection.split('\n') {
            let _ = multi_clone.println(line);
        }
    }
}

fn print_copy_to_result(
    output_path: &str,
    device: Option<narou_rs::converter::device::Device>,
    novel_id: i64,
    multi_clone: &indicatif::MultiProgress,
) -> std::result::Result<(), String> {
    if let Some(path) =
        narou_rs::compat::copy_to_converted_file(Path::new(output_path), device, novel_id)?
    {
        let _ = multi_clone.println(&format!("{} へコピーしました", path.display()));
    }
    Ok(())
}

fn print_send_result(
    output_path: &str,
    device: narou_rs::converter::device::Device,
    _multi_clone: &indicatif::MultiProgress,
) -> std::result::Result<(), String> {
    narou_rs::compat::send_file_to_device(Path::new(output_path), device)
}

fn split_output_name(output: &str) -> (String, String) {
    let path = Path::new(output);
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_else(|| ".txt".to_string());
    let basename = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("output")
        .to_string();
    (basename, ext)
}

fn build_output_filename(basename: &str, ext: &str, index: Option<usize>) -> String {
    match index {
        Some(index) => format!("{basename} ({index}){ext}"),
        None => format!("{basename}{ext}"),
    }
}

fn normalize_text_file_encoding_name(
    encoding: Option<&str>,
) -> std::result::Result<Option<String>, &'static str> {
    let Some(encoding) = encoding else {
        return Ok(None);
    };
    let normalized = encoding.trim();
    if normalized.is_empty() {
        return Ok(None);
    }
    if resolve_text_file_encoding(normalized).is_none() {
        return Err(
            "--enc で指定された文字コードは存在しません。sjis, eucjp, utf-8 等を指定して下さい",
        );
    }
    if normalized.eq_ignore_ascii_case("utf8") {
        Ok(Some("utf-8".to_string()))
    } else {
        Ok(Some(normalized.to_string()))
    }
}

fn resolve_text_file_encoding(label: &str) -> Option<&'static Encoding> {
    let normalized = label.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "utf8" | "utf-8" => Some(UTF_8),
        "sjis" | "shift_jis" | "shift-jis" | "cp932" | "windows-31j" => {
            Encoding::for_label(b"shift_jis")
        }
        "eucjp" | "euc-jp" => Encoding::for_label(b"euc-jp"),
        _ => Encoding::for_label(normalized.as_bytes()),
    }
}

fn read_text_file(path: &Path, encoding: Option<&str>) -> std::result::Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("  Error: {}", e))?;
    let decoded = match encoding {
        Some(label) => decode_text_file_with_encoding(&bytes, label, path)?,
        None => decode_text_file_as_utf8(&bytes)?,
    };
    Ok(decoded.replace('\r', ""))
}

fn decode_text_file_as_utf8(bytes: &[u8]) -> std::result::Result<String, String> {
    let bytes = strip_utf8_bom(bytes);
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        "テキストファイルの文字コードがUTF-8ではありません。--enc オプションでテキストの文字コードを指定して下さい".to_string()
    })
}

fn decode_text_file_with_encoding(
    bytes: &[u8],
    label: &str,
    path: &Path,
) -> std::result::Result<String, String> {
    let encoding = resolve_text_file_encoding(label).ok_or_else(|| {
        "--enc で指定された文字コードは存在しません。sjis, eucjp, utf-8 等を指定して下さい"
            .to_string()
    })?;

    if encoding == UTF_8 {
        return String::from_utf8(strip_utf8_bom(bytes).to_vec()).map_err(|_| {
            format!(
                "{}:\nテキストファイルの文字コードは{}ではありませんでした。\n正しい文字コードを指定して下さい",
                path.display(),
                label
            )
        });
    }

    let (decoded, _, had_errors) = encoding.decode(bytes);
    if had_errors {
        return Err(format!(
            "{}:\nテキストファイルの文字コードは{}ではありませんでした。\n正しい文字コードを指定して下さい",
            path.display(),
            label
        ));
    }
    Ok(decoded.into_owned())
}

fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        build_output_filename, decode_text_file_as_utf8, decode_text_file_with_encoding,
        normalize_text_file_encoding_name, split_output_name,
    };

    #[test]
    fn split_output_name_ignores_directory_part() {
        assert_eq!(
            split_output_name(r"C:\tmp\custom-name.epub"),
            ("custom-name".to_string(), ".epub".to_string())
        );
    }

    #[test]
    fn build_output_filename_adds_index_only_for_multiple_targets() {
        assert_eq!(build_output_filename("custom", ".txt", None), "custom.txt");
        assert_eq!(
            build_output_filename("custom", ".txt", Some(2)),
            "custom (2).txt"
        );
    }

    #[test]
    fn normalize_text_file_encoding_accepts_ruby_style_aliases() {
        assert_eq!(
            normalize_text_file_encoding_name(Some("sjis")).unwrap(),
            Some("sjis".to_string())
        );
        assert_eq!(
            normalize_text_file_encoding_name(Some("UTF8")).unwrap(),
            Some("utf-8".to_string())
        );
        assert!(normalize_text_file_encoding_name(Some("no-such-encoding")).is_err());
    }

    #[test]
    fn decode_text_file_as_utf8_rejects_non_utf8_without_enc_option() {
        let bytes = [0x82, 0xA0];
        assert!(decode_text_file_as_utf8(&bytes).is_err());
    }

    #[test]
    fn decode_text_file_with_encoding_reads_shift_jis_text() {
        let (encoded, _, _) = encoding_rs::SHIFT_JIS.encode("タイトル\r\n作者\r\n本文");
        let path = std::path::Path::new(r"C:\tmp\sample.txt");
        let decoded = decode_text_file_with_encoding(encoded.as_ref(), "sjis", path).unwrap();
        assert_eq!(decoded, "タイトル\r\n作者\r\n本文");
    }
}
