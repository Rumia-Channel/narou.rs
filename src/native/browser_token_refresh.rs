//! Native browser-token renewal.
//!
//! Some sites hand out a short-lived verification token through their own
//! browser SDK. When a download runs into that wall the token has to be
//! renewed by a real browser, which is what the sibling `narou_rs_login`
//! helper does in its machine mode (`--browser-token --token-json`).
//!
//! Nothing here decrypts or synthesises a token: the helper asks the site for
//! one and reports what the browser received. The reply is private (it carries
//! the token), so it is never logged and never appears in an error message —
//! only the site request that follows proves whether the token was accepted.
//!
//! The capability is opt-in: without `NAROU_RS_BROWSER_TOKEN` the store
//! answers `None` without starting a process or touching the disk.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};
use crate::native::cookie_store::InventoryCookieStore;
use crate::platform::cookie_store::{
    BROWSER_TOKEN_RESPONSE_VERSION, BrowserTokenResponse, HostCookie, LoginGroup,
    cookie_host_for_url, cookie_lookup_hosts, format_cookie_header, normalize_cookie_host,
    parse_cookie_header,
};

/// Environment variable that turns the renewal on.
pub(crate) const BROWSER_TOKEN_ENV: &str = "NAROU_RS_BROWSER_TOKEN";

/// Name of the sibling helper (the executable suffix is platform specific).
const HELPER_NAME: &str = "narou_rs_login";

/// Longest the helper may run.
const HELPER_TIMEOUT: Duration = Duration::from_secs(180);

/// Largest helper stdout that is read.
const MAX_STDOUT_BYTES: usize = 32 * 1024;


/// Longest a host name may be.
const MAX_HOST_LEN: usize = 253;

/// The helper executable is not next to this program.
const ERROR_HELPER_MISSING: &str =
    "ブラウザトークンを更新できませんでした (ヘルパーが見つかりません)";
/// The helper did not finish successfully.
const ERROR_HELPER_FAILED: &str =
    "ブラウザトークンを更新できませんでした (ブラウザの検証が完了しませんでした)";
/// The helper ran longer than it may.
const ERROR_TIMEOUT: &str = "ブラウザトークンを更新できませんでした (時間内に終わりませんでした)";
/// The reply was not a token response this build understands.
const ERROR_PROTOCOL: &str = "ブラウザトークンの応答が不正でした";
/// The reply was larger than it may be.
const ERROR_TOO_LARGE: &str = "ブラウザトークンの応答が大きすぎます";
/// The reply carried no token.
const ERROR_EMPTY: &str = "ブラウザトークンが発行されませんでした";
/// The reply was about another site or another cookie.
const ERROR_MISMATCH: &str = "ブラウザトークンの応答が対象と一致しません";
/// The reply carried a host the request does not cover.
const ERROR_SCOPE: &str = "ブラウザトークンの応答が対象ホストの範囲外です";
/// The login was removed while the helper ran.
const ERROR_REMOVED: &str = "ログイン情報が削除されたため、ブラウザトークンを更新できません";
/// The token was rotated while the helper ran.
const ERROR_ROTATED: &str = "別の操作でブラウザトークンが更新されたため、反映しませんでした";
/// The temporary browser profile could not be created.
const ERROR_PROFILE: &str =
    "ブラウザトークンを更新できませんでした (一時フォルダを作成できません)";

/// Whether the environment opted into the renewal.
pub(crate) fn opted_in() -> bool {
    opted_in_value(std::env::var(BROWSER_TOKEN_ENV).ok().as_deref())
}

/// The opt-in rule: `1`, `true`, `yes` or `on` in any case.
///
/// The value is read once, so a test never has to change the environment of
/// the whole process.
fn opted_in_value(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    ["1", "true", "yes", "on"].iter().any(|expected| value.trim().eq_ignore_ascii_case(expected))
}

/// Renew the browser token of a login and return the persisted login.
///
/// `session_id` names the login to renew. Without it the first login that
/// carries the token is renewed, and a token-only login is appended when no
/// login carries one yet.
pub(crate) fn refresh(
    store: &InventoryCookieStore,
    site: &str,
    target_url: &str,
    cookie_name: &str,
    session_id: Option<&str>,
) -> Result<Option<LoginGroup>> {
    let cookie_name = cookie_name.trim();
    if cookie_name.is_empty() {
        return Err(login_error(ERROR_PROTOCOL));
    }
    let Some(program) = helper_program() else {
        return Err(login_error(ERROR_HELPER_MISSING));
    };
    refresh_with(store, site, target_url, cookie_name, session_id, |target, profile| {
        run_helper(&program, target, profile)
    })
}

