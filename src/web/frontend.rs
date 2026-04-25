use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use std::borrow::Cow;

include!(concat!(env!("OUT_DIR"), "/web_asset_versions.rs"));

fn render_page(source: &'static str) -> Html<String> {
    Html(apply_asset_versions(source))
}

fn apply_asset_versions(source: &'static str) -> String {
    let mut rendered = source.to_string();
    for path in ASSET_PATHS {
        let Some(version) = asset_version(path) else {
            continue;
        };
        let asset_path = format!("/assets/{path}");
        let versioned_path = format!("{asset_path}?v={version}");
        rendered = rendered.replace(&asset_path, &versioned_path);
    }
    rendered
}

fn apply_js_module_versions(module_path: &str, source: &'static str) -> String {
    let mut rendered = source.to_string();
    for path in ASSET_PATHS {
        if !path.ends_with(".js") {
            continue;
        }
        let Some(version) = asset_version(path) else {
            continue;
        };
        let specifier = relative_module_specifier(module_path, path);
        let versioned = format!("{specifier}?v={version}");
        rendered = rendered
            .replace(
                &format!("from '{specifier}'"),
                &format!("from '{versioned}'"),
            )
            .replace(
                &format!("from \"{specifier}\""),
                &format!("from \"{versioned}\""),
            )
            .replace(
                &format!("import('{specifier}')"),
                &format!("import('{versioned}')"),
            )
            .replace(
                &format!("import(\"{specifier}\")"),
                &format!("import(\"{versioned}\")"),
            )
            .replace(
                &format!("import '{specifier}'"),
                &format!("import '{versioned}'"),
            )
            .replace(
                &format!("import \"{specifier}\""),
                &format!("import \"{versioned}\""),
            );
    }
    rendered
}

fn relative_module_specifier(from_asset_path: &str, to_asset_path: &str) -> String {
    let mut from_parts: Vec<&str> = from_asset_path.split('/').collect();
    from_parts.pop();
    let to_parts: Vec<&str> = to_asset_path.split('/').collect();

    let mut common = 0usize;
    while common < from_parts.len()
        && common < to_parts.len()
        && from_parts[common] == to_parts[common]
    {
        common += 1;
    }

    let mut rel_parts = Vec::new();
    for _ in common..from_parts.len() {
        rel_parts.push("..");
    }
    rel_parts.extend(to_parts[common..].iter().copied());

    if rel_parts.first() == Some(&"..") {
        rel_parts.join("/")
    } else {
        format!("./{}", rel_parts.join("/"))
    }
}

pub async fn index() -> Html<String> {
    render_page(include_str!("assets/index.html"))
}

pub async fn settings_page() -> Html<String> {
    render_page(include_str!("assets/settings.html"))
}

pub async fn help_page() -> Html<String> {
    render_page(include_str!("assets/help.html"))
}

pub async fn about_page() -> Html<String> {
    render_page(include_str!("assets/about.html"))
}

pub async fn bookmarklet_page() -> Html<String> {
    render_page(include_str!("assets/bookmarklet.html"))
}

pub async fn novel_setting_page(Path(_id): Path<i64>) -> Html<String> {
    render_page(include_str!("assets/novel_setting.html"))
}

pub async fn rebooting_page() -> Html<String> {
    render_page(include_str!("assets/rebooting.html"))
}

pub async fn notepad_page() -> Html<String> {
    render_page(include_str!("assets/notepad.html"))
}

pub async fn author_comments_page(Path(_id): Path<i64>) -> Html<String> {
    render_page(include_str!("assets/author_comments.html"))
}

pub async fn dnd_window_page() -> Html<String> {
    render_page(include_str!("assets/dnd_window.html"))
}

pub async fn edit_menu_page() -> Html<String> {
    render_page(include_str!("assets/edit_menu.html"))
}

