use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use similar::TextDiff;
use tempfile::{NamedTempFile, TempPath};

use crate::commands::download;
use crate::commands::log;
use crate::logger;
use narou_rs::compat;
use narou_rs::db::inventory::InventoryScope;
use narou_rs::db::{self, NovelRecord};
use narou_rs::downloader::html;
use narou_rs::downloader::types::{CACHE_SAVE_DIR, SECTION_SAVE_DIR, SectionFile};

pub struct DiffOptions {
    pub target: Option<String>,
    pub view_diff_version: Option<String>,
    pub number: usize,
    pub list: bool,
    pub clean: bool,
    pub all_clean: bool,
    pub no_tool: bool,
}

#[derive(Clone)]
struct NovelContext {
    record: NovelRecord,
    archive_root: PathBuf,
}

struct DiffTexts {
    old: String,
    new: String,
}

#[derive(Clone)]
struct RenderedSection {
    chapter: String,
    subtitle: String,
    subdate: String,
    subupdate: Option<String>,
    introduction: String,
    body: String,
    postscript: String,
}

pub fn cmd_diff(opts: DiffOptions) -> i32 {
    logger::without_logging(|| match cmd_diff_inner(opts) {
        Ok(()) => 0,
        Err(message) => {
            log::report_error(&message);
            127
        }
    })
}

fn cmd_diff_inner(opts: DiffOptions) -> std::result::Result<(), String> {
    db::init_database().map_err(|e| e.to_string())?;

    let context = match resolve_context(opts.target.as_deref())? {
        Some(context) => context,
        None => return Ok(()),
    };

    if let Some(version) = opts.view_diff_version.as_deref() {
        if invalid_diff_version_string(version) {
            return Err("差分指定の書式が違います(正しい例:2013.02.21@01.39.46)".to_string());
        }
    }

    if opts.list {
        display_diff_list(&context)?;
        return Ok(());
    }

    if opts.clean {
        clean_diff(&context)?;
        return Ok(());
    }

    if opts.all_clean {
        clean_all_diff()?;
        return Ok(());
    }

    let difftool = load_global_setting_string("difftool")?;
    if opts.no_tool || difftool.is_none() {
        display_diff_on_oneself(&context, opts.view_diff_version.as_deref(), opts.number)?;
        return Ok(());
    }

    exec_difftool(
        &context,
        opts.view_diff_version.as_deref(),
        opts.number,
        difftool.as_deref().unwrap(),
    )?;
    Ok(())
}

fn resolve_context(target: Option<&str>) -> std::result::Result<Option<NovelContext>, String> {
    match target {
        Some(target) => {
            let Some(data) = download::get_data_by_target(target) else {
                return Err(format!("{} は存在しません", target));
            };

            let context = db::with_database(|db| {
                let record = db.get(data.id).cloned().ok_or_else(|| {
                    narou_rs::error::NarouError::NotFound(format!("ID: {}", data.id))
                })?;
                Ok(NovelContext {
                    record,
                    archive_root: db.archive_root().to_path_buf(),
                })
            })
            .map_err(|e| e.to_string())?;
            Ok(Some(context))
        }
        None => db::with_database(|db| {
            let latest = db.sort_by("last_update", true).into_iter().next().cloned();
            Ok(latest.map(|record| NovelContext {
                record,
                archive_root: db.archive_root().to_path_buf(),
            }))
        })
        .map_err(|e| e.to_string()),
    }
}

fn invalid_diff_version_string(version: &str) -> bool {
    static DIFF_VERSION_RE: OnceLock<Regex> = OnceLock::new();
    let re = DIFF_VERSION_RE
        .get_or_init(|| Regex::new(r"^\d{4}\.\d{2}\.\d{2}@\d{2}[;.]\d{2}[;.]\d{2}$").unwrap());
    !re.is_match(version)
}

fn load_global_setting_string(key: &str) -> std::result::Result<Option<String>, String> {
    db::with_database(|db| {
        let settings: HashMap<String, serde_yaml::Value> = db
            .inventory()
            .load("global_setting", InventoryScope::Global)?;
        Ok(settings.get(key).and_then(compat::yaml_value_to_string))
    })
    .map_err(|e| e.to_string())
}

