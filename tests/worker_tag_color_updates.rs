#![cfg(feature = "native-runtime")]

// Execute the Worker's production snapshot codec, bind builder and ordered
// CAS batch against SQLite, including nullable legacy rows.
#[path = "../worker_entry/src/d1_repository/tag_colors.rs"]
mod tag_colors;

use narou_rs::application::tag_colors::{apply_tag_color_update, TagColorUpdate, TagColors};
use narou_rs::db::novel_codec::NovelBind;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
use tag_colors::{TagColorRow, TagColorSnapshot, READ_SQL, REMOVE_LEGACY_SQL, UPDATE_SQL};

type RawRow = Option<(Option<String>, Option<String>)>;

fn connection() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE app_state (
            scope TEXT NOT NULL, key TEXT NOT NULL,
            value_yaml TEXT, value_json TEXT,
            PRIMARY KEY(scope, key)
        );",
    ).unwrap();
    conn
}

fn insert(conn: &Connection, scope: &str, key: &str, yaml: Option<&str>, json: Option<&str>) {
    conn.execute(
        "INSERT INTO app_state(scope, key, value_yaml, value_json) VALUES (?1, ?2, ?3, ?4)",
        params![scope, key, yaml, json],
    ).unwrap();
}

fn raw(conn: &Connection, scope: &str, key: &str) -> RawRow {
    conn.query_row(
        "SELECT value_yaml, value_json FROM app_state WHERE scope = ?1 AND key = ?2",
        params![scope, key], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().unwrap()
}

fn snapshot(conn: &Connection) -> TagColorSnapshot {
    let mut statement = conn.prepare(READ_SQL).unwrap();
    let rows = statement.query_map([], |row| Ok(TagColorRow {
        scope: row.get(0)?, value_yaml: row.get(1)?, value_json: row.get(2)?,
    })).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    TagColorSnapshot::from_rows(rows)
}

fn commit(conn: &mut Connection, snapshot: TagColorSnapshot, update: &TagColorUpdate) -> Option<TagColors> {
    let mut colors = snapshot.colors().unwrap();
    apply_tag_color_update(&mut colors, update);
    let binds = snapshot.into_update_binds(colors.to_yaml_string().unwrap()).into_iter()
        .map(|bind| match bind {
            NovelBind::Text(value) => rusqlite::types::Value::Text(value),
            NovelBind::Int(value) => rusqlite::types::Value::Integer(value),
            NovelBind::Null => rusqlite::types::Value::Null,
        }).collect::<Vec<_>>();
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
    let returned = {
        let mut statement = transaction.prepare(UPDATE_SQL).unwrap();
        statement.query_map(params_from_iter(binds), |row| row.get::<_, String>(0))
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
    };
    transaction.execute(REMOVE_LEGACY_SQL, []).unwrap();
    transaction.commit().unwrap();
    if returned.is_empty() { None } else { Some(colors) }
}

fn commit_current(conn: &mut Connection, update: &TagColorUpdate) -> TagColors {
    let current = snapshot(conn);
    commit(conn, current, update).expect("fresh snapshot must commit")
}

fn ensure(tags: &[&str], color: Option<&str>) -> TagColorUpdate {
    TagColorUpdate::Ensure {
        tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
        default_color: color.map(str::to_string),
    }
}

fn set(tag: &str, color: Option<&str>) -> TagColorUpdate {
    TagColorUpdate::Set { tag: tag.to_string(), color: color.map(str::to_string) }
}

fn order(colors: &TagColors) -> Vec<String> {
    colors.to_yaml_value().as_mapping().unwrap().keys()
        .map(|tag| tag.as_str().unwrap().to_string()).collect()
}

#[test]
fn absent_rows_insert_an_ordered_table_and_no_overwrite_ensure_is_idempotent() {
    let mut conn = connection();
    let colors = commit_current(&mut conn, &ensure(&["a", "b", "a"], None));
    assert_eq!(colors.color_for("a"), Some("green"));
    assert_eq!(colors.color_for("b"), Some("yellow"));
    assert_eq!(order(&colors), vec!["a", "b"]);
    let colors = commit_current(&mut conn, &ensure(&["a", "b"], Some("red")));
    assert_eq!(colors.color_for("a"), Some("green"));
    assert_eq!(colors.color_for("b"), Some("yellow"));
    let (yaml, json) = raw(&conn, "inv", "tag_colors").unwrap();
    assert_eq!(json.as_deref(), Some("{}"));
    assert_eq!(order(&TagColors::from_yaml_payload(&yaml.unwrap()).unwrap()), vec!["a", "b"]);
}

#[test]
fn legacy_canonicalization_preserves_precedence_and_order_in_the_committed_payload() {
    let mut conn = connection();
    insert(&conn, "inv", "tag_colors", Some("shared: blue\ncurrent: green\n"), Some("{}"));
    insert(&conn, "tag_colors", "colors", Some("{}"), Some(r#"{"shared":"red","legacy":"yellow"}"#));
    let colors = commit_current(&mut conn, &ensure(&["new"], None));
    assert_eq!(colors.color_for("shared"), Some("blue"));
    assert_eq!(colors.color_for("legacy"), Some("yellow"));
    assert_eq!(colors.color_for("new"), Some("yellow"));
    assert_eq!(order(&colors), vec!["shared", "legacy", "current", "new"]);
    assert_eq!(raw(&conn, "tag_colors", "colors"), None);
    let persisted = snapshot(&conn).colors().unwrap();
    assert_eq!(order(&persisted), order(&colors));
    assert_eq!(persisted.color_for("shared"), Some("blue"));
}

#[test]
fn legacy_only_and_noop_deltas_still_canonicalize_with_a_successful_cas() {
    let mut conn = connection();
    insert(&conn, "tag_colors", "colors", None, Some(r#"{"X":"red"}"#));
    let colors = commit_current(&mut conn, &ensure(&["X"], Some("blue")));
    assert_eq!(colors.color_for("X"), Some("red"));
    assert!(raw(&conn, "inv", "tag_colors").is_some());
    assert_eq!(raw(&conn, "tag_colors", "colors"), None);
}

#[test]
fn stale_autoassignment_retries_against_the_committed_manual_color() {
    let mut conn = connection();
    commit_current(&mut conn, &set("X", Some("green")));
    let automatic_snapshot = snapshot(&conn);
    commit_current(&mut conn, &set("X", Some("red")));
    let automatic = ensure(&["Y"], None);
    assert!(commit(&mut conn, automatic_snapshot, &automatic).is_none());
    let colors = commit_current(&mut conn, &automatic);
    assert_eq!(colors.color_for("X"), Some("red"));
    assert_eq!(colors.color_for("Y"), Some("white"));
    assert_eq!(snapshot(&conn).colors().unwrap().color_for("X"), Some("red"));
}

#[test]
fn stale_autoassignment_does_not_resurrect_a_removed_legacy_or_canonical_tag() {
    let mut conn = connection();
    insert(&conn, "inv", "tag_colors", Some("X: blue\n"), Some("{}"));
    insert(&conn, "tag_colors", "colors", Some("{}"), Some(r#"{"X":"red"}"#));
    let automatic_snapshot = snapshot(&conn);
    commit_current(&mut conn, &set("X", None));
    let automatic = ensure(&["Y"], None);
    assert!(commit(&mut conn, automatic_snapshot, &automatic).is_none());
    let colors = commit_current(&mut conn, &automatic);
    assert_eq!(colors.color_for("X"), None);
    assert_eq!(colors.color_for("Y"), Some("green"));
    assert_eq!(order(&colors), vec!["Y"]);
    assert_eq!(raw(&conn, "tag_colors", "colors"), None);
}

#[test]
fn any_changed_canonical_or_legacy_raw_column_rejects_the_whole_batch() {
    for (scope, key, column, replacement) in [
        ("inv", "tag_colors", "value_yaml", "X: green\n\n"),
        ("inv", "tag_colors", "value_json", r#"{"ignored":"cyan"}"#),
        ("tag_colors", "colors", "value_yaml", "{}\n"),
        ("tag_colors", "colors", "value_json", r#"{"legacy":"red"}"#),
    ] {
        let mut conn = connection();
        insert(&conn, "inv", "tag_colors", Some("X: green\n"), Some("{}"));
        insert(&conn, "tag_colors", "colors", Some("{}"), Some(r#"{"legacy":"yellow"}"#));
        let stale = snapshot(&conn);
        conn.execute(
            &format!("UPDATE app_state SET {column} = ?1 WHERE scope = ?2 AND key = ?3"),
            params![replacement, scope, key],
        ).unwrap();
        let canonical_before = raw(&conn, "inv", "tag_colors");
        let legacy_before = raw(&conn, "tag_colors", "colors");
        assert!(commit(&mut conn, stale, &ensure(&["new"], None)).is_none(), "{scope}/{column}");
        assert_eq!(raw(&conn, "inv", "tag_colors"), canonical_before);
        assert_eq!(raw(&conn, "tag_colors", "colors"), legacy_before, "failed CAS cannot delete legacy");
        let colors = commit_current(&mut conn, &ensure(&["new"], None));
        assert_eq!(colors.color_for("X"), Some("green"));
        assert_eq!(raw(&conn, "tag_colors", "colors"), None);
    }
}

#[test]
fn absent_and_present_null_rows_are_not_interchangeable() {
    for (scope, key) in [("inv", "tag_colors"), ("tag_colors", "colors")] {
        let mut conn = connection();
        let absent = snapshot(&conn);
        insert(&conn, scope, key, None, None);
        assert!(commit(&mut conn, absent, &ensure(&["new"], None)).is_none());
        assert_eq!(raw(&conn, scope, key), Some((None, None)));
        let colors = commit_current(&mut conn, &ensure(&["new"], None));
        assert_eq!(colors.color_for("new"), Some("green"));
        assert_eq!(raw(&conn, "tag_colors", "colors"), None);

        let mut conn = connection();
        insert(&conn, "inv", "tag_colors", None, None);
        insert(&conn, "tag_colors", "colors", None, None);
        let present = snapshot(&conn);
        conn.execute("DELETE FROM app_state WHERE scope = ?1 AND key = ?2", params![scope, key]).unwrap();
        let canonical_before = raw(&conn, "inv", "tag_colors");
        let legacy_before = raw(&conn, "tag_colors", "colors");
        assert!(commit(&mut conn, present, &ensure(&["new"], None)).is_none());
        assert_eq!(raw(&conn, "inv", "tag_colors"), canonical_before);
        assert_eq!(raw(&conn, "tag_colors", "colors"), legacy_before);
    }
}

#[test]
fn null_and_text_changes_in_each_raw_column_conflict_without_deleting_legacy() {
    for (scope, key) in [("inv", "tag_colors"), ("tag_colors", "colors")] {
        for column in ["value_yaml", "value_json"] {
            for (initial, replacement) in [(None, Some("{}")), (Some("{}"), None)] {
                let mut conn = connection();
                insert(&conn, "inv", "tag_colors", initial, initial);
                insert(&conn, "tag_colors", "colors", initial, initial);
                let stale = snapshot(&conn);
                conn.execute(
                    &format!("UPDATE app_state SET {column} = ?1 WHERE scope = ?2 AND key = ?3"),
                    params![replacement, scope, key],
                ).unwrap();
                let canonical_before = raw(&conn, "inv", "tag_colors");
                let legacy_before = raw(&conn, "tag_colors", "colors");
                assert!(commit(&mut conn, stale, &ensure(&["new"], None)).is_none());
                assert_eq!(raw(&conn, "inv", "tag_colors"), canonical_before);
                assert_eq!(raw(&conn, "tag_colors", "colors"), legacy_before);
                let colors = commit_current(&mut conn, &ensure(&["new"], None));
                assert_eq!(colors.color_for("new"), Some("green"));
                assert_eq!(raw(&conn, "tag_colors", "colors"), None);
            }
        }
    }
}

#[test]
fn parse_failure_leaves_both_persisted_rows_untouched() {
    let conn = connection();
    insert(&conn, "inv", "tag_colors", Some("X: [green"), Some("{}"));
    insert(&conn, "tag_colors", "colors", Some("{}"), Some(r#"{"legacy":"yellow"}"#));
    let canonical_before = raw(&conn, "inv", "tag_colors");
    let legacy_before = raw(&conn, "tag_colors", "colors");
    assert!(snapshot(&conn).colors().is_err());
    assert_eq!(raw(&conn, "inv", "tag_colors"), canonical_before);
    assert_eq!(raw(&conn, "tag_colors", "colors"), legacy_before);
}
