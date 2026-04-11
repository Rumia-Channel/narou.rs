use std::collections::HashMap;

use narou_rs::converter::ini::{IniData, IniValue};
use narou_rs::converter::settings::NovelSettings;
use narou_rs::db::inventory::{Inventory, InventoryScope};
use narou_rs::db::{novel_dir_for_record, with_database};

use super::resolve_target_to_id;

pub fn cmd_setting(args: &[String], list: bool, all: bool, burn: bool) {
    if let Err(e) = cmd_setting_inner(args, list, all, burn) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn cmd_setting_inner(
    args: &[String],
    list: bool,
    all: bool,
    burn: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let inv = Inventory::with_default_root()?;

    if list {
        output_setting_list(&inv);
        return Ok(());
    }

    if burn {
        burn_default_settings(&inv, args)?;
        return Ok(());
    }

    if all {
        display_variable_list();
        return Ok(());
    }

    if args.is_empty() {
        display_help();
        return Ok(());
    }

    let mut local_settings: HashMap<String, serde_yaml::Value> =
        inv.load("local_setting", InventoryScope::Local)?;
    let mut global_settings: HashMap<String, serde_yaml::Value> =
        inv.load("global_setting", InventoryScope::Global)?;

    let mut error_count = 0u32;

    for arg in args {
        let (name, value_str) = split_arg(arg);
        if name.is_empty() {
            eprintln!("書式が間違っています。変数名=値 のように書いて下さい");
            error_count += 1;
            continue;
        }

        let scope = get_scope_of_variable_name(&name);

        if value_str.is_none() {
            if let Some(s) = scope {
                let settings = match s {
                    Scope::Local => &local_settings,
                    Scope::Global => &global_settings,
                };
                match settings.get(&name) {
                    Some(v) => println!("{}", format_yaml_value(v)),
                    None => println!(),
                }
            } else {
                eprintln!("{} という変数は存在しません", name);
                error_count += 1;
            }
            continue;
        }

        let value_str = value_str.unwrap();

        if scope.is_none() {
            if value_str.is_empty() {
                let deleted = sweep_dust_variable(&name, &mut local_settings, &mut global_settings);
                if deleted {
                    println!("{} の設定を削除しました", name);
                } else {
                    eprintln!("{} という変数は存在しません", name);
                    error_count += 1;
                }
            } else {
                eprintln!("{} という変数は設定出来ません", name);
                error_count += 1;
            }
            continue;
        }

        let s = scope.unwrap();
        let settings = match s {
            Scope::Local => &mut local_settings,
            Scope::Global => &mut global_settings,
        };

        if value_str.is_empty() {
            settings.remove(&name);
            println!("{} の設定を削除しました", name);
        } else {
            match cast_value(&name, &value_str) {
                Ok(casted) => {
                    let display = format_yaml_value(&casted);
                    settings.insert(name.clone(), casted);
                    println!("{} を {} に設定しました", name, display);
                }
                Err(msg) => {
                    eprintln!("{}", msg);
                    error_count += 1;
                }
            }
        }
    }

    inv.save("local_setting", InventoryScope::Local, &local_settings)?;
    inv.save("global_setting", InventoryScope::Global, &global_settings)?;

    if error_count > 0 {
        std::process::exit(error_count as i32);
    }

    Ok(())
}

fn split_arg(arg: &str) -> (String, Option<String>) {
    if let Some(idx) = arg.find('=') {
        let name = arg[..idx].trim().to_string();
        let value = arg[idx + 1..].trim().to_string();
        (name, Some(value))
    } else {
        (arg.trim().to_string(), None)
    }
}

#[derive(Debug, Clone, Copy)]
enum Scope {
    Local,
    Global,
}

fn get_scope_of_variable_name(name: &str) -> Option<Scope> {
    let vars = setting_variables();
    if vars.local.iter().any(|(n, _)| *n == name) {
        return Some(Scope::Local);
    }
    if vars.global.iter().any(|(n, _)| *n == name) {
        return Some(Scope::Global);
    }
    if name.starts_with("default.") || name.starts_with("force.") {
        return Some(Scope::Local);
    }
    if name.starts_with("default_args.") {
        return Some(Scope::Local);
    }
    None
}

const ORIGINAL_SETTING_NAMES: &[&str] = &[
    "enable_yokogaki",
    "enable_inspect",
    "enable_convert_num_to_kanji",
    "enable_kanji_num_with_units",
    "kanji_num_with_units_lower_digit_zero",
    "enable_alphabet_force_zenkaku",
    "disable_alphabet_word_to_zenkaku",
    "enable_half_indent_bracket",
    "enable_auto_indent",
    "enable_force_indent",
    "enable_auto_join_in_brackets",
    "enable_auto_join_line",
    "enable_enchant_midashi",
    "enable_author_comments",
    "enable_erase_introduction",
    "enable_erase_postscript",
    "enable_ruby",
    "enable_illust",
    "enable_transform_fraction",
    "enable_transform_date",
    "date_format",
    "enable_convert_horizontal_ellipsis",
    "enable_convert_page_break",
    "to_page_break_threshold",
    "enable_dakuten_font",
    "enable_display_end_of_book",
    "enable_add_date_to_title",
    "title_date_format",
    "title_date_align",
    "title_date_target",
    "enable_ruby_youon_to_big",
    "enable_pack_blank_line",
    "enable_kana_ni_to_kanji_ni",
    "enable_insert_word_separator",
    "enable_insert_char_separator",
    "enable_strip_decoration_tag",
    "enable_add_end_to_title",
    "enable_prolonged_sound_mark_to_dash",
    "cut_old_subtitles",
    "slice_size",
    "author_comment_style",
    "novel_author",
    "novel_title",
    "output_filename",
];

fn cast_value(name: &str, value_str: &str) -> Result<serde_yaml::Value, String> {
    if let Some(info) = setting_variables().get(name) {
        return cast_value_for_type(info.var_type, value_str, info.select_keys.as_deref());
    }

    if let Some(rest) = name
        .strip_prefix("default.")
        .or_else(|| name.strip_prefix("force."))
    {
        if let Some(info) = setting_variables().get(rest) {
            return cast_value_for_type(info.var_type, value_str, info.select_keys.as_deref());
        }
        if ORIGINAL_SETTING_NAMES.contains(&rest) {
            return Ok(serde_yaml::Value::String(value_str.to_string()));
        }
    }

    if name.starts_with("default_args.") {
        return Ok(serde_yaml::Value::String(value_str.to_string()));
    }

    Err(format!("{} は不明な名前です", name))
}

fn cast_value_for_type(
    var_type: VarType,
    value_str: &str,
    select_keys: Option<&[String]>,
) -> Result<serde_yaml::Value, String> {
    match var_type {
        VarType::Select => {
            if let Some(keys) = select_keys {
                if !keys.iter().any(|k| k == value_str) {
                    return Err(format!(
                        "不明な値です。{} の中から指定して下さい",
                        keys.join(", ")
                    ));
                }
            }
            Ok(serde_yaml::Value::String(value_str.to_string()))
        }
        VarType::Multiple => {
            if let Some(keys) = select_keys {
                for part in value_str.split(',') {
                    if !keys.iter().any(|k| k == part.trim()) {
                        return Err(format!(
                            "不明な値です。{} の中から指定して下さい",
                            keys.join(", ")
                        ));
                    }
                }
            }
            Ok(serde_yaml::Value::String(value_str.to_string()))
        }
        VarType::Boolean => {
            let lower = value_str.to_lowercase();
            match lower.as_str() {
                "true" | "yes" | "on" => Ok(serde_yaml::Value::Bool(true)),
                "false" | "no" | "off" => Ok(serde_yaml::Value::Bool(false)),
                _ => Err(format!(
                    "{} は真偽値ではありません。true または false を指定して下さい",
                    value_str
                )),
            }
        }
        VarType::Integer => value_str
            .parse::<i64>()
            .map(|i| serde_yaml::Value::Number(i.into()))
            .map_err(|_| format!("{} は整数ではありません", value_str)),
        VarType::Float => value_str
            .parse::<f64>()
            .map(|f| {
                serde_yaml::Value::Number(if f == f as i64 as f64 {
                    (f as i64).into()
                } else {
                    serde_yaml::Number::from(f)
                })
            })
            .map_err(|_| format!("{} は小数ではありません", value_str)),
        VarType::String | VarType::Directory => {
            Ok(serde_yaml::Value::String(value_str.to_string()))
        }
    }
}

fn output_setting_list(inv: &Inventory) {
    let local_settings: HashMap<String, serde_yaml::Value> = inv
        .load("local_setting", InventoryScope::Local)
        .unwrap_or_default();
    let global_settings: HashMap<String, serde_yaml::Value> = inv
        .load("global_setting", InventoryScope::Global)
        .unwrap_or_default();

    println!("[Local Variables]");
    let mut local_sorted: Vec<_> = local_settings.iter().collect();
    local_sorted.sort_by_key(|(k, _)| *k);
    for (name, value) in &local_sorted {
        let display = format_yaml_value(value);
        if display.contains(' ') {
            println!("{}='{}'", name, display);
        } else {
            println!("{}={}", name, display);
        }
    }

    println!("[Global Variables]");
    let mut global_sorted: Vec<_> = global_settings.iter().collect();
    global_sorted.sort_by_key(|(k, _)| *k);
    for (name, value) in &global_sorted {
        let display = format_yaml_value(value);
        if display.contains(' ') {
            println!("{}='{}'", name, display);
        } else {
            println!("{}={}", name, display);
        }
    }
}

fn sweep_dust_variable(
    name: &str,
    local: &mut HashMap<String, serde_yaml::Value>,
    global: &mut HashMap<String, serde_yaml::Value>,
) -> bool {
    let mut deleted = false;
    if local.remove(name).is_some() {
        deleted = true;
    }
    if global.remove(name).is_some() {
        deleted = true;
    }
    deleted
}

fn display_variable_list() {
    let vars = setting_variables();

    println!("Local Variable List:");
    for (name, info) in &vars.local {
        if !info.invisible {
            let type_desc = var_type_description(info.var_type);
            println!("    {:32} {} {}", name, type_desc, info.help);
        }
    }

    println!();
    println!("Global Variable List:");
    for (name, info) in &vars.global {
        if !info.invisible {
            let type_desc = var_type_description(info.var_type);
            println!("    {:32} {} {}", name, type_desc, info.help);
        }
    }

    println!();
    println!("default.* 系設定 (setting.ini 未設定時のデフォルト値):");
    for setting_name in ORIGINAL_SETTING_NAMES {
        println!("    default.{}", setting_name);
    }

    println!();
    println!("force.* 系設定 (全小説に強制適用):");
    for setting_name in ORIGINAL_SETTING_NAMES {
        println!("    force.{}", setting_name);
    }

    println!();
    println!("default_args.* 系設定 (各コマンドのデフォルトオプション):");
    for cmd in &[
        "init", "download", "update", "convert", "list", "tag", "freeze", "remove", "setting",
        "web", "send", "diff", "mail", "backup", "clean", "csv", "inspect", "log", "folder",
        "browser", "alias", "version", "help",
    ] {
        println!("    default_args.{}", cmd);
    }
}

fn display_help() {
    let vars = setting_variables();

    println!("各コマンドの設定の変更が出来ます。");
    println!("Global な設定はユーザープロファイルに保存され、すべての narou コマンドで使われます");
    println!("下の一覧は一部です。すべてを確認するには -a オプションを付けて確認して下さい");
    println!("default. で始まる設定は、setting.ini で未設定時の項目の挙動を指定することが出来ます");
    println!(
        "force. で始まる設定は、setting.ini や default.* 等の指定を全て無視して項目の挙動を強制出来ます"
    );
    println!();
    println!("Local Variable List:");
    println!("    {:32} {:12} 説明", "名前", "値の型");
    for (name, info) in &vars.local {
        if !info.invisible {
            let type_desc = var_type_description(info.var_type);
            println!("    {:32} {:12} {}", name, type_desc, info.help);
        }
    }

    println!();
    println!("Global Variable List:");
    for (name, info) in &vars.global {
        if !info.invisible {
            let type_desc = var_type_description(info.var_type);
            println!("    {:32} {:12} {}", name, type_desc, info.help);
        }
    }

    println!();
    println!("Examples:");
    println!("  narou setting --list                 # 現在の設置値一覧を表示");
    println!("  narou setting convert.no-open=true   # 値を設定する");
    println!("  narou setting convert.no-epub=        # 右辺に何も書かないとその設定を削除出来る");
    println!("  narou setting device                 # 変数名だけ書くと現在の値を確認出来る");
}

fn burn_default_settings(
    inv: &Inventory,
    targets: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    if targets.is_empty() {
        eprintln!("対象小説を指定して下さい");
        std::process::exit(1);
    }

    let default_settings = load_settings_by_pattern(inv, "default");
    let original_settings = get_original_settings();
    let archive_root = inv.root_dir().join("小説データ");

    for target in targets {
        let id = match resolve_target_to_id(target) {
            Some(id) => id,
            None => {
                eprintln!("{} は存在しません", target);
                continue;
            }
        };

        let record = match with_database(|db| Ok(db.get(id).cloned())) {
            Ok(Some(r)) => r,
            _ => {
                eprintln!("{} は存在しません", target);
                continue;
            }
        };

        let archive_path = novel_dir_for_record(&archive_root, &record);

        let ini_path = archive_path.join("setting.ini");
        let mut ini = IniData::load_file(&ini_path).unwrap_or_else(|_| IniData::new());

        for (name, default_value) in &original_settings {
            if ini.get_global(name).is_none() {
                if let Some(default_val) = default_settings.get(name) {
                    ini.set_global(name, yaml_to_ini_value(default_val));
                } else {
                    ini.set_global(name, default_value.clone());
                }
            }
        }

        ini.save(&ini_path)?;
        println!("{} の設定を保存しました", record.title);
    }

    Ok(())
}

fn load_settings_by_pattern(inv: &Inventory, pattern: &str) -> HashMap<String, serde_yaml::Value> {
    let local: HashMap<String, serde_yaml::Value> = inv
        .load("local_setting", InventoryScope::Local)
        .unwrap_or_default();

    let prefix = format!("{}.", pattern);
    let mut result = HashMap::new();
    for (name, value) in &local {
        if let Some(rest) = name.strip_prefix(&prefix) {
            result.insert(rest.to_string(), value.clone());
        }
    }
    result
}

fn get_original_settings() -> Vec<(String, IniValue)> {
    let d = NovelSettings::default();
    vec![
        (
            "enable_yokogaki".into(),
            IniValue::Boolean(d.enable_yokogaki),
        ),
        ("enable_inspect".into(), IniValue::Boolean(d.enable_inspect)),
        (
            "enable_convert_num_to_kanji".into(),
            IniValue::Boolean(d.enable_convert_num_to_kanji),
        ),
        (
            "enable_kanji_num_with_units".into(),
            IniValue::Boolean(d.enable_kanji_num_with_units),
        ),
        (
            "kanji_num_with_units_lower_digit_zero".into(),
            IniValue::Integer(d.kanji_num_with_units_lower_digit_zero),
        ),
        (
            "enable_alphabet_force_zenkaku".into(),
            IniValue::Boolean(d.enable_alphabet_force_zenkaku),
        ),
        (
            "disable_alphabet_word_to_zenkaku".into(),
            IniValue::Boolean(d.disable_alphabet_word_to_zenkaku),
        ),
        (
            "enable_half_indent_bracket".into(),
            IniValue::Boolean(d.enable_half_indent_bracket),
        ),
        (
            "enable_auto_indent".into(),
            IniValue::Boolean(d.enable_auto_indent),
        ),
        (
            "enable_force_indent".into(),
            IniValue::Boolean(d.enable_force_indent),
        ),
        (
            "enable_auto_join_in_brackets".into(),
            IniValue::Boolean(d.enable_auto_join_in_brackets),
        ),
        (
            "enable_auto_join_line".into(),
            IniValue::Boolean(d.enable_auto_join_line),
        ),
        (
            "enable_enchant_midashi".into(),
            IniValue::Boolean(d.enable_enchant_midashi),
        ),
        (
            "enable_author_comments".into(),
            IniValue::Boolean(d.enable_author_comments),
        ),
        (
            "enable_erase_introduction".into(),
            IniValue::Boolean(d.enable_erase_introduction),
        ),
        (
            "enable_erase_postscript".into(),
            IniValue::Boolean(d.enable_erase_postscript),
        ),
        ("enable_ruby".into(), IniValue::Boolean(d.enable_ruby)),
        ("enable_illust".into(), IniValue::Boolean(d.enable_illust)),
        (
            "enable_transform_fraction".into(),
            IniValue::Boolean(d.enable_transform_fraction),
        ),
        (
            "enable_transform_date".into(),
            IniValue::Boolean(d.enable_transform_date),
        ),
        ("date_format".into(), IniValue::String(d.date_format)),
        (
            "enable_convert_horizontal_ellipsis".into(),
            IniValue::Boolean(d.enable_convert_horizontal_ellipsis),
        ),
        (
            "enable_convert_page_break".into(),
            IniValue::Boolean(d.enable_convert_page_break),
        ),
        (
            "to_page_break_threshold".into(),
            IniValue::Integer(d.to_page_break_threshold),
        ),
        (
            "enable_dakuten_font".into(),
            IniValue::Boolean(d.enable_dakuten_font),
        ),
        (
            "enable_display_end_of_book".into(),
            IniValue::Boolean(d.enable_display_end_of_book),
        ),
        (
            "enable_add_date_to_title".into(),
            IniValue::Boolean(d.enable_add_date_to_title),
        ),
        (
            "title_date_format".into(),
            IniValue::String(d.title_date_format),
        ),
        (
            "title_date_align".into(),
            IniValue::String(d.title_date_align),
        ),
        (
            "title_date_target".into(),
            IniValue::String(d.title_date_target),
        ),
        (
            "enable_ruby_youon_to_big".into(),
            IniValue::Boolean(d.enable_ruby_youon_to_big),
        ),
        (
            "enable_pack_blank_line".into(),
            IniValue::Boolean(d.enable_pack_blank_line),
        ),
        (
            "enable_kana_ni_to_kanji_ni".into(),
            IniValue::Boolean(d.enable_kana_ni_to_kanji_ni),
        ),
        (
            "enable_insert_word_separator".into(),
            IniValue::Boolean(d.enable_insert_word_separator),
        ),
        (
            "enable_insert_char_separator".into(),
            IniValue::Boolean(d.enable_insert_char_separator),
        ),
        (
            "enable_strip_decoration_tag".into(),
            IniValue::Boolean(d.enable_strip_decoration_tag),
        ),
        (
            "enable_add_end_to_title".into(),
            IniValue::Boolean(d.enable_add_end_to_title),
        ),
        (
            "enable_prolonged_sound_mark_to_dash".into(),
            IniValue::Boolean(d.enable_prolonged_sound_mark_to_dash),
        ),
        (
            "cut_old_subtitles".into(),
            IniValue::Integer(d.cut_old_subtitles),
        ),
        ("slice_size".into(), IniValue::Integer(d.slice_size)),
        (
            "author_comment_style".into(),
            IniValue::String(d.author_comment_style),
        ),
        ("novel_author".into(), IniValue::String(d.novel_author)),
        ("novel_title".into(), IniValue::String(d.novel_title)),
        (
            "output_filename".into(),
            IniValue::String(d.output_filename),
        ),
    ]
}

fn yaml_to_ini_value(v: &serde_yaml::Value) -> IniValue {
    match v {
        serde_yaml::Value::Bool(b) => IniValue::Boolean(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                IniValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                IniValue::Float(f)
            } else {
                IniValue::Null
            }
        }
        serde_yaml::Value::String(s) => IniValue::String(s.clone()),
        _ => IniValue::Null,
    }
}

