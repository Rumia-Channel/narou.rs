//! Browser-side token preparation for sites whose content sits behind a
//! browser check.
//!
//! Some sites answer a request they do not trust with a challenge page that
//! only a real browser can pass. A site definition can then describe how the
//! browser obtains the token: which script to load, which function to call
//! and which cookie carries the result. The values are site data, so they live
//! in the YAML definition and no site is special-cased in Rust.

use serde::de;
use serde::{Deserialize, Serialize};

use crate::platform::http::{MAX_HEADER_NAME_BYTES, is_safe_header_name};

/// How a site obtains its browser token.
///
/// The configuration is checked while the definition is read, so an unusable
/// one is rejected with an ordinary site definition error instead of reaching
/// the browser helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserTokenSetting {
    /// URL of the script that issues the token.
    pub script_url: String,
    /// Dotted path of the JavaScript function that issues the token.
    pub function: String,
    /// Name of the cookie that carries the issued token.
    pub cookie_name: String,
}

impl BrowserTokenSetting {
    /// Check that the configuration can drive a browser.
    ///
    /// The messages name the offending field only; the configured values are
    /// never repeated, so an error can be logged without leaking the script
    /// URL.
    pub fn validate(&self) -> Result<(), String> {
        validate_script_url(&self.script_url)?;
        validate_function_path(&self.function)?;
        validate_cookie_name(&self.cookie_name)?;
        Ok(())
    }
}

/// The script must be an absolute HTTPS URL without credentials and without a
/// fragment, so the browser loads exactly the published script.
fn validate_script_url(script_url: &str) -> Result<(), String> {
    let parsed =
        url::Url::parse(script_url).map_err(|_| "browser_token.script_url must be a URL".to_string())?;
    if parsed.scheme() != "https" {
        return Err("browser_token.script_url must use the https scheme".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("browser_token.script_url must not contain credentials".to_string());
    }
    if parsed.fragment().is_some() {
        return Err("browser_token.script_url must not contain a fragment".to_string());
    }
    Ok(())
}

/// The function is called by name, so it must be a dotted path of JavaScript
/// identifiers: each part starts with a letter, `_` or `$` and continues with
/// letters, digits, `_` or `$`.
fn validate_function_path(function: &str) -> Result<(), String> {
    const INVALID: &str = "browser_token.function must be a dotted path of JavaScript identifiers";
    for part in function.split('.') {
        let mut characters = part.chars();
        let Some(first) = characters.next() else {
            return Err(INVALID.to_string());
        };
        if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
            return Err(INVALID.to_string());
        }
        if !characters.all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '$'
        }) {
            return Err(INVALID.to_string());
        }
    }
    Ok(())
}

/// The cookie name travels in a `Cookie` header, so it must be an HTTP token.
fn validate_cookie_name(cookie_name: &str) -> Result<(), String> {
    if !is_safe_header_name(cookie_name) {
        return Err(format!(
            "browser_token.cookie_name must be a non-empty HTTP token of at most {MAX_HEADER_NAME_BYTES} bytes"
        ));
    }
    Ok(())
}

/// Read the optional browser token settings and reject an unusable one.
///
/// The check runs while the definition is read, so an invalid configuration
/// fails the ordinary site definition load rather than being used later.
pub(super) fn deserialize_browser_token<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<BrowserTokenSetting>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let settings = Option::<BrowserTokenSetting>::deserialize(deserializer)?;
    if let Some(settings) = &settings {
        settings.validate().map_err(de::Error::custom)?;
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(script_url: &str, function: &str, cookie_name: &str) -> BrowserTokenSetting {
        BrowserTokenSetting {
            script_url: script_url.to_string(),
            function: function.to_string(),
            cookie_name: cookie_name.to_string(),
        }
    }


    #[test]
    fn only_https_scripts_without_credentials_or_fragments_are_accepted() {
        for script_url in [
            "http://example.com/challenge.js",
            "https://user:secret@example.com/challenge.js",
            "https://example.com/challenge.js#token",
            "example.com/challenge.js",
        ] {
            assert!(
                settings(script_url, "Vendor.getToken", "vendor-token")
                    .validate()
                    .is_err(),
                "{script_url} must be rejected"
            );
        }
    }

    #[test]
    fn only_dotted_identifier_paths_are_accepted() {
        for function in [
            "",
            "Vendor.",
            ".getToken",
            "Vendor..getToken",
            "Vendor.get Token",
            "Vendor.get-token",
            "1Vendor.getToken",
        ] {
            assert!(
                settings(
                    "https://example.com/challenge.js",
                    function,
                    "vendor-token"
                )
                .validate()
                .is_err(),
                "{function} must be rejected"
            );
        }
        assert!(
            settings(
                "https://example.com/challenge.js",
                "$vendor._get$Token1",
                "vendor-token"
            )
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn only_http_token_cookie_names_are_accepted() {
        for cookie_name in ["", "vendor token", "vendor;token", "vendor=token"] {
            assert!(
                settings(
                    "https://example.com/challenge.js",
                    "Vendor.getToken",
                    cookie_name
                )
                .validate()
                .is_err(),
                "{cookie_name} must be rejected"
            );
        }
    }

    #[test]
    fn a_rejected_configuration_does_not_repeat_the_configured_values() {
        let error = settings(
            "https://user:secret@example.com/challenge.js",
            "Vendor.getToken",
            "vendor-token",
        )
        .validate()
        .expect_err("credentials must be rejected");
        assert!(!error.contains("secret"));
        assert!(!error.contains("example.com"));
    }

    #[test]
    fn invalid_browser_settings_fail_at_the_user_yaml_boundary_without_leaking_values() {
        let yaml = r#"
name: Example
domain: example.test
top_url: https://example.test
sitename: Example
toc_url: https://example.test/read
browser_token:
  script_url: https://user:private-secret@example.test/sdk.js
  function: Vendor.getToken
  cookie_name: vendor-token
"#;
        let error = serde_yaml::from_str::<super::super::SiteSetting>(yaml)
            .expect_err("credential-bearing SDK URLs must not reach the browser");
        let message = error.to_string();
        assert!(message.contains("browser_token.script_url"), "{message}");
        assert!(!message.contains("private-secret"), "{message}");
    }
}
