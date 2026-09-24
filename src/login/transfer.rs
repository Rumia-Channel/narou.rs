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
            // version 1: ホスト → Cookie 1 本。サイトはホストのまま入れ、取り込み側で
            // 定義に照らしてまとめる。
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
            return Ok(group_by_site(groups));
        }
        if envelope.version == EXPORT_VERSION_PER_HOST {
            // 版 2 はホストごとの 1 本。同じ利用者のホストはサイト単位で 1 本に戻す。
            let groups = envelope
                .credentials
                .into_iter()
                .map(|credential| {
                    let host = credential.host.clone();
                    credential.into_group(&host)
                })
                .collect::<Vec<_>>();
            return Ok(group_by_site(groups));
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
    Ok(group_by_site(credentials))
}

/// Fold a flat list of logins into per-site groups.
///
/// A login written per host (`site` = host) is grouped under the site
/// definition that host belongs to, so `pixiv.net` and `www.pixiv.net` end up
/// in one place.
fn group_by_site(groups: Vec<LoginGroup>) -> BTreeMap<String, Vec<LoginGroup>> {
    let mut sites: BTreeMap<String, Vec<LoginGroup>> = BTreeMap::new();
    for group in groups {
        let site = site_for_host(&group.site);
        sites.entry(site).or_default().push(group);
    }
    sites
}

/// Site a host belongs to, through the site definitions (falling back to the
/// registrable domain).
fn site_for_host(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    if let Ok(settings) = crate::downloader::site_setting::SiteSetting::load_all()
        && let Some(setting) = settings.iter().find(|setting| {
            let domain = setting.domain.to_ascii_lowercase();
            domain == host || host.ends_with(&format!(".{domain}"))
        })
    {
        return setting.domain.to_ascii_lowercase();
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() > 2 {
        labels[labels.len() - 2..].join(".")
    } else {
        host
    }
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
        // サイト定義が読めない環境では、まとめ先が分からないのでホストのまま残す。
        let sites = parse_export(text, None).unwrap();
        assert_eq!(sites.len(), 2, "got {sites:?}");
        let group = &sites["www.pixiv.net"][0];
        assert_eq!(group.cookies[0].cookie, "yuid_b=1");
        assert_eq!(sites["pixiv.net"][0].cookies[0].cookie, "PHPSESSID=abc");
        assert!(sites["www.pixiv.net"][0].id.is_empty(), "識別子は取り込み側で振る");
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
