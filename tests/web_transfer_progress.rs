#![cfg(feature = "native-runtime")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use base64::Engine;
use serde_json::Value;

struct Server { child: Child, base: String, token: String }
impl Drop for Server {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

fn start(library: &Path) -> Server {
    std::fs::create_dir_all(library.join(".narou")).unwrap();
    std::fs::create_dir_all(library.join(".narousetting")).unwrap();
    std::fs::write(library.join(".narousetting/global_setting.yaml"), "server-ws-port: 0\n").unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let log_path = library.join("server-test.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_narou_rs"))
        .args(["web", "--port", &port.to_string(), "--no-browser"])
        .current_dir(library).env("HOME", library).env("USERPROFILE", library)
        .env_remove("NAROU_RS_LEGACY_YAML").env_remove("NAROU_RS_MIRROR_FILES")
        .stdin(Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap();
    let mut server = Server { child, base: format!("http://127.0.0.1:{port}"), token: String::new() };
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(2)).build().unwrap();
    for _ in 0..100 {
        if client.get(&server.base).send().is_ok() {
            let pid = std::fs::read_to_string(library.join(".narou/server.pid")).unwrap();
            server.token = pid.split_whitespace().nth(1).unwrap().to_string();
            return server;
        }
        assert!(server.child.try_wait().unwrap().is_none(), "{}", std::fs::read_to_string(&log_path).unwrap());
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("native web did not start: {}", std::fs::read_to_string(log_path).unwrap());
}

fn request(client: &reqwest::blocking::Client, server: &Server, path: &str) -> reqwest::blocking::RequestBuilder {
    client.get(format!("{}{path}", server.base)).header("x-narou-internal-token", &server.token)
}

fn events(response: reqwest::blocking::Response) -> (Vec<Value>, Vec<u8>) {
    assert!(response.status().is_success());
    assert!(response.headers()["content-type"].to_str().unwrap().contains("application/x-ndjson"));
    let mut frames = Vec::new();
    let mut zip = Vec::new();
    for line in response.text().unwrap().lines() {
        let value: Value = serde_json::from_str(line).unwrap();
        assert_ne!(value["type"], "error", "{value}");
        if value["type"] == "data" {
            zip.extend(base64::engine::general_purpose::STANDARD.decode(value["data"].as_str().unwrap()).unwrap());
        } else { frames.push(value); }
    }
    assert_eq!(frames.last().unwrap()["type"], "complete");
    (frames, zip)
}

fn seed_158(library: &Path) -> Vec<i64> {
    let mut records = serde_yaml::Mapping::new();
    let mut ids = Vec::new();
    for index in 0..158 {
        let id = index + 100;
        ids.push(id);
        let title = format!("作品 {index}");
        let url = format!("https://example.test/bulk/{index}/");
        let record = serde_json::json!({"id":id,"author":"作者","title":title,
            "file_title":format!("bulk-{index}"),"toc_url":url,"sitename":"TransferTest",
            "novel_type":1,"end":false,"last_update":"2026-11-02T00:00:00Z",
            "use_subdirectory":false,"tags":["検証"],"ncode":format!("bulk-{index}"),
            "domain":"example.test","general_all_no":1,"is_narou":false});
        records.insert(serde_yaml::to_value(id).unwrap(), serde_yaml::to_value(record).unwrap());
        let directory = library.join("小説データ/TransferTest").join(format!("bulk-{index}"));
        std::fs::create_dir_all(directory.join("本文")).unwrap();
        std::fs::write(directory.join("toc.yaml"), format!("title: {title}\nauthor: 作者\ntoc_url: {url}\nsubtitles: []\nnovel_type: 1\n")).unwrap();
        std::fs::write(directory.join("本文/1 話.yaml"), format!("body: 本文 {index}\n")).unwrap();
    }
    std::fs::create_dir_all(library.join(".narou")).unwrap();
    std::fs::write(library.join(".narou/database.yaml"), serde_yaml::to_string(&records).unwrap()).unwrap();
    ids
}

#[test]
fn native_web_exports_and_imports_158_novels_with_real_progress_and_errors() {
    let temp = tempfile::tempdir().unwrap();
    let source: PathBuf = temp.path().join("source");
    let target: PathBuf = temp.path().join("target");
    let ids = seed_158(&source);
    let source_server = start(&source);
    let target_server = start(&target);
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(60)).build().unwrap();
    let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
    let (exported, zip) = events(request(&client, &source_server, &format!("/api/transfer/export?progress=1&ids={ids}")).send().unwrap());
    assert_eq!(exported.last().unwrap()["bytes"].as_u64().unwrap(), zip.len() as u64);
    assert!(exported.iter().any(|frame| frame["type"] == "progress" && frame["completed"] == 158 && frame["total"] == 158));
    let parsed = narou_rs::transfer::parse_archive(&zip).unwrap();
    assert_eq!(parsed.novels.len(), 158);
    for (index, novel) in parsed.novels.iter().enumerate() {
        assert_eq!(novel.files.iter().find(|(path, _)| path == "本文/1 話.yaml").unwrap().1,
            format!("body: 本文 {index}\n").as_bytes());
    }
    let imported_response = client.post(format!("{}/api/transfer/import?progress=1", target_server.base))
        .header("x-narou-internal-token", &target_server.token).header("Content-Type", "application/zip")
        .body(zip).send().unwrap();
    let (imported, _) = events(imported_response);
    let result = &imported.last().unwrap()["result"];
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(result["created"], 158);
    assert!(imported.iter().any(|frame| frame["phase"] == "import" && frame["completed"] == 158));
    let records = std::fs::read_to_string(target.join(".narou/database.yaml")).unwrap();
    let records: serde_yaml::Mapping = serde_yaml::from_str(&records).unwrap();
    assert_eq!(records.len(), 158);
    let missing = request(&client, &source_server, "/api/transfer/export?progress=1&ids=999999").send().unwrap().text().unwrap();
    let frames: Vec<Value> = missing.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(frames.last().unwrap()["type"], "error");
    assert!(!frames.iter().any(|frame| frame["type"] == "complete"));
}
