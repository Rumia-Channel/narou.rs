//! Portable login export.
//!
//! An export is a small YAML document that carries the cookies of one or more
//! hosts from the machine with the browser to the machine that downloads.
//! With a passphrase the payload is encrypted (Argon2id → XChaCha20-Poly1305);
//! without one the cookies are written in clear and the envelope says so, so an
//! import never silently trusts a file whose secrecy was assumed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{NarouError, Result};
use crate::platform::cookie_store::LegacyCredential;
use crate::platform::LoginGroup;
use crate::login::crypto::{
    ENVELOPE_AAD, SALT_LEN, decrypt_with_key, derive_key, encrypt_with_key, random_bytes,
};

/// Envelope format version understood by this build.
pub const EXPORT_VERSION: u32 = 3;
/// Versions written before logins were grouped by site.
pub const EXPORT_VERSION_LEGACY: u32 = 1;
/// Version that kept one credential list per host.
pub const EXPORT_VERSION_PER_HOST: u32 = 2;
/// `kdf` value written for encrypted envelopes.
pub const KDF_ARGON2ID: &str = "argon2id";

fn login_error(message: impl Into<String>) -> NarouError {
    NarouError::Login(message.into())
}

/// A portable set of login cookies.
///
/// Exactly one of `payload` (encrypted) and `cookies` (clear text) is filled;
/// `encrypted` says which one to expect.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CookieEnvelope {
    /// Format version; an unknown value is rejected on import.
    pub version: u32,
    /// RFC 3339 timestamp of the export.
    pub exported_at: String,
    /// Library the cookies came from, for the operator's benefit only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<String>,
    /// Whether `payload` holds an encrypted cookie map.
    pub encrypted: bool,
    /// Key derivation function of `payload`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf: Option<String>,
    /// Base64 Argon2id salt of `payload`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salt: Option<String>,
    /// Encrypted cookie map (`nonce:payload`, base64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    /// Clear-text logins per site, only for `encrypted: false`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sites: BTreeMap<String, Vec<LoginGroup>>,
    /// Version 2 clear-text credentials (one host each), read-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<LegacyCredential>,
    /// Version 1 clear-text cookie map (`host → cookie`), read-only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cookies: BTreeMap<String, String>,
}

/// Render `cookies` as an export document.
///
/// A passphrase encrypts the payload; without one the cookies are written in
/// clear (the CLI asks for confirmation before doing that).
pub fn build_export(
    sites: &BTreeMap<String, Vec<LoginGroup>>,
    passphrase: Option<&str>,
    exported_at: &str,
    library: Option<&str>,
) -> Result<String> {
    let envelope = match passphrase {
        Some(passphrase) => {
            let salt: [u8; SALT_LEN] = random_bytes()?;
            let key = derive_key(passphrase, &salt)?;
            let plaintext = serde_json::to_string(sites).map_err(|error| {
                login_error(format!("could not serialize the logins: {error}"))
            })?;
            let payload = encrypt_with_key(&key, ENVELOPE_AAD, &plaintext)?;
            CookieEnvelope {
                version: EXPORT_VERSION,
                exported_at: exported_at.to_string(),
                library: library.map(str::to_string),
                encrypted: true,
                kdf: Some(KDF_ARGON2ID.to_string()),
                salt: Some(base64_encode(&salt)),
                payload: Some(payload),
                sites: BTreeMap::new(),
                credentials: Vec::new(),
                cookies: BTreeMap::new(),
            }
        }
        None => CookieEnvelope {
            version: EXPORT_VERSION,
            exported_at: exported_at.to_string(),
            library: library.map(str::to_string),
            encrypted: false,
            kdf: None,
            salt: None,
            payload: None,
            sites: sites.clone(),
            credentials: Vec::new(),
            cookies: BTreeMap::new(),
        },
    };
    serde_yaml::to_string(&envelope).map_err(NarouError::from)
}

