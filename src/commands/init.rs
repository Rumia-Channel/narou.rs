use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use narou_rs::error::Result;
use narou_rs::setting_info::default_local_setting_value;

pub fn cmd_init(aozora_path: Option<&str>, line_height: Option<f64>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let already_root = find_existing_narou_root(&cwd);
    let root = already_root.clone().unwrap_or(cwd);

    if already_root.is_none() {
        std::fs::create_dir_all(root.join(".narou"))?;
        println!(".narou/ を作成しました");

        let archive_root = root.join("小説データ");
        std::fs::create_dir_all(&archive_root)?;
        println!("小説データ/ を作成しました");

        let user_webnovel_dir = root.join("webnovel");
        std::fs::create_dir_all(&user_webnovel_dir)?;
        let copied = copy_bundled_webnovel_files(&user_webnovel_dir)?;
        if copied == 0 {
            println!("webnovel/ を作成しました");
        } else {
            println!("webnovel/ を作成しました ({} files)", copied);
        }
    } else {
        println!("既に初期化済みです: {}", root.display());
    }

    let created_inventory = ensure_dot_narou_files(&root)?;
    if created_inventory > 0 {
        println!(
            ".narou/ に初期ファイルを作成しました ({} files)",
            created_inventory
        );
    }

    init_aozoraepub3_settings(aozora_path, line_height, already_root.is_some())?;

    if already_root.is_none() {
        println!("初期化が完了しました！");
    }

    Ok(())
}

fn find_existing_narou_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".narou").is_dir() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn ensure_dot_narou_files(root: &Path) -> Result<usize> {
    let dir = root.join(".narou");
    std::fs::create_dir_all(&dir)?;

    let files = [
        ("local_setting.yaml", "--- {}\n"),
        ("database.yaml", "--- {}\n"),
        (
            "database_index.yaml",
            "---\nby_toc_url: {}\nby_title: {}\nmeta: {}\n",
        ),
        ("alias.yaml", "--- {}\n"),
        ("freeze.yaml", "--- {}\n"),
        ("tag_colors.yaml", "--- {}\n"),
        ("latest_convert.yaml", "--- {}\n"),
        ("queue.yaml", "---\njobs: []\ncompleted: []\nfailed: []\n"),
        ("notepad.txt", ""),
    ];

    let mut created = 0usize;
    for (name, content) in files {
        let path = dir.join(name);
        if !path.exists() {
            std::fs::write(path, content)?;
            created += 1;
        }
    }
    if ensure_default_local_settings(&dir.join("local_setting.yaml"))? {
        created += 1;
    }
    Ok(created)
}

fn ensure_default_local_settings(path: &Path) -> Result<bool> {
    const DEFAULT_KEYS: &[&str] = &[
        "convert.dc-subject-exclude-tags",
        "download.interval",
        "download.wait-steps",
        "folder-length-limit",
        "filename-length-limit",
        "user-agent",
    ];

    let mut settings = if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        serde_yaml::from_str::<std::collections::BTreeMap<String, serde_yaml::Value>>(&raw)
            .unwrap_or_default()
    } else {
        std::collections::BTreeMap::new()
    };

    let mut changed = false;
    for key in DEFAULT_KEYS {
        if settings.contains_key(*key) {
            continue;
        }
        if let Some(value) = default_local_setting_value(key) {
            settings.insert((*key).to_string(), value);
            changed = true;
        }
    }

    if changed {
        let content = serde_yaml::to_string(&settings)?;
        std::fs::write(path, content)?;
    }

    Ok(changed)
}

fn copy_bundled_webnovel_files(destination: &Path) -> Result<usize> {
    let source = bundled_webnovel_dir();
    let Some(source) = source else {
        return Ok(0);
    };

    let mut copied = 0usize;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let is_yaml = matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("yaml") | Some("yml")
        );
        if !is_yaml {
            continue;
        }
        let filename = match path.file_name() {
            Some(name) => name,
            None => continue,
        };
        let target = destination.join(filename);
        if !target.exists() {
            std::fs::copy(&path, &target)?;
            copied += 1;
        }
    }
    Ok(copied)
}

fn bundled_webnovel_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("webnovel"));
        }
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("webnovel"));

    candidates.into_iter().find(|path| path.is_dir())
}