/// The renewal, with the helper invocation behind a function.
///
/// The tests replace the invocation with a canned reply, so the protocol and
/// the merge are exercised without a browser.
fn refresh_with(
    store: &InventoryCookieStore,
    site: &str,
    target_url: &str,
    cookie_name: &str,
    session_id: Option<&str>,
    run: impl FnOnce(&str, &Path) -> Result<Vec<u8>>,
) -> Result<Option<LoginGroup>> {
    // The helper needs a browser and takes time. What the logins carry now is
    // remembered, so a token that is rotated or removed meanwhile is not
    // overwritten by the older one.
    let (observed, selected_id) = {
        let groups = store.groups_for(site)?;
        let selected = session_id.map(str::to_owned).or_else(|| {
            groups.iter().find(|group| group.has_cookie(cookie_name)).map(|group| group.id.clone())
        });
        if selected.as_deref().is_some_and(|id| !groups.iter().any(|group| group.id == id)) {
            return Err(login_error(ERROR_REMOVED));
        }
        (TokenState::capture(&groups, cookie_name), selected)
    };
    // A profile of our own: the helper may be stopped at any time and the
    // directory still disappears with this scope.
    let profile = crate::native::browser_profile::BrowserProfile::new()
        .map_err(|_| login_error(ERROR_PROFILE))?;
    let payload = run(target_url, profile.path());
    profile.close().map_err(|_| login_error("ブラウザの一時プロファイルを削除できませんでした"))?;
    let payload = payload?;
    let tokens = parse_token_payload(&payload, site, target_url, cookie_name)?;
    let group = store.update_groups(|sites| {
        let group = merge_token(sites, site, cookie_name, selected_id.as_deref(), &tokens, &observed)?;
        Ok((group, true))
    })?;
    Ok(Some(group))
}

/// The helper executable next to the running program.
///
/// The helper is not looked up in `PATH`: only the one shipped with this build
/// may see the site's browser profile.
fn helper_program() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let directory = current.parent()?;
    let program = directory.join(format!("{HELPER_NAME}{}", std::env::consts::EXE_SUFFIX));
    program.is_file().then_some(program)
}

/// Run the helper in its machine mode and return its reply.
///
/// The helper is called as
/// `narou_rs_login --browser-token --token-json <target-url> --profile <dir>`:
/// the target is the URL that was refused, and the profile is a directory of
/// ours, so a helper that is stopped leaves nothing behind.
///
/// The helper is started in its own process group, so a timeout takes the
/// browser it launched down with it. stdout is bounded (a larger reply is
/// refused) and stderr is drained but never kept beyond its bound. Neither
/// stream reaches an error message.
fn run_helper(program: &Path, target_url: &str, profile: &Path) -> Result<Vec<u8>> {
    let mut command = Command::new(program);
    command
        .arg("--browser-token")
        .arg("--token-json")
        .arg(target_url)
        .arg("--profile")
        .arg(profile)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::compat::configure_hidden_console_command(&mut command);
    crate::compat::configure_process_group_command(&mut command);
    let mut child = command
        .spawn()
        .map_err(|_| login_error(ERROR_HELPER_MISSING))?;
    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| login_error(ERROR_PROTOCOL))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| login_error(ERROR_PROTOCOL))?;

    // stderr is only drained: its content is never used.
    let (stderr_done, stderr_finished) = mpsc::channel::<()>();
    let stderr_reader = thread::spawn(move || {
        let mut stderr = stderr;
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        let _ = stderr_done.send(());
    });

    // The watchdog ends the helper (and the browser it started) when it takes
    // too long, so a stuck browser never outlives the renewal.
    let timed_out = Arc::new(AtomicBool::new(false));
    let expired = timed_out.clone();
    let (done, waiting) = mpsc::channel::<()>();
    let watchdog = thread::spawn(move || {
        if waiting.recv_timeout(HELPER_TIMEOUT).is_err() {
            expired.store(true, Ordering::SeqCst);
            let _ = crate::compat::terminate_process(pid);
        }
    });

    let payload = read_bounded(stdout, MAX_STDOUT_BYTES);
    if !matches!(payload, Payload::Complete(_)) {
        // The helper is still writing (or the pipe broke): stop it instead of
        // waiting for a reply that is never accepted.
        let _ = crate::compat::terminate_process(pid);
    }
    let status = reap(&mut child);
    let _ = done.send(());
    let _ = watchdog.join();
    let _ = stderr_finished.recv_timeout(Duration::from_secs(5));
    drop(stderr_reader);

    let succeeded = status.is_some_and(|status| status.success());
    if timed_out.load(Ordering::SeqCst) && !succeeded {
        return Err(login_error(ERROR_TIMEOUT));
    }
    match payload {
        Payload::Complete(payload) => {
            if !succeeded {
                return Err(login_error(ERROR_HELPER_FAILED));
            }
            Ok(payload)
        }
        Payload::TooLarge => Err(login_error(ERROR_TOO_LARGE)),
        Payload::Failed => Err(login_error(ERROR_PROTOCOL)),
    }
}