/// Read an export document back into credentials (trial order).
///
/// `passphrase` is required for an encrypted envelope and ignored otherwise.
/// A version 1 document (`host → cookie`) still imports, as one credential per
/// host, so an export taken before a site could hold several keeps working.
pub fn parse_export(
    text: &str,
    passphrase: Option<&str>,
) -> Result<BTreeMap<String, Vec<LoginGroup>>> {
    let envelope: CookieEnvelope = serde_yaml::from_str(text)?;
    if !matches!(
        envelope.version,
        EXPORT_VERSION | EXPORT_VERSION_PER_HOST | EXPORT_VERSION_LEGACY
    ) {
        return Err(login_error(format!(
            "unsupported export version {} (this build reads up to version {})",
            envelope.version, EXPORT_VERSION
        )));
    }
    if !envelope.encrypted {
        if envelope.version == EXPORT_VERSION_LEGACY {
            // version 1: ホスト → Cookie 1 本。1 回の取得がホストごとに分かれている。
            let groups = envelope
                .cookies
                .into_iter()
                .map(|(host, cookie)| {
                    LoginGroup::new(
                        host.clone(),
                        vec![crate::platform::HostCookie { host, cookie }],
                    )
                })
                .collect::<Vec<_>>();
            return Ok(fold_host_entries(groups));
        }
        if envelope.version == EXPORT_VERSION_PER_HOST {
            // 版 2 はホストごとの 1 本。同じ位置が同じアカウントなので、ホストごとに
            // 並べ直してから 1 ログインへ畳む。
            let groups = envelope
                .credentials
                .into_iter()
                .map(|credential| {
                    let host = credential.host.clone();
                    credential.into_group(&host)
                })
                .collect::<Vec<_>>();
            return Ok(fold_host_entries(groups));
        }
        return Ok(envelope.sites);
    }
    let passphrase = passphrase.ok_or_else(|| {
        login_error("this export is encrypted: pass a passphrase to import it")
    })?;
    let salt = envelope
        .salt
        .as_deref()
        .ok_or_else(|| login_error("encrypted export without a salt"))?;
    if let Some(kdf) = envelope.kdf.as_deref()
        && kdf != KDF_ARGON2ID
    {
        return Err(login_error(format!("unsupported key derivation: {kdf}")));
    }
    let payload = envelope
        .payload
        .as_deref()
        .ok_or_else(|| login_error("encrypted export without a payload"))?;
    let salt_bytes = base64_decode(salt)?;
    let key = derive_key(passphrase, &salt_bytes)?;
    let plaintext = decrypt_with_key(&key, ENVELOPE_AAD, payload)?;
    let value: serde_json::Value = serde_json::from_str(&plaintext).map_err(|error| {
        login_error(format!("decrypted payload is not a login list: {error}"))
    })?;
    if let Ok(sites) = serde_json::from_value::<BTreeMap<String, Vec<LoginGroup>>>(value.clone()) {
        return Ok(sites);
    }
    let credentials: Vec<LoginGroup> = serde_json::from_value(value)
        .map_err(|error| login_error(format!("decrypted payload is not a login list: {error}")))?;
    Ok(fold_host_entries(credentials))
}

/// Name what an import brought in.
///
/// One capture is one browser session per site, so a name given at import time
/// labels exactly that session (`本垢`). A counter is added only when one file
/// carries several sessions for the same site.
pub fn apply_import_name(sites: &mut BTreeMap<String, Vec<LoginGroup>>, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    for groups in sites.values_mut() {
        let total = groups.len();
        for (index, group) in groups.iter_mut().enumerate() {
            group.label = Some(if total == 1 {
                name.to_string()
            } else {
                format!("{name} {}", index + 1)
            });
        }
    }
}

