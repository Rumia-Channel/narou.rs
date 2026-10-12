//! Pure listing payload for the login status endpoints.
//!
//! `GET /api/login` (and the action handlers that echo the same `data`) must
//! show only masked credentials and per-site at-rest encryption flags. The
//! stored payload is read once; flags are computed from that single raw map
//! instead of reloading it per site (`enc:v1:` prefix check, exactly what
//! `D1CookieStore::is_encrypted` reads).
//!
//! Kept free of Worker types so host tests can pin the response shape and the
//! "no plaintext credential" rule.

use std::collections::{BTreeMap, BTreeSet};

use narou_rs::platform::{LoginGroup, mask_cookie, parse_cookie_header};

/// Sites whose stored raw value is encrypted at rest.
///
/// Mirrors the per-site lookup: the key is the trimmed, lowercased site name
/// (legacy per-host keys keep the old exact-key behavior and are not flagged).
pub fn encrypted_sites(
    raw: &BTreeMap<String, String>,
    sites: &BTreeMap<String, Vec<LoginGroup>>,
) -> BTreeSet<String> {
    sites
        .keys()
        .filter(|site| {
            raw.get(&site.trim().to_ascii_lowercase())
                .is_some_and(|value| narou_rs::login::is_encrypted_at_rest(value))
        })
        .cloned()
        .collect()
}

/// native `status_payload`: site names, login names and cookie lengths only —
/// never a cookie value. `indices`/order follow the stored try order.
pub fn status_json(
    sites: &BTreeMap<String, Vec<LoginGroup>>,
    encrypted: &BTreeSet<String>,
    key_source: &str,
) -> serde_json::Value {
    let mut total = 0;
    let entries: Vec<serde_json::Value> = sites
        .iter()
        .map(|(site, groups)| {
            total += groups.len();
            let logins: Vec<serde_json::Value> = groups
                .iter()
                .enumerate()
                .map(|(index, group)| {
                    let hosts: Vec<serde_json::Value> = group
                        .cookies
                        .iter()
                        .map(|entry| {
                            serde_json::json!({
                                "host": entry.host,
                                "cookies": mask_cookie(&entry.cookie),
                                "names": parse_cookie_header(&entry.cookie)
                                    .into_iter()
                                    .map(|(name, _)| name)
                                    .collect::<Vec<_>>(),
                                "length": entry.cookie.chars().count(),
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "index": index,
                        "id": group.id,
                        "short_id": group.short_id(),
                        "label": group.label,
                        "display_name": group.display_name(),
                        "hosts": hosts,
                        "host_count": group.cookies.len(),
                        "added_at": group.added_at,
                    })
                })
                .collect();
            serde_json::json!({
                "site": site,
                "logins": logins,
                "encrypted": encrypted.contains(site),
            })
        })
        .collect();
    serde_json::json!({
        "sites": entries,
        "sites_count": sites.len(),
        "logins_count": total,
        "key_source": key_source,
    })
}
