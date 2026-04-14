use narou_rs::converter::NovelConverter;
use narou_rs::converter::settings::NovelSettings;
use narou_rs::converter::user_converter::UserConverter;
use narou_rs::progress::CliProgress;

use super::resolve_target_to_id;

pub fn cmd_convert(targets: &[String], inspect: bool) {
    if let Err(e) = narou_rs::db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    let multi = CliProgress::multi();
    let multi_clone = multi.clone();

    for target in targets {
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

        let settings = NovelSettings::load_for_novel(id, &title, &author, &novel_dir);
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
}