pub async fn asset(Path(path): Path<String>) -> Response {
    // WEB UI assets are embedded at compile time, so this routing table must stay
    // aligned with the files under src/web/assets/.
    let (content_type, body): (&'static str, Cow<'static, [u8]>) = match path.as_str() {
        "css/theme.css" => (
            "text/css; charset=utf-8",
            Cow::Owned(apply_asset_versions(include_str!("assets/css/theme.css")).into_bytes()),
        ),
        "css/base.css" => (
            "text/css; charset=utf-8",
            Cow::Owned(apply_asset_versions(include_str!("assets/css/base.css")).into_bytes()),
        ),
        "css/layout.css" => (
            "text/css; charset=utf-8",
            Cow::Owned(apply_asset_versions(include_str!("assets/css/layout.css")).into_bytes()),
        ),
        "css/components.css" => (
            "text/css; charset=utf-8",
            Cow::Owned(
                apply_asset_versions(include_str!("assets/css/components.css")).into_bytes(),
            ),
        ),
        "css/responsive.css" => (
            "text/css; charset=utf-8",
            Cow::Owned(apply_asset_versions(include_str!("assets/css/responsive.css")).into_bytes()),
        ),
        "css/settings.css" => (
            "text/css; charset=utf-8",
            Cow::Owned(apply_asset_versions(include_str!("assets/css/settings.css")).into_bytes()),
        ),
        "js/main.js" => (
            "application/javascript; charset=utf-8",
            Cow::Owned(
                apply_js_module_versions("js/main.js", include_str!("assets/js/main.js"))
                    .into_bytes(),
            ),
        ),
        "js/core/state.js" => (
            "application/javascript; charset=utf-8",
            Cow::Owned(
                apply_js_module_versions("js/core/state.js", include_str!("assets/js/core/state.js"))
                    .into_bytes(),
            ),
        ),
        "js/core/http.js" => (
            "application/javascript; charset=utf-8",
            Cow::Owned(
                apply_js_module_versions("js/core/http.js", include_str!("assets/js/core/http.js"))
                    .into_bytes(),
            ),
        ),
        "js/ui/i18n.js" => (
            "application/javascript; charset=utf-8",
            Cow::Owned(
                apply_js_module_versions("js/ui/i18n.js", include_str!("assets/js/ui/i18n.js"))
                    .into_bytes(),
            ),
        ),
        "js/ui/render.js" => (
            "application/javascript; charset=utf-8",
            Cow::Owned(
                apply_js_module_versions("js/ui/render.js", include_str!("assets/js/ui/render.js"))
                    .into_bytes(),
            ),
        ),
        "js/ui/actions.js" => (
            "application/javascript; charset=utf-8",
            Cow::Owned(
                apply_js_module_versions("js/ui/actions.js", include_str!("assets/js/ui/actions.js"))
                    .into_bytes(),
            ),
        ),
        "js/ui/dropdown.js" => (
            "application/javascript; charset=utf-8",
            Cow::Owned(
                apply_js_module_versions("js/ui/dropdown.js", include_str!("assets/js/ui/dropdown.js"))
                    .into_bytes(),
            ),
        ),
        "js/ui/shortcuts.js" => (
            "application/javascript; charset=utf-8",
            Cow::Owned(
                apply_js_module_versions("js/ui/shortcuts.js", include_str!("assets/js/ui/shortcuts.js"))
                    .into_bytes(),
            ),
        ),
        "js/ui/context_menu.js" => (
            "application/javascript; charset=utf-8",
            Cow::Owned(
                apply_js_module_versions(
                    "js/ui/context_menu.js",
                    include_str!("assets/js/ui/context_menu.js"),
                )
                .into_bytes(),
            ),
        ),
        "js/settings.js" => (
            "application/javascript; charset=utf-8",
            Cow::Owned(
                apply_js_module_versions("js/settings.js", include_str!("assets/js/settings.js"))
                    .into_bytes(),
            ),
        ),
        "fonts/Material_Symbols/MaterialSymbolsOutlined-VariableFont_FILL,GRAD,opsz,wght.ttf" => (
            "font/ttf",
            Cow::Borrowed(include_bytes!(
                "assets/fonts/Material_Symbols/MaterialSymbolsOutlined-VariableFont_FILL,GRAD,opsz,wght.ttf"
            )
            .as_slice()),
        ),
        "fonts/FORMUDPGothic/FORMUDPGothic-Regular.ttf" => (
            "font/ttf",
            Cow::Borrowed(
                include_bytes!("assets/fonts/FORMUDPGothic/FORMUDPGothic-Regular.ttf").as_slice(),
            ),
        ),
        "fonts/FORMUDPGothic/FORMUDPGothic-Bold.ttf" => (
            "font/ttf",
            Cow::Borrowed(
                include_bytes!("assets/fonts/FORMUDPGothic/FORMUDPGothic-Bold.ttf").as_slice(),
            ),
        ),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "Not found",
            )
                .into_response();
        }
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::{apply_asset_versions, apply_js_module_versions, asset_version};
    use axum::{
        extract::Path,
        http::{StatusCode, header},
    };

    #[test]
    fn apply_asset_versions_appends_query_hashes_to_known_assets() {
        let html = r#"<script src="/assets/js/main.js"></script><link rel="stylesheet" href="/assets/css/base.css">"#;
        let rendered = apply_asset_versions(html);
        assert!(rendered.contains("/assets/js/main.js?v="));
        assert!(rendered.contains("/assets/css/base.css?v="));
    }

    #[test]
    fn asset_versions_use_url_safe_base64_without_padding() {
        let version = asset_version("js/main.js").unwrap();
        assert!(!version.contains('='));
        assert!(
            version
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        );
    }

    #[test]
    fn js_module_versions_append_query_hashes_to_static_and_dynamic_imports() {
        let actions_version = asset_version("js/ui/actions.js").unwrap();
        let rendered_main =
            apply_js_module_versions("js/main.js", include_str!("assets/js/main.js"));
        assert!(rendered_main.contains(&format!("from './ui/actions.js?v={actions_version}'")));

        let rendered_render =
            apply_js_module_versions("js/ui/render.js", include_str!("assets/js/ui/render.js"));
        assert!(rendered_render.contains(&format!("import('./actions.js?v={actions_version}')")));
    }

    #[test]
    fn css_asset_versions_append_query_hashes_to_font_urls() {
        let font_version = asset_version("fonts/FORMUDPGothic/FORMUDPGothic-Regular.ttf").unwrap();
        let rendered = apply_asset_versions(include_str!("assets/css/base.css"));
        assert!(rendered.contains(&format!(
            "/assets/fonts/FORMUDPGothic/FORMUDPGothic-Regular.ttf?v={font_version}"
        )));
    }

    #[tokio::test]
    async fn font_asset_is_served() {
        let response = super::asset(Path(
            "fonts/Material_Symbols/MaterialSymbolsOutlined-VariableFont_FILL,GRAD,opsz,wght.ttf"
                .to_string(),
        ))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "font/ttf");
    }
}