/// What the helper wrote to stdout.
enum Payload {
    /// The reply, within the bound.
    Complete(Vec<u8>),
    /// The helper wrote more than the bound allows.
    TooLarge,
    /// The pipe broke.
    Failed,
}

/// Read a stream up to `limit` bytes.
///
/// A stream that carries more is refused instead of being buffered, so a
/// runaway helper cannot fill the memory of this process.
fn read_bounded(mut reader: impl Read, limit: usize) -> Payload {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Payload::Complete(buffer),
            Ok(length) => {
                if buffer.len().saturating_add(length) > limit {
                    return Payload::TooLarge;
                }
                buffer.extend_from_slice(&chunk[..length]);
            }
            Err(_) => return Payload::Failed,
        }
    }
}

/// Wait for the helper to end, killing it when it overstays.
fn reap(child: &mut Child) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Check the helper's reply and return the token it carries.
///
/// The reply is accepted only when it is this build's protocol, names the
/// requested site and cookie, and carries nothing but the token for the hosts
/// the request covers.
pub(crate) fn parse_token_payload(
    payload: &[u8],
    site: &str,
    target_url: &str,
    cookie_name: &str,
) -> Result<BTreeMap<String, String>> {
    if payload.len() > MAX_STDOUT_BYTES {
        return Err(login_error(ERROR_TOO_LARGE));
    }
    let text = std::str::from_utf8(payload).map_err(|_| login_error(ERROR_PROTOCOL))?;
    let text = text.trim();
    if text.is_empty() {
        return Err(login_error(ERROR_EMPTY));
    }
    let response: BrowserTokenResponse = serde_json::from_str(text)
        .map_err(|_| login_error(ERROR_PROTOCOL))?;
    if response.version != BROWSER_TOKEN_RESPONSE_VERSION {
        return Err(login_error(ERROR_PROTOCOL));
    }
    if !site.eq_ignore_ascii_case(&response.site) {
        return Err(login_error(ERROR_MISMATCH));
    }
    if response.cookie_name != cookie_name {
        return Err(login_error(ERROR_MISMATCH));
    }
    if response.cookies.is_empty() {
        return Err(login_error(ERROR_EMPTY));
    }
    let Some(target_host) = cookie_host_for_url(target_url) else {
        return Err(login_error(ERROR_SCOPE));
    };
    let allowed = cookie_lookup_hosts(&target_host);
    let mut tokens = BTreeMap::new();
    for (host, mut cookie) in response.cookies {
        let host = normalize_cookie_host(&host);
        if host.is_empty() || host.len() > MAX_HOST_LEN || host.contains(char::is_whitespace) {
            return Err(login_error(ERROR_SCOPE));
        }
        if !allowed.contains(&host) {
            return Err(login_error(ERROR_SCOPE));
        }
        let value = token_value(&cookie, cookie_name).ok_or_else(|| login_error(ERROR_PROTOCOL))?;
        let end = cookie.trim_end().len();
        let start = end - value.len();
        cookie.truncate(end);
        drop(cookie.drain(..start));
        tokens.insert(host, cookie);
    }
    Ok(tokens)
}


/// The value of a cookie that carries nothing but the token.
fn token_value<'a>(cookie: &'a str, cookie_name: &str) -> Option<&'a str> {
    let trimmed = cookie.trim();
    if trimmed.is_empty() || trimmed.contains(';') {
        return None;
    }
    let (name, value) = trimmed.split_once('=')?;
    if name != cookie_name {
        return None;
    }
    let value = value.trim();
    if value.is_empty() || value.chars().any(|character| character.is_control()) {
        return None;
    }
    Some(value)
}


