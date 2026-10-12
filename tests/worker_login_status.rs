//! Pin the Worker login status payload: masked values only, encryption flags
//! computed from one raw payload read.
//!
//! The Worker crate itself is wasm-only, so the production module is included
//! here directly (same pattern as `tests/worker_storage_probe.rs`).

#[path = "../worker_entry/src/login/listing.rs"]
mod listing;

use std::collections::{BTreeMap, BTreeSet};

use narou_rs::platform::{HostCookie, LoginGroup};

fn group(site: &str, id: &str, label: Option<&str>, cookies: &[(&str, &str)]) -> LoginGroup {
    let cookies = cookies
        .iter()
        .map(|(host, cookie)| HostCookie {
            host: (*host).to_string(),
            cookie: (*cookie).to_string(),
        })
        .collect();
    let mut group = LoginGroup::new(site, cookies).with_id(id);
    group.label = label.map(str::to_string);
    group
}

fn sites(entries: &[(&str, LoginGroup)]) -> BTreeMap<String, Vec<LoginGroup>> {
    entries
        .iter()
        .map(|(site, group)| ((*site).to_string(), vec![group.clone()]))
        .collect()
}

#[test]
fn encryption_flags_follow_the_raw_prefix_of_the_site_key() {
    let mut raw = BTreeMap::new();
    raw.insert("example.com".to_string(), "enc:v1:token".to_string());
    raw.insert("plain.example".to_string(), "session=abc".to_string());
    // A legacy per-host key is folded to `folded.example`; the site-name key is
    // absent, so the flag keeps the old exact-key behavior (not encrypted).
    raw.insert("www.folded.example".to_string(), "enc:v1:token".to_string());
    let sites = sites(&[
        ("example.com", group("example.com", "id1", None, &[("example.com", "session=abc")])),
        ("plain.example", group("plain.example", "id2", None, &[("plain.example", "session=abc")])),
        ("folded.example", group("folded.example", "id3", None, &[("www.folded.example", "session=abc")])),
    ]);

    let flags = listing::encrypted_sites(&raw, &sites);
    assert!(flags.contains("example.com"));
    assert!(!flags.contains("plain.example"));
    assert!(!flags.contains("folded.example"));
    assert_eq!(flags.len(), 1);
}

#[test]
fn payload_masks_cookies_and_never_carries_the_stored_value() {
    let secret = "session=supersecret; other=1";
    let sites = sites(&[
        (
            "example.com",
            group("example.com", "abcdefgh-1234", Some("メイン"), &[("example.com", secret)]),
        ),
    ]);
    let mut flags = BTreeSet::new();
    flags.insert("example.com".to_string());
    let payload = listing::status_json(&sites, &flags, "NAROU_RS_LOGIN_KEY");

    assert_eq!(payload["sites_count"], 1);
    assert_eq!(payload["logins_count"], 1);
    assert_eq!(payload["key_source"], "NAROU_RS_LOGIN_KEY");
    let site = &payload["sites"][0];
    assert_eq!(site["site"], "example.com");
    assert_eq!(site["encrypted"], true);
    let login = &site["logins"][0];
    assert_eq!(login["id"], "abcdefgh-1234");
    assert_eq!(login["short_id"], "abcdefgh");
    assert_eq!(login["display_name"], "メイン");
    assert_eq!(login["host_count"], 1);
    assert_eq!(login["hosts"][0]["names"][0], "session");
    assert_eq!(login["hosts"][0]["length"], secret.chars().count() as i64);

    let serialized = payload.to_string();
    assert!(!serialized.contains("supersecret"), "{serialized}");
    assert!(!serialized.contains("other=1"), "{serialized}");
    // The display form keeps the cookie name with a masked value.
    let masked = login["hosts"][0]["cookies"].as_str().expect("masked string");
    assert!(masked.starts_with("session=…; other=…"), "{masked}");
}

#[test]
fn an_unflagged_site_reports_false_not_absent() {
    let sites = sites(&[("example.com", group("example.com", "id", None, &[("example.com", "a=b")]))]);
    let payload = listing::status_json(&sites, &BTreeSet::new(), "none");
    assert_eq!(payload["sites"][0]["encrypted"], false);
}
