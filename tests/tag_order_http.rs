//! Exercise real HTTP adapters with a legacy-shaped library in both storage modes.
use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use serde_json::{Value, json};

struct Server {
    child: Child,
    _root: tempfile::TempDir,
    client: Client,
    base: String,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(sqlite: bool, order: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let library = root.path().join("library");
        let home = root.path().join("home");
        let state = library.join(".narou");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(home.join(".narousetting")).unwrap();
        std::fs::write(home.join(".narousetting/global_setting.yaml"), "server-ws-port: 0\n").unwrap();
        std::fs::write(state.join("local_setting.yaml"), format!("webui.tag-sort-order: {order}\n")).unwrap();
        std::fs::write(state.join("favorite_tags.yaml"), "- Zulu\n- Middle\n- '1023'\n- '2'\n").unwrap();
        if sqlite {
            std::fs::write(state.join("storage-backend"), "sqlite").unwrap();
        }
        let records: BTreeMap<i64, Value> = [
            (1, vec!["Zulu", "Alpha", "1", "2", "5"]),
            (1023, vec!["Middle", "2", "1023"]),
            (2048, vec!["New", "Middle"]),
        ].into_iter().map(|(id, tags)| (id, json!({
            "id": id, "title": format!("Fixture {id}"), "author": "Fixture author",
            "file_title": format!("fixture-{id}"), "toc_url": format!("https://example.invalid/{id}/"),
            "sitename": "Fixture", "novel_type": 1, "end": false,
            "last_update": "2026-11-01 00:00:00.000000000 +00:00",
            "use_subdirectory": false, "tags": tags, "ncode": format!("fixture{id}"),
            "suspend": false, "is_narou": false
        }))).collect();
        std::fs::write(state.join("database.yaml"), serde_yaml::to_string(&records).unwrap()).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let log_path = root.path().join("server.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_narou_rs"));
        command.env_clear();
        for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "SYSTEMDRIVE", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let child = command.args(["web", "--no-browser", "--port", &port.to_string()])
            .current_dir(&library).env("HOME", &home).env("USERPROFILE", &home)
            .env("NAROU_RS_LEGACY_YAML", if sqlite { "0" } else { "1" })
            .stdin(Stdio::null()).stdout(Stdio::from(log.try_clone().unwrap())).stderr(Stdio::from(log))
            .spawn().unwrap();
        let client = Client::builder().timeout(Duration::from_secs(10)).no_proxy().build().unwrap();
        let mut server = Self { child, _root: root, client, base: format!("http://127.0.0.1:{port}"), token: String::new() };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if server.client.get(&server.base).send().is_ok() { break; }
            assert!(server.child.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "server failed: {}", std::fs::read_to_string(&log_path).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(50));
        }
        server.token = std::fs::read_to_string(state.join("server.pid")).unwrap()
            .split_whitespace().nth(1).unwrap().to_string();
        server
    }

    fn get(&self, path: &str) -> Value {
        let response = self.client.get(format!("{}{path}", self.base)).header("x-narou-internal-token", &self.token)
            .send().unwrap().error_for_status().unwrap();
        serde_json::from_slice(&response.bytes().unwrap()).unwrap()
    }

    fn post(&self, path: &str, body: Value) -> Value {
        let response = self.client.post(format!("{}{path}", self.base))
            .header("x-narou-internal-token", &self.token).header("content-type", "application/json")
            .body(serde_json::to_vec(&body).unwrap()).send().unwrap().error_for_status().unwrap();
        let value: Value = serde_json::from_slice(&response.bytes().unwrap()).unwrap();
        assert_eq!(value["success"], true, "mutation failed: {value}");
        value
    }
}

