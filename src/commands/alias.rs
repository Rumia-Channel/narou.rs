use narou_rs::compat::yaml_value_to_string;
use narou_rs::db;
use narou_rs::db::inventory::{Inventory, InventoryScope};

use super::download;
use super::help;
use super::log;

const BAN_WORDS: &[&str] = &["hotentry"];

pub fn cmd_alias(args: &[String], list: bool) -> i32 {
    match cmd_alias_inner(args, list) {
        Ok(()) => 0,
        Err(err) => {
            log::report_error(&err);
            1
        }
    }
}

fn cmd_alias_inner(args: &[String], list: bool) -> Result<(), String> {
    db::init_database().map_err(|e| e.to_string())?;

    if list {
        display_aliases()?;
        return Ok(());
    }

    if args.is_empty() {
        help::display_command_help("alias");
        return Ok(());
    }

    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let mut aliases: serde_yaml::Mapping = inventory
        .load("alias", InventoryScope::Local)
        .map_err(|e| e.to_string())?;

    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            println!("{}", "―".repeat(35));
        }
        process_alias_arg(arg, &mut aliases);
    }

    inventory
        .save("alias", InventoryScope::Local, &aliases)
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn process_alias_arg(arg: &str, aliases: &mut serde_yaml::Mapping) {
    let (alias_name, target) = match arg.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (arg, None),
    };

    if BAN_WORDS.contains(&alias_name) {
        log::report_error(&format!("{} は使用禁止ワードです", alias_name));
        return;
    }
    if !is_valid_alias_name(alias_name) {
        log::report_error("別名にはアルファベット・数字・アンダースコアしか使えません");
        return;
    }
    let Some(target) = target else {
        log::report_error(&format!(
            "書式が間違っています。{}=別名 のように書いて下さい",
            alias_name
        ));
        return;
    };

    if target.is_empty() {
        aliases.shift_remove(alias_name);
        println!("{} を解除しました", alias_name);
        return;
    }

    let Some(data) = download::get_data_by_target(target) else {
        log::report_error(&format!("{} は存在しません", target));
        return;
    };

    aliases.insert(
        serde_yaml::Value::String(alias_name.to_string()),
        serde_yaml::Value::Number(serde_yaml::Number::from(data.id)),
    );
    println!("{} を {} の別名に設定しました", alias_name, data.title);
}

fn display_aliases() -> Result<(), String> {
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    // Ruby の Inventory.load が返す Hash は YAML の記述順を保つので、
    // serde_yaml::Mapping (indexmap ベース) で同じく記述順に列挙する。
    let aliases: serde_yaml::Mapping = inventory
        .load("alias", InventoryScope::Local)
        .map_err(|e| e.to_string())?;

    for (name, value) in &aliases {
        let target = yaml_value_to_string(value).unwrap_or_default();
        let title = resolve_alias_title(&target);
        println!(
            "{}={}",
            yaml_value_to_string(name).unwrap_or_default(),
            title
        );
    }
    Ok(())
}

fn resolve_alias_title(target: &str) -> String {
    download::get_data_by_target(target)
        .map(|data| data.title)
        .unwrap_or_else(|| "(すでに削除されています)".to_string())
}

fn is_valid_alias_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::is_valid_alias_name;

    #[test]
    fn alias_name_must_be_ascii_word() {
        assert!(is_valid_alias_name("abc_123"));
        assert!(!is_valid_alias_name(""));
        assert!(!is_valid_alias_name("narou-rs"));
        assert!(!is_valid_alias_name("日本語"));
    }
}
