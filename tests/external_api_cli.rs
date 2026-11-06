//! The external API's Bearer tokens and the stdIO MCP server.
//!
//! The CLI stores tokens in the library's `api_tokens` inventory (YAML mode)
//! or in `app_state` (SQLite mode), and the stdIO MCP server answers the
//! same JSON-RPC requests the HTTP endpoint does.
use std::path::Path;
use std::process::{Command, Output};

/// Run the CLI in `root`.
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_narou_rs"))
        .current_dir(root)
        .env_remove("NAROU_RS_LEGACY_YAML")
        .env("USERPROFILE", root.join("home"))
        .env("HOME", root.join("home"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn library(sqlite: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join(".narou")).unwrap();
    if sqlite {
        std::fs::write(root.join(".narou/storage-backend"), "sqlite").unwrap();
    }

    let created = run(root, &["api", "create", "テスト"]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let output = stdout(&created);
    assert!(output.contains("トークンを作成しました"), "{output}");
    assert!(output.contains("nrt_"), "{output}");
    // The token itself is shown once, at creation time.
    let token = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("トークン: "))
        .expect("the created token is printed")
        .to_string();

    let listed = stdout(&run(root, &["api", "list"]));
    assert!(listed.contains("テスト"), "{listed}");
    assert!(listed.contains("1 件"), "{listed}");
    // The list shows the fingerprint, not the token itself.
    assert!(!listed.contains(&token), "{listed}");

    let info = stdout(&run(root, &["api", "info"]));
    assert!(info.contains("/api/v1"), "{info}");
    assert!(info.contains("Bearer 認証"), "{info}");

    // The stored value is the inventory entry (YAML mode) or app_state
    // (SQLite mode); both keep the token so it can be inspected later.
    if sqlite {
        let connection = rusqlite::Connection::open(root.join(".narou/db.sqlite")).unwrap();
        let stored: String = connection
            .query_row(
                "SELECT value_yaml FROM app_state WHERE scope='inv' AND key='api_tokens'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(stored.contains(&token), "{stored}");
    } else {
        let stored = std::fs::read_to_string(root.join(".narou/api_tokens.yaml")).unwrap();
        assert!(stored.contains(&token), "{stored}");
    }

    // Deleting removes it from the list and from the store. The list prints
    // one indented line per token, and its first field is the id.
    let id = stdout(&run(root, &["api", "list"]))
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .find(|field| field.len() > 16 && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .expect("the list shows the token id")
        .to_string();
    let deleted = run(root, &["api", "delete", &id]);
    assert!(
        deleted.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&deleted),
        String::from_utf8_lossy(&deleted.stderr)
    );
    assert!(stdout(&deleted).contains("削除しました"), "{}", stdout(&deleted));
    let listed = stdout(&run(root, &["api", "list"]));
    assert!(listed.contains("トークンはありません"), "{listed}");
}

#[test]
fn api_tokens_are_managed_in_yaml_mode() {
    library(false);
}

#[test]
fn api_tokens_are_managed_in_sqlite_mode() {
    library(true);
}

#[test]
fn deleting_hyphen_prefixed_token_ids_keeps_other_tokens() {
    use narou_rs::application::api_tokens::ApiToken;

    let ids = ["-AAAAAAAAAAAAAAAAAAAAA", "--AAAAAAAAAAAAAAAAAAAA", "kept"];
    for sqlite in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join(".narou")).unwrap();
        if sqlite {
            std::fs::write(root.join(".narou/storage-backend"), "sqlite").unwrap();
        }
        let tokens: Vec<_> = ids.iter().map(|id| ApiToken {
            id: (*id).to_string(),
            name: "fixture".to_string(),
            token: format!("nrt_fixture_{id}"),
            created_at: "2026-11-06 00:00:00 +0000".to_string(),
        }).collect();
        std::fs::write(
            root.join(".narou/api_tokens.yaml"),
            serde_yaml::to_string(&tokens).unwrap(),
        ).unwrap();

        for (index, id) in ids[..2].iter().enumerate() {
            let deleted = run(root, &["api", "delete", id]);
            assert!(
                deleted.status.success(),
                "sqlite={sqlite}, id={id}, stdout: {}\nstderr: {}",
                stdout(&deleted),
                String::from_utf8_lossy(&deleted.stderr),
            );
            let stored: String = if sqlite {
                let connection = rusqlite::Connection::open(root.join(".narou/db.sqlite")).unwrap();
                connection.query_row(
                    "SELECT value_yaml FROM app_state WHERE scope='inv' AND key='api_tokens'",
                    [],
                    |row| row.get(0),
                ).unwrap()
            } else {
                std::fs::read_to_string(root.join(".narou/api_tokens.yaml")).unwrap()
            };
            let remaining: Vec<ApiToken> = serde_yaml::from_str(&stored).unwrap();
            assert_eq!(
                remaining.iter().map(|token| token.id.as_str()).collect::<Vec<_>>(),
                ids[index + 1..],
                "sqlite={sqlite}, deleted={id}",
            );
        }
    }
}

/// The stdIO MCP server answers the same JSON-RPC requests as the HTTP
/// endpoint, and reads the library directly.
#[test]
fn the_stdio_mcp_server_answers_tool_calls() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join(".narou")).unwrap();

    let request = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_novels","arguments":{"format":"json"}}}"#,
        "\n",
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_narou_rs_mcp"))
        .current_dir(root)
        .env_remove("NAROU_RS_LEGACY_YAML")
        .env("USERPROFILE", root.join("home"))
        .env("HOME", root.join("home"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(request.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8_lossy(&output.stdout).into_owned();
    let lines: Vec<&str> = responses.lines().filter(|line| !line.trim().is_empty()).collect();
    // The notification is answered with nothing.
    assert_eq!(lines.len(), 3, "{responses}");

    let initialize: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(initialize["result"]["serverInfo"]["name"], "narou.rs");

    let tools: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"list_novels"), "{names:?}");
    assert!(names.contains(&"get_novel_section"), "{names:?}");

    let call: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(call["result"]["isError"], false);
    assert!(call["result"]["content"][0]["text"].is_string());
}
