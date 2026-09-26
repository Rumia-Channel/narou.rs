use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use encoding_rs::{Encoding, UTF_8};
use narou_rs::application::messages::{self, MessageSink, Stream};
use narou_rs::converter::NovelConverter;
use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::db::inventory::Inventory;
use narou_rs::progress::{CliProgress, WebProgress, is_web_mode};
use regex::Regex;
use zip::write::SimpleFileOptions;
use narou_rs::termcolor::bold_colored;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::resolve_target_to_id;

pub fn cmd_convert(
    targets: &[String],
    output: Option<&str>,
    encoding: Option<&str>,
    no_epub: bool,
    no_mobi: bool,
    no_strip: bool,
    no_zip: bool,
    make_zip: bool,
    inspect: bool,
    no_open: bool,
    verbose: bool,
    ignore_default: bool,
    ignore_force: bool,
    sink: &dyn MessageSink,
) {
    if let Err(e) = narou_rs::db::init_database() {
        sink.emit(Stream::Stderr, &messages::init_db_error(e));
        std::process::exit(1);
    }

    let multi = CliProgress::multi();
    let multi_clone = multi.clone();
    let mut first_output_dir = None;
    let output_parts = output.map(split_output_name);
    let _ = no_strip;
    let selected_devices = match resolve_selected_devices(make_zip, sink) {
        Ok(devices) => devices,
        Err(()) => return,
    };
    let encoding = match normalize_text_file_encoding_name(encoding) {
        Ok(encoding) => encoding,
        Err(message) => {
            sink.emit(Stream::Stdout, message);
            return;
        }
    };

    for selected_device in selected_devices {
        let output_device = effective_convert_device(selected_device, no_epub, no_mobi, no_zip);
        let copy_device = effective_copy_device(selected_device, output_device);
        let create_ibunko_side_epub =
            matches!(selected_device, Some(narou_rs::converter::device::Device::Ibunko))
                && !no_epub
                && matches!(output_device, Some(narou_rs::converter::device::Device::Ibunko));

        if let Some(device) = selected_device {
            sink.emit(
                Stream::Stdout,
                &bold_colored(&messages::convert::converting_for(device.display_name()), "magenta"),
            );
        }

        let total_count = targets.len();
        let mut completed_count = 0usize;
        sink.emit(Stream::Stdout, &messages::convert::convert_started(total_count));

        for (index, target) in targets.iter().enumerate() {
            if index > 0 {
                sink.emit(Stream::Stdout, &messages::separator());
            }
            sink.emit(
                Stream::Stdout,
                &messages::convert::processing(index + 1, total_count, target),
            );
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
                    selected_device,
                    output_device,
                    copy_device,
                    create_ibunko_side_epub,
                    no_strip,
                    verbose,
                    &mut first_output_dir,
                    sink,
                );
                continue;
            }

            let Some(id) = resolve_target_to_id(target) else {
                sink.emit(Stream::Stdout, &messages::target_missing(target));
                continue;
            };

            let dc_subjects = match load_dc_subjects_for_novel(id) {
                Ok(subjects) => subjects,
                Err(err) => {
                    sink.emit(Stream::Stdout, &err);
                    None
                }
            };

            if let Err(err) = narou_rs::title::sync_title_projection(id) {
                sink.emit(Stream::Stdout, &messages::indented_error(err));
                continue;
            }

            let novels = narou_rs::native::novel_repository::NativeNovelRepository::new();
            let record = match novels.get_sync(id.into()) {
                Ok(Some(record)) => record,
                _ => {
                    sink.emit(Stream::Stdout, &messages::convert::id_missing(id));
                    continue;
                }
            };
            let archive_root = narou_rs::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
                .unwrap_or_else(|_| std::path::PathBuf::from(narou_rs::downloader::ARCHIVE_ROOT_DIR));
            let (novel_dir, title, author) = (
                narou_rs::db::existing_novel_dir_for_record(&archive_root, &record),
                record.title.clone(),
                record.author.clone(),
            );

            let progress: Box<dyn narou_rs::progress::ProgressReporter> = if is_web_mode() {
                Box::new(WebProgress::new("convert"))
            } else {
                Box::new(CliProgress::with_multi(&format!("Convert {}", title), multi_clone.clone()))
            };

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
            apply_device_related_settings(&mut settings, selected_device);
            let include_illust = settings.enable_illust;
            let mut converter =
                if let Some(user_converter) = UserConverter::load_with_title(&novel_dir, &title) {
                    NovelConverter::with_user_converter(settings, user_converter)
                } else {
                    NovelConverter::new(settings)
            };
            converter.set_progress(progress);
            converter.set_display_inspector(inspect);

            let _lock = match narou_rs::compat::NovelLockGuard::acquire(Some(id)) {
                Ok(lock) => lock,
                Err(e) => {
                    sink.emit(Stream::Stdout, &messages::indented_error(e));
                    continue;
                }
            };
            let result = match output_device {
                Some(device) => converter
                    .convert_novel_by_id_with_device(id, &novel_dir, device, no_strip, verbose)
                    .map(|path| path.display().to_string()),
                None => converter.convert_novel_by_id(id, &novel_dir),
            };

            match result {
                Ok(output_path) => {
                    if create_ibunko_side_epub {
                        match create_ibunko_epub_output(
                            Path::new(&output_path),
                            narou_rs::converter::device::Device::Epub,
                            include_illust,
                            verbose,
                        ) {
                            Ok(Some(epub_path)) => {
                                apply_dc_subjects_if_needed(
                                    &epub_path,
                                    dc_subjects.as_deref(),
                                    sink,
                                );
                                if let Some(fname) = epub_path.file_name().and_then(|n| n.to_str()) {
                                    sink.emit(Stream::Stdout, &messages::convert::output_written(fname));
                                }
                                sink.emit(
                                    Stream::Stdout,
                                    &bold_colored(messages::convert::epub_written(), "green"),
                                );
                            }
                            Ok(None) => {}
                            Err(err) => {
                                sink.emit(Stream::Stdout, &err);
                            }
                        }
                    }
                    let output_lower = output_path.to_ascii_lowercase();
                    if let Some(fname) = Path::new(&output_path).file_name().and_then(|n| n.to_str()) {
                        sink.emit(Stream::Stdout, &messages::convert::output_written(fname));
                    }
                    if output_lower.ends_with(".mobi") || output_lower.ends_with(".azw3") {
                        sink.emit(
                            Stream::Stdout,
                            &bold_colored(messages::convert::mobi_written(), "green"),
                        );
                    } else if output_lower.ends_with(".epub") || output_lower.ends_with(".kepub.epub") {
                        sink.emit(
                            Stream::Stdout,
                            &bold_colored(messages::convert::epub_written(), "green"),
                        );
                    }
                    if first_output_dir.is_none() {
                        first_output_dir = std::path::Path::new(&output_path)
                            .parent()
                            .map(|path| path.to_path_buf());
                    }
                    if let Err(err) =
                        print_copy_to_result(&output_path, copy_device, id, sink)
                    {
                        sink.emit(Stream::Stdout, &err);
                    }
                    if output_path.to_ascii_lowercase().ends_with(".zip")
                        && let Err(err) = print_copy_zip_to_result(&output_path, sink) {
                            sink.emit(Stream::Stdout, &err);
                        }
                    if let Some(device) = copy_device
                        && let Err(err) =
                            print_send_result(&output_path, device)
                        {
                            sink.emit(Stream::Stdout, &err);
                        }
                    apply_dc_subjects_if_needed(
                        Path::new(&output_path),
                        dc_subjects.as_deref(),
                        sink,
                    );
                    print_inspection_output(&mut converter, sink);
                    completed_count += 1;
                    sink.emit(
                        Stream::Stdout,
                        &messages::convert::completed(index + 1, total_count, target),
                    );
                }
                Err(e) => {
                    sink.emit(
                        Stream::Stdout,
                        &messages::convert::item_error(index + 1, total_count, target, e),
                    );
                }
            }
        }

        sink.emit(
            Stream::Stdout,
            &messages::convert::convert_finished(completed_count, total_count),
        );
    }

    drop(multi);

    if !no_open && !narou_rs::compat::load_local_setting_bool("convert.no-open")
        && let Some(dir) = first_output_dir {
            narou_rs::compat::open_directory(&dir, Some("小説の保存フォルダを開きますか"));
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
    selected_device: Option<narou_rs::converter::device::Device>,
    output_device: Option<narou_rs::converter::device::Device>,
    copy_device: Option<narou_rs::converter::device::Device>,
    create_ibunko_side_epub: bool,
    no_strip: bool,
    verbose: bool,
    first_output_dir: &mut Option<PathBuf>,
    sink: &dyn MessageSink,
) {
    let text = match read_text_file(target_path, encoding) {
        Ok(text) => text,
        Err(message) => {
            for line in message.lines() {
                sink.emit(Stream::Stdout, line);
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
    apply_device_related_settings(&mut settings, selected_device);
    let include_illust = settings.enable_illust;

    let mut converter =
        if let Some(user_converter) = UserConverter::load_with_title(&archive_path, &source_name) {
            NovelConverter::with_user_converter(settings, user_converter)
        } else {
            NovelConverter::new(settings)
        };
    converter.set_display_inspector(inspect);

    let result = match output_device {
        Some(device) => converter.convert_text_file_with_device(&text, device, no_strip, verbose),
        None => converter.convert_text_file(&text),
    };

    match result {
        Ok(output_path) => {
            if create_ibunko_side_epub {
                match create_ibunko_epub_output(
                    Path::new(&output_path),
                    narou_rs::converter::device::Device::Epub,
                    include_illust,
                    verbose,
                ) {
                    Ok(Some(epub_path)) => {
                        if let Some(fname) = epub_path.file_name().and_then(|n| n.to_str()) {
                            sink.emit(Stream::Stdout, &messages::convert::output_written(fname));
                        }
                        sink.emit(
                            Stream::Stdout,
                            &bold_colored(messages::convert::epub_written(), "green"),
                        );
                    }
                    Ok(None) => {}
                    Err(err) => {
                        sink.emit(Stream::Stdout, &err);
                    }
                }
            }
            let output_lower = output_path.to_ascii_lowercase();
            if let Some(fname) = Path::new(&output_path).file_name().and_then(|n| n.to_str()) {
                        sink.emit(Stream::Stdout, &messages::convert::output_written(fname));
            }
            if output_lower.ends_with(".mobi") || output_lower.ends_with(".azw3") {
                        sink.emit(
                            Stream::Stdout,
                            &bold_colored(messages::convert::mobi_written(), "green"),
                        );
            } else if output_lower.ends_with(".epub") || output_lower.ends_with(".kepub.epub") {
                        sink.emit(
                            Stream::Stdout,
                            &bold_colored(messages::convert::epub_written(), "green"),
                        );
            }
            if first_output_dir.is_none() {
                *first_output_dir = Path::new(&output_path)
                    .parent()
                    .map(|path| path.to_path_buf());
            }
            if let Err(err) = print_copy_to_result(&output_path, copy_device, 0, sink) {
                sink.emit(Stream::Stdout, &err);
            }
            if output_path.to_ascii_lowercase().ends_with(".zip")
                && let Err(err) = print_copy_zip_to_result(&output_path, sink) {
                    sink.emit(Stream::Stdout, &err);
                }
            if let Some(device) = copy_device
                && let Err(err) = print_send_result(&output_path, device) {
                    sink.emit(Stream::Stdout, &err);
                }
            print_inspection_output(&mut converter, sink);
        }
        Err(e) => {
            sink.emit(Stream::Stdout, &messages::indented_error(e));
        }
    }
}

fn effective_convert_device(
    selected_device: Option<narou_rs::converter::device::Device>,
    no_epub: bool,
    no_mobi: bool,
    no_zip: bool,
) -> Option<narou_rs::converter::device::Device> {
    match selected_device {
        Some(narou_rs::converter::device::Device::Ibunko) => {
            if !no_zip {
                Some(narou_rs::converter::device::Device::Ibunko)
            } else if !no_epub {
                Some(narou_rs::converter::device::Device::Epub)
            } else {
                None
            }
        }
        _ if no_epub => None,
        Some(narou_rs::converter::device::Device::Mobi) if no_mobi => {
            Some(narou_rs::converter::device::Device::Epub)
        }
        other => other,
    }
}

fn resolve_selected_devices(
    make_zip: bool,
    sink: &dyn MessageSink,
) -> std::result::Result<Vec<Option<narou_rs::converter::device::Device>>, ()> {
    if make_zip {
        return Ok(vec![Some(narou_rs::converter::device::Device::Ibunko)]);
    }

    if let Some(device) = load_web_worker_device_override() {
        return Ok(vec![device]);
    }

    let Some(raw) = narou_rs::compat::load_local_setting_string("convert.multi-device") else {
        return Ok(vec![narou_rs::compat::current_device()]);
    };

    let mut devices = Vec::new();
    for name in raw.split(',').map(str::trim) {
        if name.is_empty() {
            continue;
        }
        let parsed = parse_device_name(name);
        if let Some(device) = parsed {
            devices.push(Some(device));
        } else {
            sink.emit(
                Stream::Stdout,
                &messages::convert::invalid_device_name(name),
            );
        }
    }

    if devices.is_empty() {
        sink.emit(Stream::Stdout, messages::convert::no_valid_devices());
        return Err(());
    }

    if let Some(index) = devices.iter().position(|device| {
        matches!(device, Some(narou_rs::converter::device::Device::Mobi))
    }) {
        let kindle = devices.remove(index);
        devices.insert(0, kindle);
    }

    Ok(devices)
}

fn load_web_worker_device_override() -> Option<Option<narou_rs::converter::device::Device>> {
    let raw = std::env::var("NAROU_RS_WEB_DEVICE").ok()?;
    parse_web_worker_device_name(&raw)
}

fn parse_device_name(name: &str) -> Option<narou_rs::converter::device::Device> {
    match name.trim().to_ascii_lowercase().as_str() {
        "kindle" => Some(narou_rs::converter::device::Device::Mobi),
        "kobo" => Some(narou_rs::converter::device::Device::Kobo),
        "epub" => Some(narou_rs::converter::device::Device::Epub),
        "ibunko" => Some(narou_rs::converter::device::Device::Ibunko),
        "reader" => Some(narou_rs::converter::device::Device::Reader),
        "ibooks" => Some(narou_rs::converter::device::Device::Ibooks),
        _ => None,
    }
}

fn parse_web_worker_device_name(
    name: &str,
) -> Option<Option<narou_rs::converter::device::Device>> {
    if name.trim().eq_ignore_ascii_case("text") {
        Some(None)
    } else {
        parse_device_name(name).map(Some)
    }
}

fn effective_copy_device(
    selected_device: Option<narou_rs::converter::device::Device>,
    output_device: Option<narou_rs::converter::device::Device>,
) -> Option<narou_rs::converter::device::Device> {
    output_device?;
    match (selected_device, output_device) {
        (
            Some(narou_rs::converter::device::Device::Ibunko),
            Some(narou_rs::converter::device::Device::Epub),
        ) => None,
        _ => selected_device,
    }
}

fn apply_device_related_settings(
    settings: &mut NovelSettings,
    device: Option<narou_rs::converter::device::Device>,
) {
    let Some(device) = device else {
        return;
    };
    settings.enable_half_indent_bracket =
        matches!(device, narou_rs::converter::device::Device::Mobi);
}

fn create_ibunko_epub_output(
    primary_output_path: &Path,
    epub_device: narou_rs::converter::device::Device,
    include_illust: bool,
    verbose: bool,
) -> std::result::Result<Option<PathBuf>, String> {
    if !primary_output_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase().ends_with(".zip"))
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let txt_path = primary_output_path.with_extension("txt");
    if !txt_path.exists() {
        return Ok(None);
    }
    let output_dir = txt_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let base_name = txt_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("output");
    let path = narou_rs::converter::device::OutputManager::new(epub_device)
        .with_verbose(verbose)
        .convert_file(&txt_path, output_dir, base_name, include_illust)
        .map_err(|e| e.to_string())?;
    Ok(Some(path))
}

fn load_dc_subjects_for_novel(novel_id: i64) -> std::result::Result<Option<Vec<String>>, String> {
    if !narou_rs::compat::load_local_setting_bool("convert.add-dc-subject-to-epub") {
        return Ok(None);
    }

    let record = narou_rs::native::novel_repository::NativeNovelRepository::new()
        .get_sync(novel_id.into())
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
    narou_rs::db::settings::update_with_inventory(
        &inventory,
        narou_rs::setting_core::SettingScope::Local,
        |settings| {
            settings.insert(
                "convert.dc-subject-exclude-tags".to_string(),
                serde_yaml::Value::String(default_value.clone()),
            );
            Ok(())
        },
    )
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
    sink: &dyn MessageSink,
) {
    let Some(subjects) = subjects else {
        return;
    };
    if subjects.is_empty() || !is_epub_output(output_path) {
        return;
    }

    match add_dc_subject_to_epub(output_path, subjects) {
        Ok(()) => {
            sink.emit(Stream::Stdout, &messages::convert::dc_subject_added(subjects.join(", ")));
        }
        Err(err) => {
            sink.emit(Stream::Stdout, &messages::convert::dc_subject_error(err));
            sink.emit(Stream::Stdout, messages::convert::dc_subject_continue());
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
        return Err(messages::convert::opf_missing().to_string());
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
            return Err(messages::convert::metadata_close_missing().to_string());
        }
        content = metadata_re
            .replace(&content, format!("\n{}\n$1</metadata>", subject_lines.join("\n")))
            .into_owned();
    }
    entries[opf_index].1 = content.into_bytes();

    let Some(mimetype_index) = entries.iter().position(|(name, _)| name == "mimetype") else {
        return Err(messages::convert::mimetype_missing().to_string());
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

fn print_inspection_output(converter: &mut NovelConverter, sink: &dyn MessageSink) {
    if let Some(inspection) = converter.take_inspection_output() {
        for line in inspection.split('\n') {
            sink.emit(Stream::Stdout, line);
        }
    }
}

fn print_copy_to_result(
    output_path: &str,
    device: Option<narou_rs::converter::device::Device>,
    novel_id: i64,
    sink: &dyn MessageSink,
) -> std::result::Result<(), String> {
    let Some(device) = device else {
        return Ok(());
    };
    if let Some(path) =
        narou_rs::compat::copy_to_converted_file(Path::new(output_path), Some(device), novel_id)?
    {
        sink.emit(Stream::Stdout, &messages::copied_to(path.display()));
    }
    Ok(())
}

fn print_send_result(
    output_path: &str,
    device: narou_rs::converter::device::Device,
) -> std::result::Result<(), String> {
    narou_rs::compat::send_file_to_device(Path::new(output_path), device)
}

fn print_copy_zip_to_result(
    output_path: &str,
    sink: &dyn MessageSink,
) -> std::result::Result<(), String> {
    let Some(copy_to_dir) =
        narou_rs::compat::load_local_setting_string("convert.copy-zip-to")
    else {
        return Ok(());
    };
    let base = PathBuf::from(&copy_to_dir);
    if !base.is_dir() {
        return Err(messages::convert::zip_copy_dest_not_dir(copy_to_dir));
    }
    let dst = base.join(
        Path::new(output_path)
            .file_name()
            .ok_or_else(|| messages::convert::invalid_zip_filename().to_string())?,
    );
    std::fs::copy(output_path, &dst).map_err(|e| e.to_string())?;
    sink.emit(Stream::Stdout, &messages::convert::zip_copied_to(dst.display()));
    Ok(())
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
        return Err(messages::convert::enc_invalid());
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
    let bytes = std::fs::read(path).map_err(|e| messages::indented_error(e))?;
    let decoded = match encoding {
        Some(label) => decode_text_file_with_encoding(&bytes, label, path)?,
        None => decode_text_file_as_utf8(&bytes)?,
    };
    Ok(decoded.replace('\r', ""))
}

fn decode_text_file_as_utf8(bytes: &[u8]) -> std::result::Result<String, String> {
    let bytes = strip_utf8_bom(bytes);
    String::from_utf8(bytes.to_vec())
        .map_err(|_| messages::convert::text_not_utf8().to_string())
}

fn decode_text_file_with_encoding(
    bytes: &[u8],
    label: &str,
    path: &Path,
) -> std::result::Result<String, String> {
    let encoding = resolve_text_file_encoding(label)
        .ok_or_else(|| messages::convert::enc_invalid().to_string())?;

    if encoding == UTF_8 {
        return String::from_utf8(strip_utf8_bom(bytes).to_vec())
            .map_err(|_| messages::convert::encoding_mismatch(path.display(), label));
    }

    let (decoded, _, had_errors) = encoding.decode(bytes);
    if had_errors {
        return Err(messages::convert::encoding_mismatch(path.display(), label));
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
        normalize_text_file_encoding_name, parse_device_name, parse_web_worker_device_name,
        print_copy_to_result, split_output_name,
    };

    /// 出力を捨てるだけの sink (コピー処理の副作用だけを検証するテスト用)。
    struct TestSink;

    impl narou_rs::application::messages::MessageSink for TestSink {
        fn emit(&self, _stream: narou_rs::application::messages::Stream, _text: &str) {}
    }

    #[test]
    fn split_output_name_ignores_directory_part() {
        // `/` separates path components on every platform, `\` only on Windows.
        assert_eq!(
            split_output_name("/tmp/custom-name.epub"),
            ("custom-name".to_string(), ".epub".to_string())
        );
        #[cfg(windows)]
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

    #[test]
    fn parse_device_name_accepts_web_worker_device_names() {
        assert!(matches!(
            parse_device_name("kindle"),
            Some(narou_rs::converter::device::Device::Mobi)
        ));
        assert!(parse_device_name("unknown").is_none());
    }

    #[test]
    fn parse_web_worker_device_name_accepts_text_override() {
        assert_eq!(parse_web_worker_device_name("text"), Some(None));
    }

    #[test]
    fn copy_to_result_skips_when_device_is_none() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::set_current_dir_for_test(temp.path());
        let narou_dir = temp.path().join(".narou");
        let copy_to = temp.path().join("copy-to");
        std::fs::create_dir_all(&narou_dir).unwrap();
        std::fs::create_dir_all(&copy_to).unwrap();
        std::fs::write(
            narou_dir.join("local_setting.yaml"),
            format!(
                "convert.copy-to: \"{}\"\n",
                copy_to.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let output = temp.path().join("novel.txt");
        std::fs::write(&output, "text").unwrap();

        print_copy_to_result(output.to_str().unwrap(), None, 0, &TestSink).unwrap();

        assert!(!copy_to.join("novel.txt").exists());
    }
}
