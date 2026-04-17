use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};

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
    let (content_type, body) = match path.as_str() {
        "css/theme.css" => ("text/css; charset=utf-8", include_str!("assets/css/theme.css")),
        "css/base.css" => ("text/css; charset=utf-8", include_str!("assets/css/base.css")),
        "css/layout.css" => ("text/css; charset=utf-8", include_str!("assets/css/layout.css")),
        "css/components.css" => (
            "text/css; charset=utf-8",
            include_str!("assets/css/components.css"),
        ),
        "css/responsive.css" => (
            "text/css; charset=utf-8",
            include_str!("assets/css/responsive.css"),
        ),
        "css/settings.css" => (
            "text/css; charset=utf-8",
            include_str!("assets/css/settings.css"),
        ),
        "js/main.js" => (
            "application/javascript; charset=utf-8",
            include_str!("assets/js/main.js"),
        ),
        "js/core/state.js" => (
            "application/javascript; charset=utf-8",
            include_str!("assets/js/core/state.js"),
        ),
        "js/core/http.js" => (
            "application/javascript; charset=utf-8",
            include_str!("assets/js/core/http.js"),
        ),
        "js/ui/i18n.js" => (
            "application/javascript; charset=utf-8",
            include_str!("assets/js/ui/i18n.js"),
        ),
        "js/ui/render.js" => (
            "application/javascript; charset=utf-8",
            include_str!("assets/js/ui/render.js"),
        ),
        "js/ui/actions.js" => (
            "application/javascript; charset=utf-8",
            include_str!("assets/js/ui/actions.js"),
        ),
        "js/ui/dropdown.js" => (
            "application/javascript; charset=utf-8",
            include_str!("assets/js/ui/dropdown.js"),
        ),
        "js/ui/shortcuts.js" => (
            "application/javascript; charset=utf-8",
            include_str!("assets/js/ui/shortcuts.js"),
        ),
        "js/ui/context_menu.js" => (
            "application/javascript; charset=utf-8",
            include_str!("assets/js/ui/context_menu.js"),
        ),
        "js/settings.js" => (
            "application/javascript; charset=utf-8",
            include_str!("assets/js/settings.js"),
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
    use super::{apply_asset_versions, asset_version};

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
        assert!(version.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));
    }
}