fn init_aozoraepub3_settings(
    aozora_path: Option<&str>,
    line_height: Option<f64>,
    force: bool,
) -> Result<()> {
    let global_dir = home_dir().join(".narousetting");
    let global_path = global_dir.join("global_setting.yaml");

    let mut settings = if global_path.exists() {
        let raw = std::fs::read_to_string(&global_path)?;
        serde_yaml::from_str::<std::collections::BTreeMap<String, serde_yaml::Value>>(&raw)
            .unwrap_or_default()
    } else {
        std::collections::BTreeMap::new()
    };

    if !force
        && aozora_path.is_none()
        && line_height.is_none()
        && settings.contains_key("aozoraepub3dir")
    {
        return Ok(());
    }

    println!("AozoraEpub3の設定を行います");
    if !settings.contains_key("aozoraepub3dir") {
        println!("!!!WARNING!!!");
        println!(
            "AozoraEpub3の構成ファイルを書き換えます。narouコマンド用に別途新規インストールしておくことをオススメします"
        );
    }

    let resolved_aozora_path = resolve_init_aozora_path(aozora_path, &settings)?;
    let Some(resolved_aozora_path) = resolved_aozora_path else {
        if aozora_path.is_some() {
            println!("指定されたフォルダにAozoraEpub3がありません。");
        }
        println!("AozoraEpub3 の設定をスキップしました");
        return Ok(());
    };

    let height = match line_height {
        Some(height) => height,
        None if io::stdin().is_terminal() => ask_line_height(&settings)?,
        None => settings
            .get("line-height")
            .and_then(|value| value.as_f64())
            .unwrap_or(1.8),
    };

    settings.insert(
        "aozoraepub3dir".to_string(),
        serde_yaml::Value::String(resolved_aozora_path.clone()),
    );
    settings.insert(
        "line-height".to_string(),
        serde_yaml::to_value(height).unwrap_or(serde_yaml::Value::Null),
    );

    rewrite_aozoraepub3_files(&resolved_aozora_path, height)?;

    let content = serde_yaml::to_string(&settings)?;
    std::fs::create_dir_all(&global_dir)?;
    std::fs::write(global_path, content)?;
    println!("グローバル設定を保存しました");

    Ok(())
}