/// The token entries of a site's logins, as they were before the helper ran.
///
/// A login that is rotated or removed while the helper works must keep its
/// newer state, so the tokens are captured first and compared again inside
/// the atomic update.
#[derive(Clone, Debug, Default)]
pub(crate) struct TokenState {
    /// Login key → digest of the token entries it carried.
    entries: Vec<(String, Option<[u8; 32]>)>,
}

impl TokenState {
    /// Capture the token entries of every login of one site.
    pub(crate) fn capture(groups: &[LoginGroup], cookie_name: &str) -> Self {
        let entries = groups
            .iter()
            .enumerate()
            .map(|(index, group)| (state_key(group, index), token_digest(group, cookie_name)))
            .collect();
        Self { entries }
    }

    /// The digest captured for one login, if it was captured at all.
    fn digest_of(&self, key: &str) -> Option<[u8; 32]> {
        self.entries
            .iter()
            .find(|(captured, _)| captured == key)
            .and_then(|(_, digest)| *digest)
    }
}

/// The key a login is tracked by: its id, or its position when it has none.
fn state_key(group: &LoginGroup, index: usize) -> String {
    if group.id.is_empty() {
        format!("\u{1}{index}")
    } else {
        group.id.clone()
    }
}

/// A digest of the token entries of one login.
///
/// The digest keeps a second copy of the token out of the comparison. A login
/// without the token has no digest, so "no token" and "a token" never look
/// alike.
fn token_digest(group: &LoginGroup, cookie_name: &str) -> Option<[u8; 32]> {
    let mut entries: Vec<(&str, &str)> = group.cookies.iter().flat_map(|entry| {
        token_values(&entry.cookie, cookie_name).map(|value| (entry.host.as_str(), value))
    }).collect();
    if entries.is_empty() {
        return None;
    }
    entries.sort_unstable();
    let mut hasher = Sha256::new();
    for (host, value) in entries {
        hasher.update(host.as_bytes());
        hasher.update([0u8]);
        hasher.update(value.as_bytes());
        hasher.update([0u8]);
    }
    Some(hasher.finalize().into())
}


fn token_values<'a>(header: &'a str, cookie_name: &'a str) -> impl Iterator<Item = &'a str> {
    header.split(';').filter_map(move |part| {
        let (name, value) = part.trim().split_once('=')?;
        (name.trim() == cookie_name).then_some(value.trim())
    })
}

/// Merge the fresh token into the stored logins and return the saved login.
///
/// Only the token changes: the login keeps its id, name, capture time and
/// every other cookie, and the other logins and sites are untouched. The
/// merge happens inside the store's atomic update, so it sees the newest
/// state and a login that was removed meanwhile is not re-created.
pub(crate) fn merge_token(
    sites: &mut BTreeMap<String, Vec<LoginGroup>>,
    site: &str,
    cookie_name: &str,
    session_id: Option<&str>,
    tokens: &BTreeMap<String, String>,
    observed: &TokenState,
) -> Result<LoginGroup> {
    let key = site.trim().to_ascii_lowercase();
    let groups = sites.entry(key.clone()).or_default();

    let target = match session_id {
        Some(id) => match groups.iter().position(|group| group.id == id) {
            Some(index) => index,
            // The login was removed while the helper ran: it must not come back.
            None => return Err(login_error(ERROR_REMOVED)),
        },
        None => match groups.iter().position(|group| group.has_cookie(cookie_name)) {
            Some(index) => index,
            None => {
                // No login carries the token yet: one is added for it alone.
                groups.push(LoginGroup::new(key.clone(), Vec::new()));
                groups.len() - 1
            }
        },
    };

    // A token that changed while the helper ran is newer than this one.
    let current = token_digest(&groups[target], cookie_name);
    let expected = observed.digest_of(&state_key(&groups[target], target));
    if current != expected {
        return Err(login_error(ERROR_ROTATED));
    }

    let group = &mut groups[target];
    // Every instance of the token name goes: a token of a specific host would
    // shadow the fresh token of the parent domain.
    group.cookies.retain_mut(|entry| {
        let fresh = tokens.get(&entry.host);
        if fresh.is_none() && token_values(&entry.cookie, cookie_name).next().is_none() {
            return true;
        }
        let mut pairs = parse_cookie_header(&entry.cookie);
        pairs.retain(|(name, _)| name != cookie_name);
        if let Some(value) = fresh {
            pairs.push((cookie_name.to_string(), value.clone()));
        }
        entry.cookie = format_cookie_header(&pairs);
        !entry.cookie.is_empty()
    });
    // The fresh token applies to every host the reply named.
    for (host, value) in tokens {
        if !group.cookies.iter().any(|entry| entry.host == *host) {
            group.cookies.push(HostCookie {
                host: host.clone(),
                cookie: format!("{cookie_name}={value}"),
            });
        }
    }
    if group.id.is_empty() {
        group.id = crate::login::new_credential_id()
            .map_err(|_| login_error("ログイン識別子を発行できませんでした"))?;
    }

    Ok(group.clone())
}

