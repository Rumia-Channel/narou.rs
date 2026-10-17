use chrono::{DateTime, Duration, Utc};

use crate::platform::cookie_store::{format_cookie_header, parse_cookie_header};
use crate::platform::http::{HttpResponse, is_safe_header_name, is_safe_header_value};

const MAX_SESSION_COOKIES: usize = 128;
const MAX_COOKIE_BYTES: usize = 4096;

#[derive(Debug)]
struct SessionCookie {
    origin: String,
    name: String,
    value: String,
    path: String,
    secure: bool,
    expires: Option<DateTime<Utc>>,
}

/// Opt-in, short-lived cookies. Even Domain cookies stay on the exact origin.
#[derive(Debug, Default)]
pub(super) struct SessionCookies {
    cookies: Vec<SessionCookie>,
}

impl SessionCookies {
    pub(super) fn observe(&mut self, source: &str, response: &HttpResponse) {
        let Ok(url) = url::Url::parse(source) else { return };
        let Some(host) = url.host_str() else { return };
        let origin = url.origin().ascii_serialization();
        let now = Utc::now();
        self.cookies.retain(|cookie| cookie.expires.is_none_or(|expires| expires > now));
        for (_, header) in response.headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie")) {
            if header.len() > MAX_COOKIE_BYTES || !is_safe_header_value(header) { continue; }
            let mut parts = header.split(';');
            let Some((name, value)) = parts.next().and_then(|part| part.split_once('=')) else { continue };
            let (name, value) = (name.trim(), value.trim());
            if !is_safe_header_name(name) { continue; }
            let mut cookie = SessionCookie {
                origin: origin.clone(), name: name.to_string(), value: value.to_string(),
                path: url.path().rsplit_once('/').map(|(path, _)| path).filter(|path| !path.is_empty()).unwrap_or("/").to_string(),
                secure: false, expires: None,
            };
            let mut max_age = None;
            let mut valid_domain = true;
            for attribute in parts {
                let (key, value) = attribute.trim().split_once('=').unwrap_or((attribute.trim(), ""));
                match key.trim().to_ascii_lowercase().as_str() {
                    "path" if value.trim().starts_with('/') => cookie.path = value.trim().to_string(),
                    "secure" => cookie.secure = true,
                    "domain" => {
                        let domain = value.trim().trim_start_matches('.').to_ascii_lowercase();
                        valid_domain = !domain.is_empty() && (host == domain
                            || host.strip_suffix(&domain).is_some_and(|prefix| prefix.ends_with('.')));
                    }
                    "max-age" => max_age = value.trim().parse::<i64>().ok(),
                    "expires" => cookie.expires = DateTime::parse_from_rfc2822(value.trim()).ok().map(|date| date.with_timezone(&Utc)),
                    _ => {}
                }
            }
            if !valid_domain || (cookie.secure && url.scheme() != "https") { continue; }
            if let Some(seconds) = max_age {
                cookie.expires = Some(Duration::try_seconds(seconds.max(0))
                    .and_then(|duration| now.checked_add_signed(duration)).unwrap_or(now));
            }
            self.cookies.retain(|existing| !(existing.origin == cookie.origin && existing.name == cookie.name && existing.path == cookie.path));
            if !cookie.value.is_empty() && cookie.expires.is_none_or(|expires| expires > now)
                && self.cookies.len() < MAX_SESSION_COOKIES
            {
                self.cookies.push(cookie);
            }
        }
        self.cookies.sort_by_key(|cookie| cookie.path.len());
    }

    pub(super) fn apply(&self, target: &str, headers: &mut Vec<(String, String)>) {
        let Ok(url) = url::Url::parse(target) else { return };
        let origin = url.origin().ascii_serialization();
        let now = Utc::now();
        let matches = |cookie: &&SessionCookie| cookie.origin == origin
            && (!cookie.secure || url.scheme() == "https")
            && cookie.expires.is_none_or(|expires| expires > now)
            && (url.path() == cookie.path || (url.path().starts_with(&cookie.path)
                && (cookie.path.ends_with('/') || url.path().as_bytes().get(cookie.path.len()) == Some(&b'/'))));
        let mut cookies = self.cookies.iter().filter(matches).peekable();
        if cookies.peek().is_none() { return; }
        let mut pairs: Vec<_> = headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
            .flat_map(|(_, value)| parse_cookie_header(value)).collect();
        for cookie in cookies {
            if let Some(pair) = pairs.iter_mut().find(|(name, _)| *name == cookie.name) {
                pair.1.clone_from(&cookie.value);
            } else {
                pairs.push((cookie.name.clone(), cookie.value.clone()));
            }
        }
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("cookie"));
        headers.push(("Cookie".into(), format_cookie_header(&pairs)));
    }
}