fn resolve_init_aozora_path(
    aozora_path: Option<&str>,
    settings: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> Result<Option<String>> {
    match aozora_path {
        Some(":keep") => Ok(settings
            .get("aozoraepub3dir")
            .and_then(|value| value.as_str())
            .and_then(validate_aozoraepub3_path)),
        Some(path) => Ok(validate_aozoraepub3_path(path)),
        None if io::stdin().is_terminal() => ask_aozoraepub3_path(settings),
        None => Ok(settings
            .get("aozoraepub3dir")
            .and_then(|value| value.as_str())
            .and_then(validate_aozoraepub3_path)),
    }
}

fn ask_aozoraepub3_path(
    settings: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> Result<Option<String>> {
    let current_path = settings
        .get("aozoraepub3dir")
        .and_then(|value| value.as_str());
    println!();
    println!("AozoraEpub3のあるフォルダを入力して下さい:");
    if let Some(current_path) = current_path {
        println!("(未入力でスキップ、:keep で現在と同じ場所を指定)");
        println!("(現在の場所:{})", current_path);
    } else {
        println!("(未入力でスキップ)");
    }

    loop {
        print!(">");
        io::stdout().flush()?;
        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            return Ok(None);
        }
        let input = input.trim();
        if input.is_empty() {
            return Ok(None);
        }
        if input == ":keep" {
            if let Some(path) = current_path.and_then(validate_aozoraepub3_path) {
                return Ok(Some(path));
            }
        } else if let Some(path) = validate_aozoraepub3_path(input) {
            return Ok(Some(path));
        }
        println!("入力されたフォルダにAozoraEpub3がありません。もう一度入力して下さい:");
    }
}

fn ask_line_height(
    settings: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> Result<f64> {
    let default = settings
        .get("line-height")
        .and_then(|value| value.as_f64())
        .unwrap_or(1.8);

    println!();
    println!("行間の調整を行います。小説の行の高さを設定して下さい(単位 em):");
    println!("1em = 1文字分の高さ");
    println!("行の高さ＝1文字分の高さ＋行間の高さ");
    println!("オススメは 1.8");
    println!("(未入力で {} を採用)", format_line_height(default));

    loop {
        print!(">");
        io::stdout().flush()?;
        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            return Ok(default);
        }
        let input = input.trim();
        if input.is_empty() {
            return Ok(default);
        }
        match input.parse::<f64>() {
            Ok(value) => return Ok(value),
            Err(_) => println!("数値を入力して下さい:"),
        }
    }
}

fn validate_aozoraepub3_path(path: &str) -> Option<String> {
    let normalized = normalize_path_string(path);
    if PathBuf::from(&normalized).join("AozoraEpub3.jar").exists() {
        Some(normalized)
    } else {
        None
    }
}

fn rewrite_aozoraepub3_files(aozora_path: &str, line_height: f64) -> Result<()> {
    let preset_dir = preset_dir()?;
    let aozora_dir = PathBuf::from(aozora_path);

    let custom_chuki_tag = std::fs::read_to_string(preset_dir.join("custom_chuki_tag.txt"))?;
    let chuki_tag_path = aozora_dir.join("chuki_tag.txt");
    let mut chuki_tag = std::fs::read_to_string(&chuki_tag_path)?;
    let embedded_mark = "### Narou.rb embedded custom chuki ###";
    if let (Some(start), Some(end)) = (
        chuki_tag.find(embedded_mark),
        chuki_tag.rfind(embedded_mark),
    ) {
        if start != end {
            let end = end + embedded_mark.len();
            chuki_tag.replace_range(start..end, &custom_chuki_tag);
        } else {
            chuki_tag.push('\n');
            chuki_tag.push_str(&custom_chuki_tag);
        }
    } else {
        chuki_tag.push('\n');
        chuki_tag.push_str(&custom_chuki_tag);
    }
    std::fs::write(&chuki_tag_path, chuki_tag)?;

    std::fs::copy(
        preset_dir.join("AozoraEpub3.ini"),
        aozora_dir.join("AozoraEpub3.ini"),
    )?;

    let vertical_font = std::fs::read_to_string(preset_dir.join("vertical_font.css"))?
        .replace("<%= line_height %>", &format_line_height(line_height));
    let vertical_font_path = aozora_dir
        .join("template")
        .join("OPS")
        .join("css_custom")
        .join("vertical_font.css");
    if let Some(parent) = vertical_font_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(vertical_font_path, vertical_font)?;

    println!("AozoraEpub3 の構成ファイルを書き換えました");
    Ok(())
}

fn preset_dir() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("preset"));
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(manifest_dir.join("preset"));
    candidates.push(manifest_dir.join("sample").join("narou").join("preset"));

    candidates
        .into_iter()
        .find(|path| path.is_dir())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "narou preset directory not found",
            )
            .into()
        })
}

fn format_line_height(line_height: f64) -> String {
    let mut text = line_height.to_string();
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::ensure_default_local_settings;

    #[test]
    fn ensure_default_local_settings_writes_expected_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("local_setting.yaml");

        assert!(ensure_default_local_settings(&path).unwrap());

        let settings: std::collections::BTreeMap<String, serde_yaml::Value> =
            serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            settings.get("user-agent"),
            Some(&serde_yaml::Value::String("auto".to_string()))
        );
        assert_eq!(
            settings.get("download.interval"),
            Some(&serde_yaml::to_value(0.7f64).unwrap())
        );
        assert_eq!(
            settings.get("download.wait-steps"),
            Some(&serde_yaml::Value::Number(serde_yaml::Number::from(0)))
        );
    }

    #[test]
    fn ensure_default_local_settings_preserves_existing_values() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("local_setting.yaml");
        std::fs::write(&path, "---\nuser-agent: custom-agent\n").unwrap();

        assert!(ensure_default_local_settings(&path).unwrap());

        let settings: std::collections::BTreeMap<String, serde_yaml::Value> =
            serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            settings.get("user-agent"),
            Some(&serde_yaml::Value::String("custom-agent".to_string()))
        );
    }
}

fn normalize_path_string(path: &str) -> String {
    let path = path.trim_matches('"');
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| PathBuf::from(path))
        .display()
        .to_string()
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
        })
}
