use std::net::SocketAddr;

use tracing::info;

pub async fn run_web_server(port: u16, no_browser: bool) {
    use narou_rs::web;

    info!("Starting narou.rs web server on port {}", port);

    if let Err(e) = narou_rs::db::init_database() {
        eprintln!("Error initializing database: {}", e);
        std::process::exit(1);
    }

    let app = web::create_router(port);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("Listening on http://localhost:{}", port);

    if !no_browser {
        let url = format!("http://localhost:{}", port);
        let _ = open::that(&url);
    }

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
