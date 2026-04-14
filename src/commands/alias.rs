use std::collections::HashMap;

use narou_rs::compat::yaml_value_to_string;
use narou_rs::db;
use narou_rs::db::inventory::{Inventory, InventoryScope};

use super::download;
use super::help;
use super::log;

const BANNED_ALIAS_NAME: &str = "hotentry";

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
    let mut aliases: HashMap<String, serde_yaml::Value> = inventory
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

fn process_alias_arg(arg: &str, aliases: &mut HashMap<String, serde_yaml::Value>) {
    let Some((alias_name, target)) = arg.split_once('=') else {
        log::report_error(&format!(
            "書式が間違っています。{}=別名 のように書いて下さい",
            arg
        ));
        return;
    };

    if alias_name == BANNED_ALIAS_NAME {
        log::report_error(&format!("{} は予約語のため使用出来ません", alias_name));
        return;
    }
    if !is_valid_alias_name(alias_name) {
        log::report_error(&format!(
            "{} は別名に使用出来ません。半角英数字と_が使えます",
            alias_name
        ));
        return;
    }

    if target.is_empty() {
        aliases.remove(alias_name);
        println!("{} を解除しました", alias_name);
        return;
    }

    let Some(data) = download::get_data_by_target(target) else {
        log::report_error(&format!("{} は存在しません", target));
        return;
    };

    aliases.insert(
        alias_name.to_string(),
        serde_yaml::Value::Number(serde_yaml::Number::from(data.id)),
    );
    println!("{} に {} の別名を設定しました", data.title, alias_name);
}

fn display_aliases() -> Result<(), String> {
    let inventory = Inventory::with_default_root().map_err(|e| e.to_string())?;
    let aliases: HashMap<String, serde_yaml::Value> = inventory
        .load("alias", InventoryScope::Local)
        .map_err(|e| e.to_string())?;

    if aliases.is_empty() {
        return Ok(());
    }

    let mut rows = aliases
        .iter()
        .map(|(alias_name, value)| {
            let target = yaml_value_to_string(value).unwrap_or_default();
            let title = resolve_alias_title(&target);
            (alias_name.clone(), title)
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    for (alias_name, title) in rows {
        println!("{}={}", alias_name, title);
    }
    Ok(())
}

fn resolve_alias_title(target: &str) -> String {
    download::get_data_by_target(target)
        .map(|data| data.title)
        .unwrap_or_else(|| target.to_string())
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
