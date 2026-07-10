use std::collections::HashMap;

use narou_rs::compat::confirm;
use narou_rs::converter::ini::{IniData, IniValue};
use narou_rs::converter::settings::NovelSettings;
use narou_rs::db::inventory::{Inventory, InventoryScope};
use narou_rs::db::{novel_dir_for_record, with_database};
use narou_rs::setting_core::{
    SettingScope as Scope, apply_device_related_settings, cast_setting_value, setting_scope,
    var_type_description, yaml_value_display,
};
use narou_rs::setting_info::{self, SettingVariables, VarInfo, VarType, default_arg_command_names};

use super::download::{get_data_by_target, tagname_to_ids};

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
        display_variable_list(true);
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
                    if name == "device" && matches!(s, Scope::Local) {
                        modify_settings_when_device_changed(settings);
                    }
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

fn get_scope_of_variable_name(name: &str) -> Option<Scope> {
    setting_scope(name)
}

fn cast_value(name: &str, value_str: &str) -> Result<serde_yaml::Value, String> {
    cast_setting_value(name, value_str)
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

fn print_variable_entry(name: &str, info: &VarInfo, newline_help: bool) {
    let type_desc = var_type_description(info.var_type);
    if newline_help {
        println!("    {:32} {}", name, type_desc);
        println!("      {}", info.help);
    } else {
        println!("    {:32} {} {}", name, type_desc, info.help);
    }
}

fn display_variable_list(show_all: bool) {
    let vars = setting_variables();

    println!("Local Variable List:");
    for (name, info) in &vars.local {
        if show_all || !info.invisible {
            print_variable_entry(name, info, false);
        }
    }
    if show_all {
        for prefix in ["default", "force"] {
            for (name, info) in original_setting_var_infos() {
                print_variable_entry(&format!("{}.{}", prefix, name), &info, true);
            }
        }
        for cmd in default_arg_command_names() {
            println!(
                "    {:32} {} {} コマンドのデフォルトオプション",
                format!("default_args.{}", cmd),
                var_type_description(VarType::String),
                cmd
            );
        }
    }

    println!();
    println!("Global Variable List:");
    for (name, info) in &vars.global {
        if show_all || !info.invisible {
            print_variable_entry(name, info, false);
        }
    }
}

pub(crate) fn display_help() {
    let vars = setting_variables();

    println!("  Usage: narou setting [<name>=<value> ...] [options]");
    println!("  Usage: narou setting --burn <target> [<target2> ...]");
    println!();
    println!("各コマンドの設定の変更が出来ます。");
    println!("Global な設定はユーザープロファイルに保存され、すべての narou コマンドで使われます");
    println!("下の一覧は一部です。すべてを確認するには -a オプションを付けて確認して下さい");
    println!("default. で始まる設定は、setting.ini で未設定時の項目の挙動を指定することが出来ます");
    println!(
        "force. で始まる設定は、setting.ini や default.* 等の指定を全て無視して項目の挙動を強制出来ます"
    );
    println!(
        "default_args. で始まる設定は、各コマンドのデフォルトオプションを指定することが出来ます"
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
    println!("これ以外にも設定出来る項目があります。確認する場合は");
    println!("narou setting -a コマンドを参照して下さい");
    println!();
    println!("Examples:");
    println!("  narou setting --list                 # 現在の設置値一覧を表示");
    println!("  narou setting convert.no-open=true   # 値を設定する");
    println!("  narou setting convert.no-epub=        # 右辺に何も書かないとその設定を削除出来る");
    println!("  narou setting device                 # 変数名だけ書くと現在の値を確認出来る");
    println!("  narou s convert.copy-to=C:/dropbox/mobi");
    println!("  narou s convert.copy-to=\"C:\\Documents and Settings\\user\\epub\"");
    println!();
    println!("Options:");
    println!("    -l, --list                    現在の設定値を確認する");
    println!("    -a, --all                     設定できる全ての変数名を表示する");
    println!("        --burn                    指定した小説の未設定項目に共通設定を焼き付ける");
}

fn burn_default_settings(
    inv: &Inventory,
    targets: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    if targets.is_empty() {
        eprintln!("対象小説を指定して下さい");
        std::process::exit(127);
    }

    let msg = "指定された小説のsetting.iniの未項目設定に共通設定を焼き付けます。\n\
(共通設定とはsetting.iniの項目が未設定時に使用される default.* 系設定およびNarou.rbオリジナル設定のこと)\n\
よろしいですか";
    if !confirm(msg, false, true) {
        return Ok(());
    }

    let targets = tagname_to_ids(targets);
    let default_settings = load_settings_by_pattern(inv, "default");
    let original_settings = get_original_settings();
    let archive_root = inv.root_dir().join("小説データ");

    for target in targets {
        let data = match get_data_by_target(&target) {
            Some(data) => data,
            None => {
                eprintln!("{} は存在しません", target);
                continue;
            }
        };
        let record = match with_database(|db| Ok(db.get(data.id).cloned())) {
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
        println!("{} の設定を保存しました", data.title);
    }

    Ok(())
}

fn modify_settings_when_device_changed(settings: &mut HashMap<String, serde_yaml::Value>) {
    let Some(changed) = apply_device_related_settings(settings) else {
        return;
    };
    if changed.changes.is_empty() {
        return;
    }

    println!(
        "端末を{}に指定したことで、以下の関連設定が変更されました",
        changed.device_display_name
    );
    for (name, value) in changed.changes {
        println!(
            "  → {} が {} に変更されました",
            name,
            format_yaml_value(&value)
        );
    }
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
            "enable_strip_title_prefix".into(),
            IniValue::Boolean(d.enable_strip_title_prefix),
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
    yaml_value_display(v)
}

fn original_setting_var_infos() -> Vec<(&'static str, VarInfo)> {
    setting_info::original_setting_var_infos()
}

fn setting_variables() -> SettingVariables {
    setting_info::setting_variables()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_dynamic_setting_names_are_validated() {
        assert!(matches!(
            get_scope_of_variable_name("default.enable_auto_indent"),
            Some(Scope::Local)
        ));
        assert!(matches!(
            get_scope_of_variable_name("force.enable_auto_indent"),
            Some(Scope::Local)
        ));
        assert!(matches!(
            get_scope_of_variable_name("default_args.trace"),
            Some(Scope::Local)
        ));
        assert!(matches!(
            get_scope_of_variable_name("default_args.console"),
            Some(Scope::Local)
        ));

        assert!(get_scope_of_variable_name("default.not_exists").is_none());
        assert!(get_scope_of_variable_name("force.not_exists").is_none());
        assert!(get_scope_of_variable_name("default_args.not_exists").is_none());
    }

    #[test]
    fn unknown_dynamic_values_are_rejected() {
        assert!(cast_value("default_args.not_exists", "-n").is_err());
        assert!(cast_value("default.not_exists", "true").is_err());
        assert!(cast_value("webui.table.reload-timing", "invalid").is_err());
        assert!(cast_value("webui.table.reload-timing", "every").is_ok());
        assert!(cast_value("webui.theme", "unknown").is_err());
        assert!(cast_value("webui.theme", "Cerulean").is_ok());
        assert!(cast_value("webui.new-tag-color", "purple").is_err());
        assert!(cast_value("webui.new-tag-color", "default").is_ok());
        assert!(cast_value("webui.new-tag-color", "white").is_ok());
        assert!(cast_value("default.title_date_align", "middle").is_err());
        assert!(cast_value("default.title_date_align", "left").is_ok());
    }
}
