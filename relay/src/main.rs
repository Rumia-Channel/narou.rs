//! 外部取得リレーの実行ファイル。
//!
//! 起動順は旧 Node 版と同じ: `.env` を読み、鍵・許可ホスト・取得順を解決し、
//! CA バンドルを検証してから待ち受ける。設定不備は
//! `narou relay startup failed: ...` を出して終了コード 1 で止まる。

use std::sync::Arc;

use narou_relay::ca;
use narou_relay::config::Config;
use narou_relay::security::DnsResolver;
use narou_relay::server::{Relay, RelayHandle, stderr_logger};

#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("narou relay startup failed: {message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let config = Config::from_process_env()
        .await
        .map_err(|error| error.to_string())?;
    if let Some(loaded) = &config.env_loaded {
        let applied = if loaded.unreadable {
            "読み取り失敗".to_string()
        } else if loaded.applied.is_empty() {
            "新しい値なし".to_string()
        } else {
            loaded.applied.join(", ")
        };
        println!(
            "narou relay: {} を読み込みました ({applied})",
            loaded.path.display()
        );
    }
    if let Some(path) = &config.cacert {
        ca::verify_cacert_file(path).map_err(|error| format!("CA バンドルを検証できません: {error}"))?;
    } else {
        eprintln!(
            "narou relay: NAROU_RELAY_CACERT が未設定です。侵害されたルート CA の検査は行いません (NAROU_RELAY_CACERT の設定を推奨)"
        );
    }
    if config.token.len() < 32 {
        eprintln!(
            "narou relay: NAROU_RELAY_KEY が 32 バイト未満です。ランダムな 32 バイト以上の鍵を推奨します"
        );
    }
    if config.wget_dropped {
        eprintln!(
            "wget が見つからないため取得順から外します (NAROU_RELAY_WGET または PATH の wget)"
        );
    }
    let host = config.host.clone();
    let port = config.port;
    let resolver = Arc::new(DnsResolver::from_normalized(config.allowed_hosts.clone()));
    let relay = Arc::new(Relay::new(Arc::new(config), resolver, stderr_logger()));
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|_| "narou relay listener failed".to_string())?;
    let handle = RelayHandle::start(Arc::clone(&relay), listener)
        .map_err(|_| "narou relay listener failed".to_string())?;
    println!(
        "narou relay listening on {}:{} (fetch: {})",
        host,
        handle.addr.port(),
        relay.fetch_summary()
    );

    let shutdown = handle.shutdown_token();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        shutdown.cancel();
    });
    handle.wait().await.map_err(|error| error.to_string())?;
    Ok(())
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let terminate = signal(SignalKind::terminate());
        match terminate {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