fn cache_root_dir(context: &NovelContext) -> PathBuf {
    narou_rs::db::existing_novel_dir_for_record(&context.archive_root, &context.record)
        .join(SECTION_SAVE_DIR)
        .join(CACHE_SAVE_DIR)
}

fn select_cache_dir(
    context: &NovelContext,
    view_diff_version: Option<&str>,
    number: usize,
) -> std::result::Result<Option<PathBuf>, String> {
    let cache_root = cache_root_dir(context);
    if !cache_root.is_dir() {
        return Ok(None);
    }

    if let Some(version) = view_diff_version {
        let cache_dir = cache_root.join(version);
        return Ok(cache_dir.is_dir().then_some(cache_dir));
    }

    let mut list = read_cache_dir_list(&cache_root)?;
    list.sort_by(|a, b| {
        let a_name = a
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_string();
        let b_name = b
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_string();
        b_name.cmp(&a_name)
    });
    Ok(list.get(number.saturating_sub(1)).cloned())
}

fn read_cache_dir_list(cache_root: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    let mut list = Vec::new();
    for entry in fs::read_dir(cache_root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            list.push(path);
        }
    }
    Ok(list)
}

fn read_sorted_yaml_files(dir: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    let mut list = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("yaml") {
            list.push(path);
        }
    }
    list.sort_by_key(|path| parse_section_index(path).unwrap_or(0));
    Ok(list)
}

fn parse_section_index(path: &Path) -> Option<usize> {
    let stem = path.file_stem()?.to_str()?;
    let (index, _) = stem.split_once(' ')?;
    index.parse().ok()
}

fn load_section_file(path: &Path) -> std::result::Result<SectionFile, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_yaml::from_str(&content).map_err(|e| e.to_string())
}

fn render_section(section: SectionFile) -> RenderedSection {
    let mut introduction = section.element.introduction;
    let mut body = section.element.body;
    let mut postscript = section.element.postscript;

    if section.element.data_type == "html" {
        introduction = html::to_aozora_strip_decoration(&introduction);
        body = html::to_aozora_strip_decoration(&body);
        postscript = html::to_aozora_strip_decoration(&postscript);
    }

    RenderedSection {
        chapter: section.chapter,
        subtitle: section.subtitle,
        subdate: section.subdate,
        subupdate: section.subupdate,
        introduction,
        body,
        postscript,
    }
}

fn build_diff_texts(
    context: &NovelContext,
    view_diff_version: Option<&str>,
    number: usize,
) -> std::result::Result<Option<DiffTexts>, String> {
    let Some(cache_dir) = select_cache_dir(context, view_diff_version, number)? else {
        println!("{} の差分データがありません", context.record.title);
        return Ok(None);
    };

    let cache_section_list = read_sorted_yaml_files(&cache_dir)?;
    if cache_section_list.is_empty() {
        println!(
            "{} は最新話のみのアップデートのようです",
            context.record.title
        );
        return Ok(None);
    }

    let novel_dir =
        narou_rs::db::existing_novel_dir_for_record(&context.archive_root, &context.record);
    let section_dir = novel_dir.join(SECTION_SAVE_DIR);

    let mut cache_sections = Vec::new();
    let mut latest_sections = Vec::new();

    for cache_path in cache_section_list {
        let Some(file_name) = cache_path.file_name() else {
            continue;
        };
        let current_path = section_dir.join(file_name);

        let cache_section = load_section_file(&cache_path)?;
        cache_sections.push(render_section(cache_section));

        if current_path.exists() {
            let latest_section = load_section_file(&current_path)?;
            latest_sections.push(render_section(latest_section));
        }
    }

    Ok(Some(DiffTexts {
        old: render_diff_text(&context.record.title, &cache_sections),
        new: render_diff_text(&context.record.title, &latest_sections),
    }))
}

