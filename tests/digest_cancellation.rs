#![cfg(feature = "native-runtime")]

//! Exercise the real CLI and the Web UI's subprocess mode against a local HTTP
//! proxy. No requests reach the public fixture address, and TempDir owns every
//! library so failures do not leave test libraries behind.
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

const TOC_URL: &str = "http://8.8.8.8/novel/n1234ab/";
const SITE: &str = r#"
name: Digest fixture
domain: 8.8.8.8
sitename: 8.8.8.8
top_url: http://8.8.8.8
url: http://8\.8\.8\.8/novel/(?<ncode>n\d+[a-z]+)/?
toc_url: http://8.8.8.8/novel/\k<ncode>/
encoding: UTF-8
title: '<h1>(?<title>.+?)</h1>'
author: '<h2>(?<author>.+?)</h2>'
subtitles: '<a href="(?<href>/novel/[^/]+/(?<index>\d+)/)">(?<subtitle>[^<]+)</a>'
body_pattern: '<p>(?<body>.+?)</p>'
version: 1.0
"#;

struct Proxy {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Proxy {
    fn start() -> Self {
        Self::with_toc("<h1>Digest Novel</h1><h2>Test Author</h2><a href=\"/novel/n1234ab/1/\">第一話</a>")
    }

    fn with_toc(toc: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let thread = std::thread::spawn(move || {
            for connection in listener.incoming() {
                let mut stream = connection.unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                if request.is_empty() { break; } // Drop wakes the accept loop.
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" { break; }
                }
                seen.lock().unwrap().push(request.clone());
                let body = if request.contains("/novel/n1234ab/1/") {
                    "<p>更新後の本文</p>"
                } else {
                    toc
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self { address, requests, thread: Some(thread) }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        drop(TcpStream::connect(self.address));
        if let Some(thread) = self.thread.take() { thread.join().unwrap(); }
    }
}

fn run(root: &Path, proxy: &Proxy, sqlite: bool, web: bool, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_narou_rs"));
    command.args(args).current_dir(root).stdin(Stdio::null())
        .env("HOME", root.join("home")).env("USERPROFILE", root.join("home"))
        .env("http_proxy", format!("http://{}", proxy.address))
        .env("ALL_PROXY", format!("http://{}", proxy.address))
        .env("NO_PROXY", "").env("no_proxy", "")
        .env_remove("NAROU_ENV").env_remove("NAROU_RELAY_ENDPOINT")
        .env_remove("NAROU_RS_WEB_MODE").env_remove("NAROU_RS_LEGACY_YAML")
        .env_remove("NAROU_RS_MIRROR_FILES");
    if !sqlite { command.env("NAROU_RS_LEGACY_YAML", "1"); }
    if web { command.env("NAROU_RS_WEB_MODE", "1"); }
    command.output().unwrap()
}

fn library(proxy: &Proxy, sqlite: bool, choice: Option<&str>) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for path in [".narou", ".narousetting", "home/.narousetting", "webnovel"] {
        std::fs::create_dir_all(root.join(path)).unwrap();
    }
    // Windows' known-folder lookup can ignore HOME/USERPROFILE. The library
    // override keeps the real user's global settings out of these tests.
    std::fs::write(root.join(".narousetting/global_setting.yaml"), "{}\n").unwrap();
    std::fs::write(root.join("home/.narousetting/global_setting.yaml"), "{}\n").unwrap();
    std::fs::write(root.join("webnovel/digest.yaml"), SITE).unwrap();
    let mut settings = "update.interval: 0\ndownload.interval: 0\nconvert.no-open: true\n".to_string();
    if let Some(choice) = choice {
        settings.push_str(&format!("download.choices-of-digest-options: '{choice}'\n"));
    }
    std::fs::write(root.join(".narou/local_setting.yaml"), settings).unwrap();
    std::fs::write(root.join(".narou/database.yaml"), format!(
        "1:\n  id: 1\n  title: Digest Novel\n  author: Test Author\n  file_title: digest\n  sitename: 8.8.8.8\n  toc_url: {TOC_URL}\n  ncode: n1234ab\n  novel_type: 1\n  general_all_no: 2\n  last_update: '2026-01-01T00:00:00Z'\n  _convert_failure: true\n"
    )).unwrap();
    let novel = root.join("小説データ/8.8.8.8/digest");
    std::fs::create_dir_all(novel.join("本文")).unwrap();
    let mut subtitles = Vec::new();
    for (index, title) in [("1", "第一話"), ("2", "第二話")] {
        let subtitle = narou_rs::downloader::SubtitleInfo {
            index: index.into(), href: format!("/novel/n1234ab/{index}/"),
            chapter: String::new(), subchapter: String::new(), subtitle: title.into(),
            file_subtitle: title.into(), subdate: "2026-01-01 00:00:00".into(),
            subupdate: None, download_time: Some("2026-01-01 00:00:00 +0900".into()),
        };
        let mut section = serde_yaml::to_value(&subtitle).unwrap();
        section["element"] = serde_yaml::from_str("data_type: text\nbody: 保存済みの本文\nintroduction: ''\npostscript: ''\n").unwrap();
        std::fs::write(novel.join(format!("本文/{index} {title}.yaml")), serde_yaml::to_string(&section).unwrap()).unwrap();
        subtitles.push(subtitle);
    }
    let toc = narou_rs::downloader::TocFile {
        title: "Digest Novel".into(), author: "Test Author".into(), toc_url: TOC_URL.into(),
        story: Some("保存済みのあらすじ".into()), subtitles, novel_type: Some(1),
    };
    std::fs::write(novel.join("toc.yaml"), serde_yaml::to_string(&toc).unwrap()).unwrap();
    std::fs::write(novel.join("saved.epub"), b"previous EPUB must remain byte-for-byte unchanged").unwrap();
    if sqlite { std::fs::write(root.join(".narou/storage-backend"), "sqlite\n").unwrap(); }
    let imported = run(root, proxy, sqlite, false, &["list"]);
    assert!(imported.status.success(), "{}", text(&imported));
    (temp, novel)
}

fn text(output: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

fn saved_files(novel: &Path) -> Vec<Vec<u8>> {
    ["toc.yaml", "本文/1 第一話.yaml", "本文/2 第二話.yaml", "saved.epub"]
        .iter().map(|name| std::fs::read(novel.join(name)).unwrap()).collect()
}

fn saved_objects(root: &Path, sqlite: bool) -> Vec<(String, Vec<u8>)> {
    if !sqlite { return Vec::new(); }
    let connection = rusqlite::Connection::open(root.join(".narou/db.sqlite")).unwrap();
    connection.prepare("SELECT object_key, data FROM objects ORDER BY object_key").unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap()
        .collect::<Result<_, _>>().unwrap()
}

#[test]
fn digest_cancellation_preserves_content_and_is_not_an_error_in_cli_or_web_mode() {
    let proxy = Proxy::start();
    for sqlite in [false, true] {
        for web in [false, true] {
            for choice in [None, Some(""), Some("2"), Some("5"), Some("5,2"), Some("5,invalid")] {
                let (temp, novel) = library(&proxy, sqlite, choice);
                let files = saved_files(&novel);
                let objects = saved_objects(temp.path(), sqlite);
                proxy.requests.lock().unwrap().clear();
                // Do not disable auto-conversion: even a previous conversion failure
                // must not cause a canceled update to rewrite the saved EPUB.
                let output = run(temp.path(), &proxy, sqlite, web, &["update", "1"]);
                let log = text(&output);
                assert!(output.status.success(), "sqlite={sqlite}, web={web}, choice={choice:?}: {log}");
                assert!(log.contains("の更新はキャンセルされました"), "{log}");
                assert!(!log.contains("件のエラーが発生しました"), "{log}");
                assert!(!log.contains("Convert error") && !log.contains("Converted:"), "{log}");
                assert_eq!(saved_files(&novel), files, "canceled update changed saved files");
                assert_eq!(saved_objects(temp.path(), sqlite), objects, "canceled update changed canonical objects");
                let requests = proxy.requests.lock().unwrap();
                assert!(!requests.is_empty(), "fixture must actually fetch the latest TOC");
                assert!(requests.iter().all(|line| line.starts_with(&format!("GET {TOC_URL} "))), "canceled update fetched a section: {requests:?}");
            }
        }
    }
}

#[test]
fn download_cancellation_is_not_an_error_and_skips_conversion() {
    // No readable sections: the downloader intentionally cancels registration.
    let proxy = Proxy::with_toc("<h1>Digest Novel</h1><h2>Test Author</h2>");
    for sqlite in [false, true] {
        let (temp, novel) = library(&proxy, sqlite, None);
        let files = saved_files(&novel);
        let output = run(temp.path(), &proxy, sqlite, true,
            &["download", "http://8.8.8.8/novel/n2345ab/"]);
        assert!(output.status.success(), "{}", text(&output));
        assert!(text(&output).contains("の更新はキャンセルされました"), "{}", text(&output));
        assert!(!text(&output).contains("Converted:"));
        assert_eq!(saved_files(&novel), files);
    }
}

#[test]
fn explicit_freeze_cancels_without_replacing_content() {
    let proxy = Proxy::start();
    for sqlite in [false, true] {
        let (temp, novel) = library(&proxy, sqlite, Some("5,3"));
        let files = saved_files(&novel);
        let output = run(temp.path(), &proxy, sqlite, true, &["update", "1"]);
        assert!(output.status.success(), "{}", text(&output));
        assert!(text(&output).contains("の更新はキャンセルされました"));
        assert_eq!(saved_files(&novel), files);
        let frozen: std::collections::HashMap<i64, serde_yaml::Value> =
            narou_rs::db::inventory::Inventory::new(temp.path().to_path_buf())
                .load("freeze", narou_rs::db::inventory::InventoryScope::Local).unwrap();
        assert!(frozen.contains_key(&1), "explicit 3 must still freeze the novel");
    }
}

#[test]
fn explicit_update_still_accepts_the_reduced_toc_and_real_errors_remain_errors() {
    let proxy = Proxy::start();
    for sqlite in [false, true] {
        let (temp, novel) = library(&proxy, sqlite, Some("5,1"));
        let output = run(temp.path(), &proxy, sqlite, true, &["update", "1", "--no-convert"]);
        assert!(output.status.success(), "{}", text(&output));
        let toc: narou_rs::downloader::TocFile = serde_yaml::from_slice(&std::fs::read(novel.join("toc.yaml")).unwrap()).unwrap();
        assert_eq!(toc.subtitles.len(), 1, "explicit 1 must still accept the reduced TOC");
        assert_eq!(toc.subtitles[0].index, "1");
        // Native may retain old section files; the active TOC defines which
        // sections are included in subsequent conversions.
        assert!(!text(&output).contains("の更新はキャンセルされました"));

        let (temp, novel) = library(&proxy, sqlite, Some("2"));
        let files = saved_files(&novel);
        let output = run(temp.path(), &proxy, sqlite, true, &["update", "1", "999999"]);
        assert_eq!(output.status.code(), Some(1), "only the invalid target is an error: {}", text(&output));
        assert!(text(&output).contains("1 件のエラーが発生しました"), "{}", text(&output));
        assert_eq!(saved_files(&novel), files);
    }
}
