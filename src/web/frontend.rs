use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};

pub async fn index() -> Html<&'static str> {
    Html(include_str!("assets/index.html"))
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