fn format_yaml_value(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Null => String::new(),
        other => format!("{:?}", other),
    }
}

#[derive(Debug, Clone, Copy)]
enum VarType {
    Boolean,
    Integer,
    Float,
    String,
    Select,
    Multiple,
    Directory,
}

#[derive(Debug, Clone)]
struct VarInfo {
    var_type: VarType,
    help: &'static str,
    invisible: bool,
    select_keys: Option<Vec<String>>,
}

struct SettingVariables {
    local: Vec<(&'static str, VarInfo)>,
    global: Vec<(&'static str, VarInfo)>,
}

impl SettingVariables {
    fn get(&self, name: &str) -> Option<&VarInfo> {
        for (n, info) in &self.local {
            if *n == name {
                return Some(info);
            }
        }
        for (n, info) in &self.global {
            if *n == name {
                return Some(info);
            }
        }
        None
    }
}

fn setting_variables() -> SettingVariables {
    let vis = |vt: VarType, help: &'static str| VarInfo {
        var_type: vt,
        help,
        invisible: false,
        select_keys: None,
    };
    let invis = |vt: VarType, help: &'static str| VarInfo {
        var_type: vt,
        help,
        invisible: true,
        select_keys: None,
    };
    let sel = |help: &'static str, keys: Vec<&'static str>| VarInfo {
        var_type: VarType::Select,
        help,
        invisible: false,
        select_keys: Some(keys.iter().map(|s| s.to_string()).collect()),
    };
    let multi = |help: &'static str, keys: Vec<&'static str>| VarInfo {
        var_type: VarType::Multiple,
        help,
        invisible: false,
        select_keys: Some(keys.iter().map(|s| s.to_string()).collect()),
    };

