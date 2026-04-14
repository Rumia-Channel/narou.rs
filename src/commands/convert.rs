use narou_rs::converter::NovelConverter;
use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::progress::CliProgress;

use super::resolve_target_to_id;

pub fn cmd_convert(
    targets: &[String],
    output: Option<&str>,
    inspect: bool,
    no_open: bool,
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

    for (index, target) in targets.iter().enumerate() {
        let Some(id) = resolve_target_to_id(target) else {
            let _ = multi_clone.println(&format!("  Not found: {}", target));
            continue;
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
        if let Some((basename, ext)) = &output_parts {
            settings.output_filename = build_output_filename(
                basename,
                ext,
                if targets.len() > 1 {
                    Some(index + 1)
                } else {
                    None
                },
            );
        }
        let mut converter =
            if let Some(user_converter) = UserConverter::load_with_title(&novel_dir, &title) {
                NovelConverter::with_user_converter(settings, user_converter)
            } else {
                NovelConverter::new(settings)
            };
        converter.set_progress(Box::new(progress));
        converter.set_display_inspector(inspect);

        match converter.convert_novel_by_id(id, &novel_dir) {
            Ok(output_path) => {
                let _ = multi_clone.println(&format!("  Output: {}", output_path));
                if first_output_dir.is_none() {
                    first_output_dir = std::path::Path::new(&output_path)
                        .parent()
                        .map(|path| path.to_path_buf());
                }
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

fn split_output_name(output: &str) -> (String, String) {
    let path = std::path::Path::new(output);
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

#[cfg(test)]
mod tests {
    use super::{build_output_filename, split_output_name};

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
}