/// A login error with a fixed message.
///
/// The helper's output and the token itself never reach the user.
fn login_error(message: &str) -> NarouError {
    NarouError::Login(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::CookieStore;
    use crate::test_support::{legacy_yaml_guard, set_current_dir_for_test};

    const SITE: &str = "novelup.plus";
    const COOKIE: &str = "aws-waf-token";

    fn store_in(temp: &tempfile::TempDir) -> InventoryCookieStore {
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        InventoryCookieStore::for_current_root().unwrap()
    }

    fn group(id: &str, cookies: &[(&str, &str)]) -> LoginGroup {
        LoginGroup::new(
            SITE,
            cookies
                .iter()
                .map(|(host, cookie)| HostCookie {
                    host: (*host).to_string(),
                    cookie: (*cookie).to_string(),
                })
                .collect(),
        )
        .with_id(id)
    }

    /// A helper reply as the machine mode writes it.
    fn payload(site: &str, cookie_name: &str, cookies: &[(&str, &str)]) -> Vec<u8> {
        let response = BrowserTokenResponse {
            version: BROWSER_TOKEN_RESPONSE_VERSION,
            site: site.to_string(),
            cookie_name: cookie_name.to_string(),
            cookies: cookies
                .iter()
                .map(|(host, cookie)| ((*host).to_string(), (*cookie).to_string()))
                .collect(),
        };
        serde_json::to_vec(&response).unwrap()
    }

    /// The renewal with a canned helper reply.
    fn refresh_with_payload(
        store: &InventoryCookieStore,
        site: &str,
        session_id: Option<&str>,
        payload: Vec<u8>,
    ) -> Result<Option<LoginGroup>> {
        refresh_with(store, site, "https://novelup.plus/story/1", COOKIE, session_id, |_, _| {
            Ok(payload)
        })
    }

    #[test]
    fn the_opt_in_names_the_allowed_values() {
        for value in ["1", "true", "TRUE", "yes", "on", " on "] {
            assert!(opted_in_value(Some(value)), "{value} should opt in");
        }
        for value in ["0", "false", "no", "off", "", "2"] {
            assert!(!opted_in_value(Some(value)), "{value} should not opt in");
        }
        assert!(!opted_in_value(None));
    }

    #[test]
    fn a_reply_for_another_site_or_cookie_is_refused() {
        assert!(parse_token_payload(
            &payload("other.example", COOKIE, &[("novelup.plus", "aws-waf-token=x")]),
            SITE,
            "https://novelup.plus/story/1",
            COOKIE,
        )
        .is_err());
        assert!(parse_token_payload(
            &payload(SITE, "other-token", &[("novelup.plus", "other-token=x")]),
            SITE,
            "https://novelup.plus/story/1",
            COOKIE,
        )
        .is_err());
    }

    #[test]
    fn a_reply_for_another_host_is_refused() {
        assert!(parse_token_payload(
            &payload(SITE, COOKIE, &[("evil.example", "aws-waf-token=x")]),
            SITE,
            "https://novelup.plus/story/1",
            COOKIE,
        )
        .is_err());
    }

    #[test]
    fn a_reply_that_carries_more_than_the_token_is_refused() {
        assert!(parse_token_payload(
            &payload(SITE, COOKIE, &[("novelup.plus", "aws-waf-token=x; sid=1")]),
            SITE,
            "https://novelup.plus/story/1",
            COOKIE,
        )
        .is_err());
        assert!(parse_token_payload(
            &payload(SITE, COOKIE, &[("novelup.plus", "sid=1")]),
            SITE,
            "https://novelup.plus/story/1",
            COOKIE,
        )
        .is_err());
        assert!(parse_token_payload(
            &payload(SITE, COOKIE, &[("novelup.plus", "aws-waf-token=")]),
            SITE,
            "https://novelup.plus/story/1",
            COOKIE,
        )
        .is_err());
    }

    #[test]
    fn a_malformed_or_oversized_reply_is_refused() {
        assert!(parse_token_payload(
            b"not json",
            SITE,
            "https://novelup.plus/story/1",
            COOKIE,
        )
        .is_err());
        assert!(parse_token_payload(
            b"",
            SITE,
            "https://novelup.plus/story/1",
            COOKIE,
        )
        .is_err());
        let oversized = vec![b'x'; MAX_STDOUT_BYTES + 1];
        assert!(parse_token_payload(
            &oversized,
            SITE,
            "https://novelup.plus/story/1",
            COOKIE,
        )
        .is_err());
    }

    #[test]
    fn a_non_json_prefix_invalidates_the_private_reply() {
        let mut reply = b"browser opened\n".to_vec();
        reply.extend_from_slice(&payload(SITE, COOKIE, &[("novelup.plus", "aws-waf-token=x")]));
        assert!(parse_token_payload(&reply, SITE, "https://novelup.plus/story/1", COOKIE).is_err());
    }

    #[test]
    fn a_reply_of_another_protocol_version_is_refused() {
        let mut response: serde_json::Value =
            serde_json::from_slice(&payload(SITE, COOKIE, &[("novelup.plus", "aws-waf-token=x")]))
                .unwrap();
        response["version"] = serde_json::json!(BROWSER_TOKEN_RESPONSE_VERSION + 1);
        assert!(
            parse_token_payload(
                serde_json::to_vec(&response).unwrap().as_slice(),
                SITE,
                "https://novelup.plus/story/1",
                COOKIE,
            )
            .is_err()
        );
    }

    #[test]
    fn a_stream_longer_than_the_bound_is_refused() {
        let reader = std::io::Cursor::new(vec![b'x'; 64]);
        assert!(matches!(read_bounded(reader, 32), Payload::TooLarge));
        let reader = std::io::Cursor::new(vec![b'x'; 16]);
        assert!(matches!(read_bounded(reader, 32), Payload::Complete(_)));
    }

    #[test]
    fn the_token_replaces_only_the_token_of_the_chosen_login() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        let account = group(
            "g-account",
            &[
                ("novelup.plus", "sid=abc; aws-waf-token=OLD"),
                ("cdn.novelup.plus", "cdn=1"),
            ],
        )
        .with_label(Some("メイン".into()))
        .with_added_at(Some("2026-01-02T03:04:05+09:00".into()));
        let other = group("g-other", &[("novelup.plus", "other=1")]);
        store
            .save_groups_for(SITE, &[account.clone(), other.clone()])
            .unwrap();

        let refreshed = refresh_with_payload(
            &store,
            SITE,
            Some("g-account"),
            payload(SITE, COOKIE, &[("novelup.plus", "aws-waf-token=NEW")]),
        )
        .unwrap()
        .unwrap();

        assert_eq!(refreshed.id, "g-account", "識別子はそのまま");
        assert_eq!(refreshed.label.as_deref(), Some("メイン"), "名前もそのまま");
        assert_eq!(
            refreshed.added_at.as_deref(),
            Some("2026-01-02T03:04:05+09:00"),
            "取得時刻もそのまま"
        );
        let stored = store.groups_for(SITE).unwrap();
        assert_eq!(stored.len(), 2, "ログインは増えない");
        assert_eq!(stored[0].id, "g-account", "順序はそのまま");
        assert_eq!(stored[1].id, "g-other", "順序はそのまま");
        assert_eq!(stored[1].cookies, other.cookies, "他のログインは変わらない");
        assert_eq!(
            stored[0].merged_cookie(),
            "cdn=1; sid=abc; aws-waf-token=NEW",
            "アカウントの Cookie は残り、トークンだけが入れ替わる"
        );
    }

    #[test]
    fn a_stale_token_of_a_specific_host_does_not_shadow_the_new_one() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        store
            .save_groups_for(
                SITE,
                &[group(
                    "g-account",
                    &[
                        ("www.novelup.plus", "aws-waf-token=OLD"),
                        ("novelup.plus", "sid=abc"),
                    ],
                )],
            )
            .unwrap();

        let refreshed = refresh_with_payload(
            &store,
            SITE,
            Some("g-account"),
            payload(SITE, COOKIE, &[("novelup.plus", "aws-waf-token=NEW")]),
        )
        .unwrap()
        .unwrap();

        let merged = refreshed.merged_cookie();
        assert!(
            !merged.contains("OLD"),
            "古いトークンは残さない: {merged}"
        );
        assert!(
            merged.contains("aws-waf-token=NEW"),
            "新しいトークンが送られる: {merged}"
        );
        assert!(merged.contains("sid=abc"), "他の Cookie は残る: {merged}");
    }

    #[test]
    fn a_login_without_the_token_is_added_once() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        store
            .save_groups_for(SITE, &[group("g-account", &[("novelup.plus", "sid=abc")])])
            .unwrap();

        refresh_with_payload(
            &store,
            SITE,
            None,
            payload(SITE, COOKIE, &[("novelup.plus", "aws-waf-token=NEW")]),
        )
        .unwrap()
        .unwrap();
        let groups = store.groups_for(SITE).unwrap();
        assert_eq!(groups.len(), 2, "トークンだけのログインが加わる");
        assert!(groups[1].has_cookie(COOKIE), "トークンが保存される");

        // A second renewal must not add another login.
        refresh_with_payload(
            &store,
            SITE,
            None,
            payload(SITE, COOKIE, &[("novelup.plus", "aws-waf-token=NEWER")]),
        )
        .unwrap()
        .unwrap();
        let groups = store.groups_for(SITE).unwrap();
        assert_eq!(groups.len(), 2, "更新のたびにログインは増えない");
        assert_eq!(
            groups[1].merged_cookie(),
            "aws-waf-token=NEWER",
            "同じログインが更新される"
        );
    }

    #[test]
    fn a_removed_login_is_not_re_created() {
        let _legacy = legacy_yaml_guard();
        for (session_id, cookie) in [
            (Some("g-account"), "sid=abc"),
            (None, "sid=abc; aws-waf-token=OLD"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let _guard = set_current_dir_for_test(temp.path());
            let store = store_in(&temp);
            store.save_groups_for(SITE, &[group("g-account", &[("novelup.plus", cookie)])]).unwrap();
            let outcome = refresh_with(&store, SITE, "https://novelup.plus/story/1", COOKIE, session_id, |_, _| {
                store.remove(SITE).unwrap();
                Ok(payload(SITE, COOKIE, &[("novelup.plus", "aws-waf-token=NEW")]))
            });
            assert!(matches!(outcome, Err(NarouError::Login(_))), "削除されたログインは戻さない");
            assert!(store.groups_for(SITE).unwrap().is_empty(), "削除はそのまま");
        }
    }

    #[test]
    fn a_token_that_was_rotated_meanwhile_is_not_overwritten() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        store
            .save_groups_for(
                SITE,
                &[group(
                    "g-account",
                    &[("novelup.plus", "sid=abc; aws-waf-token=OLD")],
                )],
            )
            .unwrap();

        let outcome = refresh_with(&store, SITE, "https://novelup.plus/story/1", COOKIE, Some("g-account"), |_, _| {
            // Another renewal replaces the token while the helper runs.
            store
                .save_groups_for(
                    SITE,
                    &[group(
                        "g-account",
                        &[("novelup.plus", "sid=abc; aws-waf-token=NEWER")],
                    )],
                )
                .unwrap();
            Ok(payload(SITE, COOKIE, &[("novelup.plus", "aws-waf-token=OLD")]))
        });

        assert!(
            matches!(outcome, Err(NarouError::Login(_))),
            "回転後のトークンは上書きしない"
        );
        let groups = store.groups_for(SITE).unwrap();
        assert_eq!(
            groups[0].merged_cookie(),
            "sid=abc; aws-waf-token=NEWER",
            "新しいトークンが残る"
        );
    }

    #[test]
    fn the_capability_stays_off_without_the_opt_in() {
        if std::env::var(BROWSER_TOKEN_ENV).is_ok() {
            return;
        }
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        let outcome = futures::executor::block_on(store.refresh_browser_token(
            SITE,
            "https://novelup.plus/story/1",
            COOKIE,
            None,
        ))
        .unwrap();
        assert!(outcome.is_none(), "許可されていない環境では何もしない");
    }
}
