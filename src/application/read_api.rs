//! Read-only browser integration uses the existing UI session, never a new token.

pub const PREFIX: &str = "/api/read/v1";
pub const ALLOWED_ORIGINS_SETTING: &str = "external-api.allowed-origins";
pub const EXPOSE_HEADERS: &str =
    "Content-Length, Content-Range, Accept-Ranges, ETag, Last-Modified";

pub fn is_read_path(path: &str) -> bool {
    path == PREFIX
        || path
            .strip_prefix(PREFIX)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn origin(value: &str) -> Option<url::Origin> {
    let url = url::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url.origin())
}

pub fn origin_allowed(configured: &str, presented: &str) -> bool {
    let Some(presented_origin) = origin(presented) else {
        return false;
    };
    if presented_origin.ascii_serialization() != presented {
        return false;
    }
    configured
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .any(|value| origin(value).is_some_and(|allowed| allowed == presented_origin))
}

pub fn preflight_allowed(method: Option<&str>, headers: Option<&str>) -> bool {
    matches!(method, Some("GET" | "HEAD"))
        && headers.is_none_or(|headers| {
            headers
                .split(',')
                .all(|header| header.trim().eq_ignore_ascii_case("range"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_require_an_explicit_exact_origin() {
        let configured = "https://books.example:443, http://127.0.0.1:3000";
        assert!(origin_allowed(configured, "https://books.example"));
        assert!(origin_allowed(configured, "http://127.0.0.1:3000"));
        for denied in [
            "https://books.example.attacker.test",
            "http://books.example",
            "http://127.0.0.1:3001",
            "null",
            "https://user@books.example",
            "https://@books.example",
            "https://books.example/",
            "https://books.example/path",
        ] {
            assert!(!origin_allowed(configured, denied), "{denied}");
        }
        assert!(!origin_allowed("", "https://books.example"));
        assert!(!origin_allowed("*", "https://books.example"));
    }

    #[test]
    fn preflight_never_authorizes_writes_or_credential_headers() {
        assert!(preflight_allowed(Some("GET"), Some("Range")));
        assert!(preflight_allowed(Some("HEAD"), None));
        assert!(!preflight_allowed(Some("POST"), None));
        assert!(!preflight_allowed(Some("GET"), Some("Authorization")));
        assert!(!preflight_allowed(
            Some("GET"),
            Some("Range, CF-Access-Client-Secret")
        ));
    }
}