/// Fold one capture's per-host entries into one login per site.
///
/// Versions 1 and 2 wrote one entry per host, so a single browser session
/// arrived as several entries. The same position in every host's list was the
/// same account, so folding per host gives the session back.
fn fold_host_entries(entries: Vec<LoginGroup>) -> BTreeMap<String, Vec<LoginGroup>> {
    let mut per_site: BTreeMap<String, BTreeMap<String, Vec<LoginGroup>>> = BTreeMap::new();
    for entry in entries {
        let host = entry
            .cookies
            .first()
            .map(|cookie| cookie.host.clone())
            .unwrap_or_else(|| entry.site.clone());
        let site = site_for_host(&host);
        per_site
            .entry(site)
            .or_default()
            .entry(host)
            .or_default()
            .push(entry);
    }
    per_site
        .into_iter()
        .map(|(site, by_host)| {
            let lists: Vec<Vec<LoginGroup>> = by_host.into_values().collect();
            let groups = crate::platform::fold_per_host_lists(lists, &site);
            (site, groups)
        })
        .collect()
}

/// Site a host belongs to (site definitions first, then its parent domain).
fn site_for_host(host: &str) -> String {
    crate::platform::site_for_host(host)
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|error| login_error(format!("malformed base64 value: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sites() -> BTreeMap<String, Vec<LoginGroup>> {
        BTreeMap::from([
            (
                "ncode.syosetu.com".to_string(),
                vec![LoginGroup::new(
                    "ncode.syosetu.com",
                    vec![crate::platform::HostCookie {
                        host: "ncode.syosetu.com".to_string(),
                        cookie: "over18=yes; ses=1".to_string(),
                    }],
                )],
            ),
            (
                "www.pixiv.net".to_string(),
                vec![
                    LoginGroup::new(
                        "www.pixiv.net",
                        vec![
                            crate::platform::HostCookie {
                                host: "pixiv.net".to_string(),
                                cookie: "PHPSESSID=abc".to_string(),
                            },
                            crate::platform::HostCookie {
                                host: "www.pixiv.net".to_string(),
                                cookie: "yuid_b=1".to_string(),
                            },
                        ],
                    )
                    .with_label(Some("Pixiv1".to_string())),
                ],
            ),
        ])
    }

    #[test]
    fn plain_envelope_round_trips() {
        let text = build_export(&sites(), None, "2026-09-20T00:00:00+09:00", Some("lib")).unwrap();
        assert!(text.contains("encrypted: false"));
        assert!(!text.contains("payload"));
        assert_eq!(parse_export(&text, None).unwrap(), sites());
    }

    #[test]
    fn encrypted_envelope_hides_the_logins_and_needs_the_passphrase() {
        let text = build_export(&sites(), Some("hunter2"), "2026-09-20T00:00:00+09:00", None)
            .unwrap();
        assert!(text.contains("encrypted: true"));
        assert!(text.contains("kdf: argon2id"));
        assert!(!text.contains("PHPSESSID=abc"));
        assert_eq!(parse_export(&text, Some("hunter2")).unwrap(), sites());
        assert!(parse_export(&text, Some("wrong")).is_err());
        assert!(parse_export(&text, None).is_err());
    }

    #[test]
    fn version_two_exports_are_folded_into_sites() {
        // 版 2 はホストごとの 1 本。取り込み時にサイトへまとめる
        // (定義があればそのドメイン、無ければ登録可能ドメイン)。
        let text = "version: 2\nexported_at: 2026-09-20T00:00:00+09:00\nencrypted: false\n\
                    credentials:\n- host: www.pixiv.net\n  cookie: yuid_b=1\n\
                    - host: pixiv.net\n  cookie: PHPSESSID=abc\n";
        // 1 回の取得がホストごとに分かれていたものを、サイト単位で 1 ログインに戻す。
        let sites = parse_export(text, None).unwrap();
        let (site, groups) = sites.iter().next().unwrap();
        assert_eq!(sites.len(), 1, "got {sites:?}");
        assert_eq!(site, "www.pixiv.net");
        assert_eq!(groups.len(), 1, "1 セッションは 1 ログイン: {groups:?}");
        let hosts = groups[0].hosts();
        assert_eq!(hosts, vec!["www.pixiv.net", "pixiv.net"]);
        assert_eq!(groups[0].merged_cookie(), "yuid_b=1; PHPSESSID=abc");
        assert!(groups[0].id.is_empty(), "識別子は取り込み側で振る");
    }

    #[test]
    fn version_one_cookies_fold_into_one_login() {
        let text = "version: 1\nexported_at: 2026-09-20T00:00:00+09:00\nencrypted: false\n\
                    cookies:\n  www.pixiv.net: yuid_b=1\n  pixiv.net: PHPSESSID=abc\n";
        let sites = parse_export(text, None).unwrap();
        let groups = &sites["www.pixiv.net"];
        assert_eq!(groups.len(), 1, "got {groups:?}");
        assert_eq!(groups[0].merged_cookie(), "yuid_b=1; PHPSESSID=abc");
    }

    #[test]
    fn two_accounts_stay_two_logins() {
        // ホストごとの一覧で 2 番目にいるものは別アカウント。まとめてはいけない。
        let text = "version: 2\nexported_at: 2026-09-20T00:00:00+09:00\nencrypted: false\n\
                    credentials:\n- host: www.pixiv.net\n  cookie: PHPSESSID=main\n\
                    - host: www.pixiv.net\n  cookie: PHPSESSID=alt\n\
                    - host: pixiv.net\n  cookie: PHPSESSID=main\n\
                    - host: pixiv.net\n  cookie: PHPSESSID=alt\n";
        let sites = parse_export(text, None).unwrap();
        let groups = &sites["www.pixiv.net"];
        assert_eq!(groups.len(), 2, "got {groups:?}");
        assert_eq!(groups[0].cookies.len(), 2);
        assert_eq!(groups[1].cookies.len(), 2);
        assert_eq!(groups[0].merged_cookie(), "PHPSESSID=main");
        assert_eq!(groups[1].merged_cookie(), "PHPSESSID=alt");
    }

    #[test]
    fn import_name_labels_what_one_file_brought_in() {
        let mut sites = BTreeMap::from([(
            "www.pixiv.net".to_string(),
            vec![LoginGroup::new("www.pixiv.net", Vec::new())],
        )]);
        apply_import_name(&mut sites, "  本垢  ");
        assert_eq!(sites["www.pixiv.net"][0].display_name(), "本垢");

        // 1 ファイルに複数のセッションがあれば番号を足す。
        let mut sites = BTreeMap::from([(
            "www.pixiv.net".to_string(),
            vec![
                LoginGroup::new("www.pixiv.net", Vec::new()),
                LoginGroup::new("www.pixiv.net", Vec::new()),
            ],
        )]);
        apply_import_name(&mut sites, "サブ垢");
        let names: Vec<&str> = sites["www.pixiv.net"]
            .iter()
            .map(LoginGroup::display_name)
            .collect();
        assert_eq!(names, vec!["サブ垢 1", "サブ垢 2"]);

        // 空の名前は何もしない。
        let mut sites = BTreeMap::from([(
            "www.pixiv.net".to_string(),
            vec![LoginGroup::new("www.pixiv.net", Vec::new())],
        )]);
        apply_import_name(&mut sites, "   ");
        assert_eq!(sites["www.pixiv.net"][0].display_name(), "www.pixiv.net");
    }

    #[test]
    fn rejects_unknown_versions_and_damaged_payloads() {
        let text = build_export(&sites(), Some("hunter2"), "2026-09-20T00:00:00+09:00", None)
            .unwrap();
        let bumped = text.replace("version: 3", "version: 99");
        assert!(parse_export(&bumped, Some("hunter2")).is_err());

        let damaged = text.replace("payload: ", "payload: AAAA");
        assert!(parse_export(&damaged, Some("hunter2")).is_err());
    }
}