fn render_diff_text(title: &str, sections: &[RenderedSection]) -> String {
    let mut out = String::new();
    let separator = "―".repeat(29);
    push_line(&mut out, title);
    out.push('\n');
    push_line(&mut out, "(この差分データに含まれる話数一覧)");

    for section in sections {
        if !section.chapter.is_empty() {
            push_line(&mut out, &section.chapter);
        }
        let subdate = section
            .subupdate
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or(section.subdate.as_str());
        push_line(
            &mut out,
            &format!("・{} {}", section.subtitle.trim_end(), subdate),
        );
    }

    push_line(&mut out, &separator);
    out.push('\n');

    for section in sections {
        push_line(&mut out, &format!("　　　{}", section.subtitle.trim_end()));
        if !section.introduction.is_empty() {
            push_line(&mut out, "(前書き)");
            push_line(&mut out, &section.introduction);
            push_line(
                &mut out,
                "**********************************************************",
            );
        }
        out.push('\n');
        push_line(&mut out, &section.body);
        if !section.postscript.is_empty() {
            push_line(
                &mut out,
                "**********************************************************",
            );
            push_line(&mut out, "(後書き)");
            push_line(&mut out, &section.postscript);
        }
        push_line(&mut out, &separator);
        out.push('\n');
    }

    out
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

fn display_diff_list(context: &NovelContext) -> std::result::Result<(), String> {
    let cache_root = cache_root_dir(context);
    print!("{} の", context.record.title);
    if !cache_root.is_dir() {
        println!("差分はひとつもありません");
        return Ok(());
    }

    let mut cache_list = read_cache_dir_list(&cache_root)?;
    cache_list.sort_by(|a, b| {
        let a_name = a
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_string();
        let b_name = b
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_string();
        b_name.cmp(&a_name)
    });

    if cache_list.is_empty() {
        println!("差分はひとつもありません");
        return Ok(());
    }

    println!("差分一覧");

    for (number, cache_dir) in cache_list.iter().enumerate() {
        let version_string = cache_dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        println!(
            "{}",
            bold_yellow(&format!("{}   -{}", version_string, number + 1))
        );

        let section_list = read_sorted_yaml_files(cache_dir)?;
        let mut objects: Vec<(String, String)> = Vec::new();
        for section_path in section_list {
            let section = load_section_file(&section_path)?;
            objects.push((section.index, section.subtitle));
        }

        if objects.is_empty() {
            println!("   (最新話のみのアップデート)");
            continue;
        }

        for (index, subtitle) in objects {
            println!("   第{}部分　{}", index, subtitle.trim_end());
        }
    }

    Ok(())
}

fn clean_diff(context: &NovelContext) -> std::result::Result<(), String> {
    let cache_root = cache_root_dir(context);
    print!("{} の", context.record.title);
    if !cache_root.exists() {
        println!("差分はひとつもありません");
        return Ok(());
    }

    fs::remove_dir_all(&cache_root).map_err(|e| e.to_string())?;
    println!("差分を削除しました");
    Ok(())
}

fn clean_all_diff() -> std::result::Result<(), String> {
    let (records, archive_root) = db::with_database(|db| {
        Ok((
            db.sort_by("id", false)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>(),
            db.archive_root().to_path_buf(),
        ))
    })
    .map_err(|e| e.to_string())?;

    let frozen_ids = compat::load_frozen_ids().map_err(|e| e.to_string())?;

    for record in records {
        if compat::record_is_frozen(&record, &frozen_ids) {
            continue;
        }

        let context = NovelContext {
            archive_root: archive_root.clone(),
            record,
        };
        let cache_root = cache_root_dir(&context);
        if !cache_root.exists() {
            continue;
        }
        fs::remove_dir_all(&cache_root).map_err(|e| e.to_string())?;
        println!("{} の差分を削除しました", context.record.title);
    }

    Ok(())
}

fn display_diff_on_oneself(
    context: &NovelContext,
    view_diff_version: Option<&str>,
    number: usize,
) -> std::result::Result<(), String> {
    let Some(texts) = build_diff_texts(context, view_diff_version, number)? else {
        return Ok(());
    };

    println!("{} の差分を表示します", context.record.title);
    print!("{}", render_builtin_diff(&texts.old, &texts.new));
    Ok(())
}

fn exec_difftool(
    context: &NovelContext,
    view_diff_version: Option<&str>,
    number: usize,
    difftool: &str,
) -> std::result::Result<(), String> {
    let Some(texts) = build_diff_texts(context, view_diff_version, number)? else {
        return Ok(());
    };

    let temp_files = create_temp_files(&texts)?;
    let diff_output = run_difftool(difftool, &temp_files)?;

    print_stdout(&diff_output.stdout);
    if !diff_output.stderr.trim().is_empty() {
        log::report_error(&diff_output.stderr);
    }

    Ok(())
}

fn create_temp_files(texts: &DiffTexts) -> std::result::Result<DiffTempFiles, String> {
    Ok(DiffTempFiles {
        old: write_temp_file(&texts.old)?,
        new: write_temp_file(&texts.new)?,
    })
}

fn write_temp_file(content: &str) -> std::result::Result<TempPath, String> {
    let mut file = NamedTempFile::new().map_err(|e| e.to_string())?;
    file.write_all(content.as_bytes())
        .map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    Ok(file.into_temp_path())
}

struct DiffTempFiles {
    old: TempPath,
    new: TempPath,
}

struct DiffOutput {
    stdout: String,
    stderr: String,
}

fn run_difftool(
    difftool: &str,
    temp_files: &DiffTempFiles,
) -> std::result::Result<DiffOutput, String> {
    let old_path = temp_files.old.to_path_buf().display().to_string();
    let new_path = temp_files.new.to_path_buf().display().to_string();
    let mut command = std::process::Command::new(difftool);

    if let Some(diff_args) = load_global_setting_string("difftool.arg")? {
        let args = shell_words::split(&diff_args).map_err(|e| e.to_string())?;
        let args = args
            .into_iter()
            .map(|arg| arg.replace("%OLD", &old_path).replace("%NEW", &new_path))
            .collect::<Vec<_>>();
        command.args(args);
    } else {
        command.arg(&old_path).arg(&new_path);
    }

    compat::configure_hidden_console_command(&mut command);
    let output = command.output().map_err(|e| e.to_string())?;
    Ok(DiffOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn print_stdout(text: &str) {
    if text.is_empty() {
        return;
    }
    print!("{}", text);
    if !text.ends_with('\n') {
        println!();
    }
}

fn render_builtin_diff(old_text: &str, new_text: &str) -> String {
    let old_text = old_text.trim_end_matches('\n');
    let new_text = new_text.trim_end_matches('\n');
    let diff = TextDiff::from_lines(old_text, new_text);
    let unified = format!(
        "{}",
        diff.unified_diff().context_radius(3).header("old", "new")
    );
    let header_re = Regex::new(r"^@@ -(\d+),(\d+) \+(\d+),(\d+) @@$").unwrap();

    let mut out = String::new();
    for line in unified.lines().skip(2) {
        let line = if let Some(caps) = header_re.captures(line) {
            format!("@@ -{}, +{} @@", &caps[1], &caps[3])
        } else {
            line.to_string()
        };

        if line.starts_with("@@") {
            out.push_str(&bold_cyan(&line));
        } else if line.starts_with('-') {
            out.push_str(&bold_red(&line));
        } else if line.starts_with('+') {
            out.push_str(&bold_green(&line));
        } else {
            out.push_str(&line);
        }
        out.push('\n');
    }
    out
}

fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

fn bold_red(s: &str) -> String {
    if use_color() {
        format!("\x1b[1;31m{}\x1b[0m", s)
    } else {
        s.to_string()
    }
}

fn bold_green(s: &str) -> String {
    if use_color() {
        format!("\x1b[1;32m{}\x1b[0m", s)
    } else {
        s.to_string()
    }
}

fn bold_cyan(s: &str) -> String {
    if use_color() {
        format!("\x1b[1;36m{}\x1b[0m", s)
    } else {
        s.to_string()
    }
}

fn bold_yellow(s: &str) -> String {
    if use_color() {
        format!("\x1b[1;33m{}\x1b[0m", s)
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::invalid_diff_version_string;

    #[test]
    fn diff_version_validation_matches_ruby_format() {
        assert!(!invalid_diff_version_string("2024.01.03@04.05.06"));
        assert!(!invalid_diff_version_string("2024.01.03@04;05;06"));
        assert!(invalid_diff_version_string("2024-01-03@04.05.06"));
        assert!(invalid_diff_version_string("2024.01.03 04.05.06"));
    }
}
