use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Local, Utc};
use narou_rs::compat::confirm;
use narou_rs::db::inventory::{Inventory, InventoryScope};
use narou_rs::db::novel_record::NovelRecord;

use super::{download, help, log};

use crate::logger;

const ANNOTATION_COLOR_TIME_LIMIT: i64 = 6 * 60 * 60;
const TAG_COLOR_ORDER: [&str; 7] = ["green", "yellow", "blue", "magenta", "cyan", "red", "white"];
const FILTER_TYPE_HELP: &str = "series(連載),ss(短編),frozen(凍結),nonfrozen(非凍結)";

#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    pub limit: Option<usize>,
    pub latest: bool,
    pub general_lastup: bool,
    pub reverse: bool,
    pub url: bool,
    pub kind: bool,
    pub site: bool,
    pub author: bool,
    pub filter: Option<String>,
    pub grep: Option<String>,
    pub tag: Option<Option<String>>,
    pub echo: bool,
    pub frozen: bool,
}

impl ListOptions {
    fn view_date_type(&self) -> &'static str {
        if self.general_lastup {
            "general_lastup"
        } else {
            "last_update"
        }
    }

    fn header(&self) -> String {
        [
            Some(" ID ".to_string()),
            Some(if self.general_lastup {
                " 掲載日 ".to_string()
            } else {
                " 更新日 ".to_string()
            }),
            self.kind.then(|| "種別".to_string()),
            self.author.then(|| "作者名".to_string()),
            self.site.then(|| "サイト名".to_string()),
            Some("     タイトル".to_string()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" | ")
    }

    fn filter_words(&self) -> Result<Vec<String>, String> {
        let mut filters = split_words(self.filter.as_deref());
        if self.frozen {
            filters.push("frozen".to_string());
        }
        if let Some(invalid) = filters
            .iter()
            .find(|item| !matches!(item.as_str(), "series" | "ss" | "frozen" | "nonfrozen"))
        {
            return Err(format!(
                "不明なフィルターです({})\nfilters = {}",
                invalid, FILTER_TYPE_HELP
            ));
        }
        Ok(filters)
    }

    fn grep_words(&self) -> Vec<String> {
        split_words(self.grep.as_deref())
    }

    fn tag_filters(&self) -> Vec<String> {
        match &self.tag {
            Some(Some(tags)) => split_words(Some(tags.as_str())),
            _ => Vec::new(),
        }
    }

    fn show_tags(&self) -> bool {
        self.tag.is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct TagOptions {
    pub add: Option<String>,
    pub delete: Option<String>,
    pub color: Option<String>,
    pub clear: bool,
    pub no_overwrite_color: bool,
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct TagColors {
    order: Vec<String>,
    colors: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct DecoratedNovel {
    id: i64,
    plain: String,
    colored: String,
}

#[derive(Debug, Clone)]
enum TagMode {
    List,
    Add(Vec<String>),
    Delete(Vec<String>),
    Clear,
}

impl TagMode {
    fn tag_names(&self) -> Option<&[String]> {
        match self {
            Self::Add(tags) | Self::Delete(tags) => Some(tags.as_slice()),
            Self::List | Self::Clear => None,
        }
    }
}

enum TagOutput {
    Info(String),
    Current(Vec<String>),
    Error(String),
}

pub fn cmd_list(options: ListOptions) -> i32 {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        return 1;
    }

    let mut exit_code = 0;
    logger::without_logging(|| {
        exit_code = cmd_list_inner(&options);
    });
    exit_code
}

fn cmd_list_inner(options: &ListOptions) -> i32 {
    use narou_rs::db;

    let filter_words = match options.filter_words() {
        Ok(filters) => filters,
        Err(message) => {
            let mut lines = message.lines();
            if let Some(first) = lines.next() {
                log::report_error(first);
            }
            for line in lines {
                println!("{}", line);
            }
            return 127;
        }
    };

    let grep_words = options.grep_words();
    let tag_filters = options.tag_filters();
    let stdout_is_tty = std::io::stdout().is_terminal();
    let frozen_ids = load_inventory_ids("freeze");

    let records = match db::with_database(|db| {
        let mut values = if options.latest {
            db.sort_by(options.view_date_type(), false)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        } else {
            let mut records = db.all_records().values().cloned().collect::<Vec<_>>();
            records.sort_by_key(|record| record.id);
            records
        };
        if options.reverse {
            values.reverse();
        }
        Ok(values)
    }) {
        Ok(records) => records,
        Err(err) => {
            log::report_error(&err.to_string());
            return 1;
        }
    };

    let mut tag_colors = match load_tag_colors() {
        Ok(colors) => colors,
        Err(err) => {
            log::report_error(&err);
            return 1;
        }
    };

    let mut selected = Vec::new();
    for record in &records {
        let frozen = frozen_ids.contains(&record.id);
        if !matches_filters(record, frozen, &filter_words) {
            continue;
        }
        if !valid_tags(record, &tag_filters) {
            continue;
        }

        let plain = decorate_line(record, options, frozen, &tag_colors, false);
        if !matches_grep(&plain, &grep_words) {
            continue;
        }
        selected.push((record.clone(), frozen, plain));
    }

    let colors_changed = if options.show_tags() {
        ensure_tag_colors(
            &mut tag_colors,
            selected
                .iter()
                .flat_map(|(record, _, _)| record.tags.iter().map(String::as_str)),
        )
    } else {
        false
    };

    let lines = selected
        .into_iter()
        .map(|(record, frozen, plain)| DecoratedNovel {
            id: record.id,
            colored: if stdout_is_tty {
                decorate_line(&record, options, frozen, &tag_colors, true)
            } else {
                plain.clone()
            },
            plain,
        })
        .collect::<Vec<_>>();

    if colors_changed {
        if let Err(err) = save_tag_colors(&tag_colors) {
            log::report_error(&err);
            return 1;
        }
    }

    let limit = options.limit.unwrap_or(lines.len());
    let taken = lines.into_iter().take(limit).collect::<Vec<_>>();
    output_list(options, &taken, stdout_is_tty);
    0
}

pub fn cmd_tag(options: TagOptions) -> i32 {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        return 1;
    }

    let mode = match build_tag_mode(&options) {
        Ok(mode) => mode,
        Err(err) => {
            log::report_error(&err);
            return 127;
        }
    };

    let mut tag_colors = match load_tag_colors() {
        Ok(colors) => colors,
        Err(err) => {
            log::report_error(&err);
            return 1;
        }
    };

    let mut explicit_color_changed = false;
    if let Some(color) = normalize_color(options.color.as_deref()) {
        match color.as_str() {
            "green" | "yellow" | "blue" | "magenta" | "cyan" | "red" | "white" => {
                if let Some(tags) = mode.tag_names() {
                    for tag in tags {
                        explicit_color_changed |=
                            set_tag_color(&mut tag_colors, tag, &color, options.no_overwrite_color);
                    }
                }
            }
            _ => {
                eprintln!("{}という色は存在しません。色指定は無視されます", color);
            }
        }
    }

    if explicit_color_changed {
        if let Err(err) = save_tag_colors(&tag_colors) {
            log::report_error(&err);
            return 1;
        }
    }

    if options.targets.is_empty() {
        if matches!(mode, TagMode::List) {
            return display_tag_list(&mut tag_colors);
        }
        if explicit_color_changed {
            println!("タグの色を変更しました");
            return display_tag_list(&mut tag_colors);
        }
        log::report_error("対象の小説を指定して下さい");
        return 127;
    }

    if matches!(mode, TagMode::List) {
        return cmd_list(ListOptions {
            tag: Some(Some(options.targets.join(" "))),
            ..ListOptions::default()
        });
    }

    let resolved_targets = download::tagname_to_ids(&options.targets);
    let resolved = resolved_targets
        .into_iter()
        .map(|target| {
            download::get_data_by_target(&target)
                .map(|data| (data.id, data.title))
                .ok_or(target)
        })
        .collect::<Vec<_>>();

    let outputs = match db::with_database_mut(|db| {
        let mut outputs = Vec::new();
        let mut auto_color_changed = false;
        for item in resolved {
            let (id, title) = match item {
                Ok(data) => data,
                Err(target) => {
                    outputs.push(TagOutput::Error(format!("{} は存在しません", target)));
                    continue;
                }
            };

            let Some(record) = db.get(id).cloned() else {
                outputs.push(TagOutput::Error(format!("ID:{} は存在しません", id)));
                continue;
            };

            let mut updated = record;
            match &mode {
                TagMode::Add(tags) => {
                    for tag in tags {
                        if !updated.tags.contains(tag) {
                            updated.tags.push(tag.clone());
                        }
                    }
                    outputs.push(TagOutput::Info(format!("{} にタグを設定しました", title)));
                }
                TagMode::Delete(tags) => {
                    updated.tags.retain(|tag| !tags.contains(tag));
                    outputs.push(TagOutput::Info(format!("{} からタグを外しました", title)));
                }
                TagMode::Clear => {
                    updated.tags.clear();
                    outputs.push(TagOutput::Info(format!(
                        "{} のタグをすべて外しました",
                        title
                    )));
                }
                TagMode::List => {}
            }

            if !updated.tags.is_empty() {
                auto_color_changed |=
                    ensure_tag_colors(&mut tag_colors, updated.tags.iter().map(String::as_str));
                outputs.push(TagOutput::Current(updated.tags.clone()));
            }

            db.insert(updated);
        }
        db.save()?;
        Ok::<(Vec<TagOutput>, bool), narou_rs::error::NarouError>((outputs, auto_color_changed))
    }) {
        Ok(result) => result,
        Err(err) => {
            log::report_error(&err.to_string());
            return 1;
        }
    };

    let (outputs, auto_color_changed) = outputs;

    if auto_color_changed {
        if let Err(err) = save_tag_colors(&tag_colors) {
            log::report_error(&err);
            return 1;
        }
    }

    for output in outputs {
        match output {
            TagOutput::Info(message) => println!("{}", message),
            TagOutput::Current(tags) => println!(
                "現在のタグは {} です",
                render_tags(&tags, &tag_colors, " ", true)
            ),
            TagOutput::Error(message) => log::report_error(&message),
        }
    }

    0
}

fn split_words(value: Option<&str>) -> Vec<String> {
    value
        .map(|text| {
            text.split_whitespace()
                .map(|item| item.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn valid_tags(record: &NovelRecord, tags: &[String]) -> bool {
    if tags.is_empty() {
        return true;
    }
    if record.tags.is_empty() {
        return false;
    }
    tags.iter().all(|tag| record.tags.contains(tag))
}

fn matches_filters(record: &NovelRecord, frozen: bool, filters: &[String]) -> bool {
    filters.iter().all(|filter| match filter.as_str() {
        "series" => matches!(record.novel_type, 0 | 1),
        "ss" => record.novel_type == 2,
        "frozen" => frozen,
        "nonfrozen" => !frozen,
        _ => false,
    })
}

fn matches_grep(line: &str, grep_words: &[String]) -> bool {
    grep_words.iter().all(|word| {
        if let Some(negated) = word.strip_prefix('-') {
            !line.contains(negated)
        } else {
            line.contains(word)
        }
    })
}

fn output_list(options: &ListOptions, lines: &[DecoratedNovel], stdout_is_tty: bool) {
    if stdout_is_tty {
        println!("{}", options.header());
        println!(
            "{}",
            lines
                .iter()
                .map(|line| line.colored.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
    } else if options.echo {
        println!("{}", options.header());
        println!(
            "{}",
            lines
                .iter()
                .map(|line| line.plain.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
    } else {
        println!(
            "{}",
            lines
                .iter()
                .map(|line| line.id.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
}

fn decorate_line(
    record: &NovelRecord,
    options: &ListOptions,
    frozen: bool,
    tag_colors: &TagColors,
    colored: bool,
) -> String {
    let mut parts = vec![
        decorate_id(record.id, frozen, colored),
        decorate_date(record, options, colored),
    ];

    if options.kind {
        parts.push(match record.novel_type {
            2 => "短編".to_string(),
            _ => "連載".to_string(),
        });
    }
    if options.author {
        parts.push(record.author.clone());
    }
    if options.site {
        parts.push(record.sitename.clone());
    }

    parts.push(decorate_title(record, options, colored));

    if options.url {
        parts.push(record.toc_url.clone());
    }
    if options.show_tags() {
        if let Some(tags) = decorate_tags(&record.tags, tag_colors, colored) {
            parts.push(tags);
        }
    }

    parts.join(" | ")
}

fn decorate_id(id: i64, frozen: bool, colored: bool) -> String {
    let text = format!(
        "{:>4}",
        if frozen {
            format!("*{}", id)
        } else {
            id.to_string()
        }
    );
    if frozen && colored {
        text.replacen('*', &paint("*", "cyan", true), 1)
    } else {
        text
    }
}

fn decorate_date(record: &NovelRecord, options: &ListOptions, colored: bool) -> String {
    let base_time = if options.general_lastup {
        record.general_lastup
    } else {
        Some(record.last_update)
    };
    let new_arrivals_date = record.new_arrivals_date;
    let last_update = record.last_update;
    let now = Utc::now();
    let limit = Duration::seconds(ANNOTATION_COLOR_TIME_LIMIT);

    if let Some(new_arrival) = new_arrivals_date {
        if new_arrival >= last_update && new_arrival + limit >= now {
            return format_date(new_arrival, colored.then_some("magenta"));
        }
    }

    if last_update + limit >= now {
        return format_date(base_time.unwrap_or(last_update), colored.then_some("green"));
    }

    base_time
        .map(|date| format_date(date, None))
        .unwrap_or_default()
}

fn decorate_title(record: &NovelRecord, options: &ListOptions, colored: bool) -> String {
    let mut parts = vec![record.title.clone()];
    if !options.kind && record.novel_type == 2 {
        parts.push(decorate_annotation("(短編)", colored));
    }
    if record.tags.iter().any(|tag| tag == "end") {
        parts.push(decorate_annotation("(完結)", colored));
    }
    if record.tags.iter().any(|tag| tag == "404") {
        parts.push(decorate_annotation("(削除)", colored));
    }
    parts.join(" ")
}

fn decorate_annotation(text: &str, colored: bool) -> String {
    if colored {
        paint(text, "black", true)
    } else {
        text.to_string()
    }
}

fn decorate_tags(tags: &[String], tag_colors: &TagColors, colored: bool) -> Option<String> {
    if tags.is_empty() {
        return None;
    }
    Some(render_tags(tags, tag_colors, ",", colored))
}

fn format_date(date: DateTime<Utc>, color: Option<&str>) -> String {
    let text = date.with_timezone(&Local).format("%y/%m/%d").to_string();
    color.map_or(text.clone(), |color| paint(&text, color, true))
}

fn paint(text: &str, color: &str, bold: bool) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        return text.to_string();
    }

    let code = match color {
        "black" => 30,
        "red" => 31,
        "green" => 32,
        "yellow" => 33,
        "blue" => 34,
        "magenta" => 35,
        "cyan" => 36,
        "white" => 37,
        _ => return text.to_string(),
    };

    if bold {
        format!("\x1b[1;{}m{}\x1b[0m", code, text)
    } else {
        format!("\x1b[{}m{}\x1b[0m", code, text)
    }
}

fn build_tag_mode(options: &TagOptions) -> Result<TagMode, String> {
    if options.clear {
        return Ok(TagMode::Clear);
    }
    if let Some(tags) = &options.delete {
        return Ok(TagMode::Delete(split_words(Some(tags.as_str()))));
    }
    if let Some(tags) = &options.add {
        let tags = split_words(Some(tags.as_str()));
        for tag in &tags {
            if tag.chars().any(|c| {
                matches!(
                    c,
                    ':' | ';'
                        | '"'
                        | '\''
                        | '>'
                        | '<'
                        | '$'
                        | '@'
                        | '&'
                        | '^'
                        | '\\'
                        | '|'
                        | '%'
                        | '/'
                        | '`'
                )
            }) {
                return Err(format!("{} に使用禁止記号が含まれています", tag));
            }
            if tag == "hotentry" {
                return Err(format!("{} は使用禁止ワードです", tag));
            }
        }
        return Ok(TagMode::Add(tags));
    }
    Ok(TagMode::List)
}

fn normalize_color(color: Option<&str>) -> Option<String> {
    color.map(|value| value.to_ascii_lowercase())
}

fn display_tag_list(tag_colors: &mut TagColors) -> i32 {
    let tag_list = match get_tag_list() {
        Ok(tag_list) => tag_list,
        Err(err) => {
            log::report_error(&err);
            return 1;
        }
    };

    let changed = ensure_tag_colors(tag_colors, tag_list.iter().map(|(tag, _)| tag.as_str()));
    if changed {
        if let Err(err) = save_tag_colors(tag_colors) {
            log::report_error(&err);
            return 1;
        }
    }

    println!("タグ一覧");
    println!(
        "{}",
        tag_list
            .iter()
            .map(|(tag, count)| render_tag_count(tag, *count, tag_colors))
            .collect::<Vec<_>>()
            .join(" ")
    );
    0
}

fn get_tag_list() -> Result<Vec<(String, usize)>, String> {
    use narou_rs::db;

    db::with_database(|db| {
        let mut records = db.all_records().values().collect::<Vec<_>>();
        records.sort_by_key(|record| record.id);

        let mut counts = HashMap::<String, usize>::new();
        let mut order = Vec::<String>::new();
        for record in records {
            for tag in &record.tags {
                if !counts.contains_key(tag) {
                    order.push(tag.clone());
                }
                *counts.entry(tag.clone()).or_insert(0) += 1;
            }
        }

        Ok(order
            .into_iter()
            .map(|tag| {
                let count = counts.get(&tag).copied().unwrap_or_default();
                (tag, count)
            })
            .collect::<Vec<_>>())
    })
    .map_err(|err| err.to_string())
}

fn render_tag_count(tag: &str, count: usize, tag_colors: &TagColors) -> String {
    let text = format!("{}({})", tag, count);
    if std::io::stdout().is_terminal() {
        if let Some(color) = tag_colors.colors.get(tag) {
            return paint(&text, color, true);
        }
    }
    text
}

fn render_tags(tags: &[String], tag_colors: &TagColors, separator: &str, colored: bool) -> String {
    tags.iter()
        .map(|tag| {
            if colored && std::io::stdout().is_terminal() {
                if let Some(color) = tag_colors.colors.get(tag) {
                    return paint(tag, color, true);
                }
            }
            tag.clone()
        })
        .collect::<Vec<_>>()
        .join(separator)
}

fn load_tag_colors() -> Result<TagColors, String> {
    let inventory = Inventory::with_default_root().map_err(|err| err.to_string())?;
    let raw = inventory
        .load_raw("tag_colors", InventoryScope::Local)
        .map_err(|err| err.to_string())?;
    if raw.trim().is_empty() {
        return Ok(TagColors::default());
    }

    let value: serde_yaml::Value = serde_yaml::from_str(&raw).map_err(|err| err.to_string())?;
    let Some(mapping) = value.as_mapping() else {
        return Ok(TagColors::default());
    };

    let mut tag_colors = TagColors::default();
    for (tag, color) in mapping {
        let (Some(tag), Some(color)) = (tag.as_str(), color.as_str()) else {
            continue;
        };
        tag_colors.order.push(tag.to_string());
        tag_colors.colors.insert(tag.to_string(), color.to_string());
    }
    Ok(tag_colors)
}

fn save_tag_colors(tag_colors: &TagColors) -> Result<(), String> {
    let inventory = Inventory::with_default_root().map_err(|err| err.to_string())?;
    let mut mapping = serde_yaml::Mapping::new();
    let mut written = HashSet::new();

    for tag in &tag_colors.order {
        let Some(color) = tag_colors.colors.get(tag) else {
            continue;
        };
        mapping.insert(
            serde_yaml::Value::String(tag.clone()),
            serde_yaml::Value::String(color.clone()),
        );
        written.insert(tag.clone());
    }

    for (tag, color) in &tag_colors.colors {
        if written.contains(tag) {
            continue;
        }
        mapping.insert(
            serde_yaml::Value::String(tag.clone()),
            serde_yaml::Value::String(color.clone()),
        );
    }

    inventory
        .save(
            "tag_colors",
            InventoryScope::Local,
            &serde_yaml::Value::Mapping(mapping),
        )
        .map_err(|err| err.to_string())
}

fn ensure_tag_colors<'a>(
    tag_colors: &mut TagColors,
    tags: impl IntoIterator<Item = &'a str>,
) -> bool {
    let mut changed = false;
    for tag in tags {
        if tag_colors.colors.contains_key(tag) {
            continue;
        }
        changed |= set_tag_color(tag_colors, tag, next_tag_color(tag_colors), false);
    }
    changed
}

fn set_tag_color(
    tag_colors: &mut TagColors,
    tag: &str,
    color: impl Into<String>,
    no_overwrite_color: bool,
) -> bool {
    if no_overwrite_color && tag_colors.colors.contains_key(tag) {
        return false;
    }

    if !tag_colors.colors.contains_key(tag) {
        tag_colors.order.push(tag.to_string());
    }

    let color = color.into();
    if tag_colors.colors.get(tag) == Some(&color) {
        return false;
    }
    tag_colors.colors.insert(tag.to_string(), color);
    true
}

fn next_tag_color(tag_colors: &TagColors) -> String {
    let last_color = tag_colors
        .order
        .iter()
        .rev()
        .find_map(|tag| tag_colors.colors.get(tag))
        .map(String::as_str)
        .unwrap_or(TAG_COLOR_ORDER[TAG_COLOR_ORDER.len() - 1]);
    let current_index = TAG_COLOR_ORDER
        .iter()
        .position(|color| *color == last_color)
        .unwrap_or(TAG_COLOR_ORDER.len() - 1);
    TAG_COLOR_ORDER[(current_index + 1) % TAG_COLOR_ORDER.len()].to_string()
}

pub fn cmd_freeze(targets: &[String], list: bool, on: bool, off: bool) {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    if list {
        let _ = cmd_list(ListOptions {
            frozen: true,
            ..ListOptions::default()
        });
        return;
    }

    if targets.is_empty() {
        crate::commands::help::display_command_help("freeze");
        return;
    }

    for target in download::tagname_to_ids(targets) {
        let Some(data) = download::get_data_by_target(&target) else {
            eprintln!("{} は存在しません", target);
            continue;
        };
        let id = data.id;

        let result = db::with_database_mut(|db| {
            let mut frozen_list: std::collections::HashMap<i64, serde_yaml::Value> = db
                .inventory()
                .load("freeze", narou_rs::db::inventory::InventoryScope::Local)?;
            let record = db
                .get(id)
                .cloned()
                .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
            let title = record.title.clone();
            let is_frozen = frozen_list.contains_key(&id);

            let mut updated = record;

            let should_freeze = if on {
                true
            } else if off {
                false
            } else {
                !is_frozen
            };

            if should_freeze {
                if !is_frozen {
                    updated.tags.push("frozen".to_string());
                }
                frozen_list.insert(id, serde_yaml::Value::Bool(true));
                db.insert(updated);
                db.inventory().save(
                    "freeze",
                    narou_rs::db::inventory::InventoryScope::Local,
                    &frozen_list,
                )?;
                db.save()?;
                Ok::<(String, bool), narou_rs::error::NarouError>((title, true))
            } else {
                if is_frozen {
                    updated.tags.retain(|t| t != "frozen");
                }
                if updated.tags.contains(&"404".to_string()) {
                    updated.tags.retain(|t| t != "404");
                }
                frozen_list.remove(&id);
                db.insert(updated);
                db.inventory().save(
                    "freeze",
                    narou_rs::db::inventory::InventoryScope::Local,
                    &frozen_list,
                )?;
                db.save()?;
                Ok::<(String, bool), narou_rs::error::NarouError>((title, false))
            }
        });

        match result {
            Ok((title, true)) => println!("{} を凍結しました", title),
            Ok((title, false)) => println!("{} の凍結を解除しました", title),
            Err(e) => eprintln!("  Error: {}", e),
        }
    }
}

pub fn cmd_remove(targets: &[String], yes: bool, with_file: bool, all_ss: bool) -> i32 {
    use narou_rs::db;

    if let Err(e) = db::init_database() {
        eprintln!("Error initializing database: {}", e);
        return 1;
    }

    let mut targets = targets.to_vec();
    if all_ss {
        let short_story_ids = collect_all_short_story_ids();
        if short_story_ids.is_empty() {
            println!("短編小説がひとつもありません");
            return 0;
        }
        targets.extend(short_story_ids);
    }

    if targets.is_empty() {
        help::display_command_help("remove");
        return 0;
    }

    let frozen_ids = load_inventory_ids("freeze");
    let locked_ids = load_inventory_ids("lock");

    for (index, target) in download::tagname_to_ids(&targets).into_iter().enumerate() {
        if index > 0 {
            println!("{}", "―".repeat(35));
        }

        let Some(data) = download::get_data_by_target(&target) else {
            log::report_error(&format!("{} は存在しません", target));
            continue;
        };

        if locked_ids.contains(&data.id) {
            log::report_error(&format!(
                "{} は変換中なため削除出来ませんでした",
                data.title
            ));
            continue;
        }
        if frozen_ids.contains(&data.id) {
            println!("{} は凍結中です\n削除を中止しました", data.title);
            continue;
        }

        if !yes
            && !confirm(
                &build_remove_confirm_message(&data.title, with_file),
                false,
                true,
            )
        {
            continue;
        }

        match remove_novel_by_id(data.id, with_file) {
            Ok(outcome) => {
                if let Some(path) = outcome.removed_path {
                    println!("{} を完全に削除しました", path.display());
                }
                println!("{}", colorize_removed_message(&outcome.title));
            }
            Err(err) => log::report_error(&err),
        }
    }

    0
}

pub fn freeze_by_target(target: &str) {
    use narou_rs::db;

    let Some(data) = download::get_data_by_target(target) else {
        return;
    };
    let id = data.id;

    let result = db::with_database_mut(|db| {
        let mut frozen_list: std::collections::HashMap<i64, serde_yaml::Value> = db
            .inventory()
            .load("freeze", narou_rs::db::inventory::InventoryScope::Local)?;
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        if !updated.tags.contains(&"frozen".to_string()) {
            updated.tags.push("frozen".to_string());
        }
        frozen_list.insert(id, serde_yaml::Value::Bool(true));
        db.insert(updated);
        db.inventory().save(
            "freeze",
            narou_rs::db::inventory::InventoryScope::Local,
            &frozen_list,
        )?;
        db.save()
    });

    match result {
        Ok(()) => println!("  Froze ID: {}", id),
        Err(e) => eprintln!("  Error: {}", e),
    }
}

pub fn remove_by_target(target: &str) {
    let Some(data) = download::get_data_by_target(target) else {
        return;
    };

    match remove_novel_by_id(data.id, false) {
        Ok(outcome) => println!("{}", colorize_removed_message(&outcome.title)),
        Err(err) => log::report_error(&err),
    }
}

struct RemoveOutcome {
    title: String,
    removed_path: Option<PathBuf>,
}

fn remove_novel_by_id(id: i64, with_file: bool) -> Result<RemoveOutcome, String> {
    use narou_rs::db;

    let result = db::with_database_mut(|db| {
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| narou_rs::error::NarouError::NotFound(format!("ID: {}", id)))?;
        let dir = db::existing_novel_dir_for_record(db.archive_root(), &record);
        remove_novel_files(&dir, with_file).map_err(narou_rs::error::NarouError::Conversion)?;
        db.remove(id);
        db.save()?;
        Ok::<RemoveOutcome, narou_rs::error::NarouError>(RemoveOutcome {
            title: record.title,
            removed_path: with_file.then_some(dir),
        })
    });

    result.map_err(|e| e.to_string())
}

fn collect_all_short_story_ids() -> Vec<String> {
    use narou_rs::db;

    db::with_database(|db| {
        let mut ids = db
            .all_records()
            .values()
            .filter(|record| record.novel_type == 2)
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
    })
    .unwrap_or_default()
}

fn load_inventory_ids(name: &str) -> HashSet<i64> {
    use narou_rs::db;

    db::with_database(|db| {
        let values: std::collections::HashMap<i64, serde_yaml::Value> = db
            .inventory()
            .load(name, InventoryScope::Local)
            .unwrap_or_default();
        Ok(values.into_keys().collect::<HashSet<_>>())
    })
    .unwrap_or_default()
}

fn build_remove_confirm_message(title: &str, with_file: bool) -> String {
    if with_file {
        format!("{} を“完全に”削除しますか", title)
    } else {
        format!("{} を削除しますか", title)
    }
}

fn colorize_removed_message(title: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        format!("{} を削除しました", title)
    } else {
        format!("\x1b[1;32m{} を削除しました\x1b[0m", title)
    }
}

fn remove_novel_files(dir: &Path, with_file: bool) -> Result<(), String> {
    if with_file {
        if dir.exists() {
            std::fs::remove_dir_all(dir).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }

    let toc_path = dir.join("toc.yaml");
    if toc_path.exists() {
        std::fs::remove_file(toc_path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    use super::{
        TagColors, TagOptions, build_tag_mode, ensure_tag_colors, matches_filters, matches_grep,
        remove_novel_files,
    };
    use narou_rs::db::novel_record::NovelRecord;

    #[test]
    fn remove_without_with_file_only_deletes_toc() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("novel");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("toc.yaml"), "toc").unwrap();
        std::fs::write(dir.join("section.txt"), "body").unwrap();

        remove_novel_files(&dir, false).unwrap();

        assert!(dir.exists());
        assert!(!dir.join("toc.yaml").exists());
        assert!(dir.join("section.txt").exists());
    }

    #[test]
    fn remove_with_file_deletes_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("novel");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("toc.yaml"), "toc").unwrap();

        remove_novel_files(&dir, true).unwrap();

        assert!(!dir.exists());
    }

    #[test]
    fn build_tag_mode_rejects_banned_word() {
        let err = build_tag_mode(&TagOptions {
            add: Some("hotentry".to_string()),
            ..TagOptions::default()
        })
        .unwrap_err();

        assert_eq!(err, "hotentry は使用禁止ワードです");
    }

    #[test]
    fn ensure_tag_colors_rotates_in_insertion_order() {
        let mut tag_colors = TagColors::default();
        assert!(ensure_tag_colors(&mut tag_colors, ["fav"]));
        assert!(ensure_tag_colors(&mut tag_colors, ["later"]));
        assert!(ensure_tag_colors(&mut tag_colors, ["todo"]));

        assert_eq!(
            tag_colors.colors.get("fav").map(String::as_str),
            Some("green")
        );
        assert_eq!(
            tag_colors.colors.get("later").map(String::as_str),
            Some("yellow")
        );
        assert_eq!(
            tag_colors.colors.get("todo").map(String::as_str),
            Some("blue")
        );
    }

    #[test]
    fn matches_filters_and_grep_follow_ruby_rules() {
        let series = sample_record(1, 1, &["end"]);
        let short_story = sample_record(2, 2, &[]);

        assert!(matches_filters(
            &series,
            true,
            &["series".to_string(), "frozen".to_string()]
        ));
        assert!(matches_filters(
            &short_story,
            false,
            &["ss".to_string(), "nonfrozen".to_string()]
        ));
        assert!(!matches_filters(
            &short_story,
            true,
            &["series".to_string()]
        ));

        assert!(matches_grep(
            "作者名 紫炎 ハーメルン",
            &["紫炎".to_string(), "-なろう".to_string()]
        ));
        assert!(!matches_grep("小説家になろう", &["-なろう".to_string()]));
    }

    fn sample_record(id: i64, novel_type: u8, tags: &[&str]) -> NovelRecord {
        NovelRecord {
            id,
            author: "author".to_string(),
            title: "title".to_string(),
            file_title: "file_title".to_string(),
            toc_url: "https://example.com".to_string(),
            sitename: "site".to_string(),
            novel_type,
            end: false,
            last_update: Utc.with_ymd_and_hms(2026, 4, 14, 0, 0, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }
}