    let local_vars = vec![
        (
            "device",
            sel("変換、送信対象の端末", vec!["Kindle", "Kobo", "PocketBook"]),
        ),
        (
            "hotentry",
            vis(VarType::Boolean, "新着投稿だけをまとめたデータを作る"),
        ),
        (
            "hotentry.auto-mail",
            vis(VarType::Boolean, "hotentryをメールで送る"),
        ),
        (
            "concurrency",
            vis(VarType::Boolean, "ダウンロードと変換の同時実行を有効にする"),
        ),
        (
            "concurrency.format-queue-text",
            invis(
                VarType::String,
                "同時実行時の変換キュー表示テキストのフォーマット",
            ),
        ),
        (
            "concurrency.format-queue-style",
            vis(
                VarType::String,
                "同時実行時の変換キュー表示スタイルのフォーマット",
            ),
        ),
        ("logging", vis(VarType::Boolean, "ログの保存を有効にする")),
        (
            "logging.format-filename",
            vis(VarType::String, "ログファイル名のフォーマット"),
        ),
        (
            "logging.format-timestamp",
            vis(VarType::String, "ログ内のタイムスタンプのフォーマット"),
        ),
        (
            "update.interval",
            vis(VarType::Float, "更新時に各作品間で指定した秒数待機する"),
        ),
        (
            "update.strong",
            vis(
                VarType::Boolean,
                "改稿日当日の連続更新でも更新漏れが起きないようにする",
            ),
        ),
        (
            "update.convert-only-new-arrival",
            vis(VarType::Boolean, "更新時に新着がある場合のみ変換を実行する"),
        ),
        (
            "update.sort-by",
            sel(
                "アップデートを指定した項目順で行う",
                vec!["id", "title", "author", "general_lastup"],
            ),
        ),
        (
            "update.auto-schedule.enable",
            vis(VarType::Boolean, "自動アップデート機能を有効にする"),
        ),
        (
            "update.auto-schedule",
            vis(VarType::String, "自動アップデートする時間を指定する"),
        ),
        (
            "convert.copy-to",
            vis(VarType::Directory, "変換したらこのフォルダにコピーする"),
        ),
        (
            "convert.copy-zip-to",
            vis(
                VarType::Directory,
                "生成したZIPファイルをこのフォルダにコピーする",
            ),
        ),
        (
            "convert.copy-to-grouping",
            multi(
                "copy-toで指定したフォルダの中で更に指定の各種フォルダにまとめる",
                vec!["device", "site"],
            ),
        ),
        (
            "convert.copy_to",
            invis(VarType::Directory, "copy-toの昔の書き方(非推奨)"),
        ),
        (
            "convert.no-epub",
            invis(VarType::Boolean, "EPUB変換を無効にする"),
        ),
        (
            "convert.no-mobi",
            invis(VarType::Boolean, "MOBI変換を無効にする"),
        ),
        (
            "convert.no-strip",
            invis(VarType::Boolean, "MOBIのstripを無効にする"),
        ),
        (
            "convert.no-zip",
            invis(VarType::Boolean, "i文庫用のzipファイル作成を無効にする"),
        ),
        (
            "convert.make-zip",
            vis(VarType::Boolean, "ZIPファイルの作成を有効にする"),
        ),
        (
            "convert.no-open",
            vis(VarType::Boolean, "変換時に保存フォルダを開かないようにする"),
        ),
        (
            "convert.inspect",
            vis(VarType::Boolean, "常に変換時に調査結果を表示する"),
        ),
        (
            "convert.multi-device",
            multi(
                "複数の端末用に同時に変換する",
                vec!["Kindle", "Kobo", "PocketBook"],
            ),
        ),
        (
            "convert.filename-to-ncode",
            vis(VarType::Boolean, "書籍ファイル名をNコードで出力する"),
        ),
        (
            "convert.add-dc-subject-to-epub",
            vis(VarType::Boolean, "EPUB変換時にdc:subject要素を追加する"),
        ),
        (
            "convert.dc-subject-exclude-tags",
            vis(
                VarType::String,
                "dc:subjectから除外するタグをカンマ区切りで指定する",
            ),
        ),
        (
            "download.interval",
            vis(VarType::Float, "各話DL時に指定秒数待機する"),
        ),
        (
            "download.wait-steps",
            vis(VarType::Integer, "指定した話数ごとに長めのウェイトが入る"),
        ),
        (
            "download.use-subdirectory",
            vis(
                VarType::Boolean,
                "小説を一定数ごとにサブフォルダへ分けて保存する",
            ),
        ),
        (
            "download.choices-of-digest-options",
            vis(
                VarType::String,
                "ダイジェスト化選択肢が出た場合に自動で項目を選択する",
            ),
        ),
        (
            "send.without-freeze",
            vis(VarType::Boolean, "送信時に凍結された小説は対象外にする"),
        ),
        (
            "send.backup-bookmark",
            vis(
                VarType::Boolean,
                "一括送信時に栞データを自動でバックアップする",
            ),
        ),
        (
            "multiple-delimiter",
            vis(VarType::String, "--multiple指定時の区切り文字"),
        ),
        (
            "economy",
            multi(
                "容量節約に関する設定",
                vec!["cleanup_temp", "send_delete", "nosave_diff", "nosave_raw"],
            ),
        ),
        ("guard-spoiler", vis(VarType::Boolean, "ネタバレ防止機能")),
        (
            "auto-add-tags",
            vis(
                VarType::Boolean,
                "サイトから取得したタグを自動的に小説データに追加する",
            ),
        ),
        (
            "normalize-filename",
            vis(VarType::Boolean, "ファイル名の文字列をNFCで正規化する"),
        ),
        (
            "folder-length-limit",
            vis(VarType::Integer, "小説を格納するフォルダ名の長さを制限する"),
        ),
        (
            "filename-length-limit",
            vis(VarType::Integer, "各話保存時のファイル名の長さを制限する"),
        ),
        (
            "ebook-filename-length-limit",
            vis(
                VarType::Integer,
                "出力される電子書籍ファイル名の長さを制限する",
            ),
        ),
        ("user-agent", vis(VarType::String, "User-Agent 設定")),
        ("webui.theme", invis(VarType::Select, "WEB UI 用テーマ選択")),
        (
            "webui.table.reload-timing",
            invis(VarType::Select, "小説リストの更新タイミングを選択"),
        ),
        (
            "webui.performance-mode",
            sel("パフォーマンスモードを設定", vec!["auto", "on", "off"]),
        ),
    ];

