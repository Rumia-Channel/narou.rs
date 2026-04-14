use std::collections::HashMap;
use std::fs;

use ::csv::{ReaderBuilder, Terminator, WriterBuilder};

use narou_rs::db;
use narou_rs::db::inventory::InventoryScope;

use super::download;
use super::log;

const HR_TEXT: &str = "―――――――――――――――――――――――――――――――";

pub fn cmd_csv(output: Option<&str>, import: Option<&str>) -> i32 {
    match cmd_csv_inner(output, import) {
        Ok(code) => code,
        Err(err) => {
            log::report_error(&err);
            127
        }
    }
}

fn cmd_csv_inner(output: Option<&str>, import: Option<&str>) -> Result<i32, String> {
    db::init_database().map_err(|e| e.to_string())?;

    if let Some(path) = import {
        import_csv(path)?;
        return Ok(0);
    }

    output_csv(output)?;
    Ok(0)
}

fn output_csv(path: Option<&str>) -> Result<(), String> {
    let content = generate_csv()?;
    match path {
        Some(path) => fs::write(path, content).map_err(|e| e.to_string())?,
        None => {
            print!("{}", content);
        }
    }
    Ok(())
}

fn generate_csv() -> Result<String, String> {
    db::with_database(|db| {
        let frozen: HashMap<i64, serde_yaml::Value> =
            db.inventory().load("freeze", InventoryScope::Local)?;
        let mut ids = db.ids();
        ids.sort_unstable();

        let mut writer = WriterBuilder::new()
            .terminator(Terminator::Any(b'\n'))
            .from_writer(Vec::new());
        writer
            .write_record([
                "id",
                "title",
                "author",
                "sitename",
                "url",
                "novel_type",
                "tags",
                "frozen",
                "last_update",
                "general_lastup",
            ])
            .map_err(|e| narou_rs::error::NarouError::Io(std::io::Error::other(e.to_string())))?;

        for id in ids {
            let Some(record) = db.get(id) else { continue };
            let is_frozen =
                frozen.contains_key(&record.id) || record.tags.iter().any(|tag| tag == "frozen");
            let general_lastup = record
                .general_lastup
                .map(|date| date.timestamp().to_string())
                .unwrap_or_else(|| "0".to_string());
            writer
                .write_record([
                    record.id.to_string(),
                    record.title.clone(),
                    record.author.clone(),
                    record.sitename.clone(),
                    record.toc_url.clone(),
                    if record.novel_type == 2 {
                        "短編".to_string()
                    } else {
                        "連載".to_string()
                    },
                    record.tags.join(" "),
                    is_frozen.to_string(),
                    record.last_update.timestamp().to_string(),
                    general_lastup,
                ])
                .map_err(|e| {
                    narou_rs::error::NarouError::Io(std::io::Error::other(e.to_string()))
                })?;
        }

        let bytes = writer
            .into_inner()
            .map_err(|e| narou_rs::error::NarouError::Io(std::io::Error::other(e.to_string())))?;
        Ok::<String, narou_rs::error::NarouError>(String::from_utf8_lossy(&bytes).to_string())
    })
    .map_err(|e| e.to_string())
}

fn import_csv(path: &str) -> Result<(), String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let urls = parse_csv_urls(&content)?;
    for url in urls {
        let _ = download::cmd_download(download::DownloadOptions {
            targets: vec![url],
            force: false,
            no_convert: false,
            freeze: false,
            remove: false,
            mail: false,
            user_agent: None,
        });
        println!("{}", HR_TEXT);
    }
    Ok(())
}

fn parse_csv_urls(content: &str) -> Result<Vec<String>, String> {
    let content = strip_utf8_bom(content);
    let mut reader = ReaderBuilder::new()
        .flexible(true)
        .from_reader(content.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| format!("不正なCSVデータです({})", e))?
        .clone();
    let Some(url_index) = headers.iter().position(|header| header == "url") else {
        return Err("不正なCSVデータです(url ヘッダーがありません)".to_string());
    };

    let mut urls = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|e| format!("不正なCSVデータです({})", e))?;
        if let Some(url) = record
            .get(url_index)
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            urls.push(url.to_string());
        }
    }
    Ok(urls)
}

fn strip_utf8_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

#[cfg(test)]
mod tests {
    use super::parse_csv_urls;

    #[test]
    fn parse_csv_urls_collects_non_empty_url_values() {
        let urls = parse_csv_urls(
            "id,url,title\n1,https://example.com/a,A\n2,,B\n3,https://example.com/c,C\n",
        )
        .unwrap();
        assert_eq!(
            urls,
            vec![
                "https://example.com/a".to_string(),
                "https://example.com/c".to_string()
            ]
        );
    }

    #[test]
    fn parse_csv_urls_requires_url_header() {
        let err = parse_csv_urls("id,title\n1,A\n").unwrap_err();
        assert!(err.contains("url ヘッダー"));
    }
}