#[test]
fn legacy_library_orders_and_colors_are_stable_in_yaml_and_sqlite() {
    for sqlite in [false, true] {
        let count_server = Server::start(sqlite, "count");
        let count_colors = count_server.get("/api/tag_list?format=json")["tag_colors"].clone();
        drop(count_server);
        let server = Server::start(sqlite, "favorites_first");
        let initial = server.get("/api/tag_list?format=json");
        assert_eq!(initial["tags"], json!(["Zulu", "Middle", "1023", "2", "1", "5", "Alpha", "New"]));
        assert_eq!(initial["tag_colors"], count_colors, "display order must not choose newly assigned colors; sqlite={sqlite}");
        for (order, expected) in [
            ("alphabetical", vec!["1", "1023", "2", "5", "Alpha", "Middle", "New", "Zulu"]),
            ("registered", vec!["1", "2", "5", "Alpha", "Zulu", "1023", "Middle", "New"]),
            ("count", vec!["2", "Middle", "1", "1023", "5", "Alpha", "New", "Zulu"]),
            ("favorites_first", vec!["Zulu", "Middle", "1023", "2", "1", "5", "Alpha", "New"]),
            ("alphabetical", vec!["1", "1023", "2", "5", "Alpha", "Middle", "New", "Zulu"]),
        ] {
            server.post("/api/global_setting", json!({"settings": {"webui.tag-sort-order": order}}));
            let actual = server.get("/api/tag_list?format=json");
            assert_eq!(actual["tags"], json!(expected), "order={order}; sqlite={sqlite}");
            assert_eq!(actual["tag_colors"], count_colors);
        }
        server.post("/api/global_setting", json!({"settings": {"webui.tag-sort-order": "favorites_first"}}));
        server.post("/api/favorite_tags", json!({"action": "add", "tags": ["New"]}));
        server.post("/api/favorite_tags", json!({"action": "reorder", "tags": ["New", "Zulu", "Middle", "1023", "2"]}));
        let reordered = server.get("/api/tag_list?format=json");
        assert_eq!(reordered["tags"], json!(["New", "Zulu", "Middle", "1023", "2", "1", "5", "Alpha"]));
        assert_eq!(reordered["tag_colors"], count_colors);
        let before = server.get("/api/favorite_tags");
        let rejected = server.client.post(format!("{}/api/favorite_tags", server.base))
            .header("x-narou-internal-token", &server.token)
            .header("content-type", "application/json")
            .body(r#"{"action":"unknown","tags":["unexpected"]}"#).send().unwrap();
        assert_eq!(rejected.status().as_u16(), 400);
        assert_eq!(server.get("/api/favorite_tags"), before);
    }
}

#[test]
fn malformed_favorite_state_is_reported_without_overwriting_it() {
    for sqlite in [false, true] {
        let server = Server::start(sqlite, "favorites_first");
        server.get("/api/favorite_tags");
        let state = server._root.path().join("library/.narou");
        let path = state.join("favorite_tags.yaml");
        let malformed = "tags: not-a-list\n";
        let db = sqlite.then(|| rusqlite::Connection::open(state.join("db.sqlite")).unwrap());
        if let Some(db) = &db {
            assert_eq!(db.execute(
                "UPDATE app_state SET value_yaml=?1, value_json=?2 WHERE scope='inv' AND key='favorite_tags'",
                rusqlite::params![malformed, r#"{"tags":"not-a-list"}"#],
            ).unwrap(), 1);
        } else {
            std::fs::write(&path, malformed).unwrap();
        }
        for endpoint in ["/api/favorite_tags", "/api/tag_list?format=json"] {
            let response = server.client.get(format!("{}{endpoint}", server.base))
                .header("x-narou-internal-token", &server.token).send().unwrap();
            assert_eq!(response.status().as_u16(), 503, "{endpoint}; sqlite={sqlite}");
        }
        let response = server.client.post(format!("{}/api/favorite_tags", server.base))
            .header("x-narou-internal-token", &server.token).header("content-type", "application/json")
            .body(r#"{"action":"add","tags":["New"]}"#).send().unwrap();
        assert_eq!(response.status().as_u16(), 503);
        let stored: String = if let Some(db) = &db {
            db.query_row("SELECT value_yaml FROM app_state WHERE scope='inv' AND key='favorite_tags'", [], |row| row.get(0)).unwrap()
        } else {
            std::fs::read_to_string(path).unwrap()
        };
        assert_eq!(stored, malformed);
        server.post("/api/global_setting", json!({"settings": {"webui.tag-sort-order": "alphabetical"}}));
        assert_eq!(server.get("/api/tag_list?format=json")["tags"],
            json!(["1", "1023", "2", "5", "Alpha", "Middle", "New", "Zulu"]));
    }
}