    let global_vars = vec![
        (
            "aozoraepub3dir",
            invis(VarType::Directory, "AozoraEpub3のあるフォルダを指定"),
        ),
        ("line-height", invis(VarType::Float, "行間サイズ")),
        (
            "difftool",
            vis(VarType::String, "diffで使うツールのパスを指定する"),
        ),
        (
            "difftool.arg",
            vis(VarType::String, "difftoolで使う引数を設定"),
        ),
        ("no-color", vis(VarType::Boolean, "カラー表示を無効にする")),
        (
            "color-parser",
            sel(
                "コンソール上でのANSIカラーを表示する方法の選択",
                vec!["system", "self"],
            ),
        ),
        (
            "server-port",
            vis(VarType::Integer, "WEBサーバ起動時のポート"),
        ),
        (
            "server-bind",
            invis(VarType::String, "WEBサーバのホスト制限"),
        ),
        (
            "server-basic-auth.enable",
            invis(VarType::Boolean, "WEBサーバでBasic認証を使用するかどうか"),
        ),
        (
            "server-basic-auth.user",
            invis(VarType::String, "WEBサーバでBasic認証をするユーザ名"),
        ),
        (
            "server-basic-auth.password",
            invis(VarType::String, "WEBサーバのBasic認証のパスワード"),
        ),
        (
            "server-ws-add-accepted-domains",
            invis(
                VarType::String,
                "PushServer の accepted_domains に追加するホストのリスト",
            ),
        ),
        ("over18", invis(VarType::Boolean, "18歳以上かどうか")),
    ];

    SettingVariables {
        local: local_vars,
        global: global_vars,
    }
}

fn var_type_description(vt: VarType) -> &'static str {
    match vt {
        VarType::Boolean => "真偽値",
        VarType::Integer => "整数",
        VarType::Float => "小数",
        VarType::String => "文字列",
        VarType::Directory => "フォルダパス",
        VarType::Select => "選択",
        VarType::Multiple => "複数選択",
    }
}
