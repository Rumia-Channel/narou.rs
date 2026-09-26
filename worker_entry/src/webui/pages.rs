//! 動的パスに乗る HTML ページ (native: `src/web/frontend.rs` の
//! `novel_setting_page` / `author_comments_page` / `dnd_window_page` /
//! `rebooting_page`) と、一覧行の DL ボタンが向ける `GET /novels/{id}/download`
//! (native `novels::download_ebook`)。
//!
//! Worker 側では静的ページは `[assets]` (`public/`) がそのまま配信するが、
//! id を含むパスや `/_rebooting` (`rebooting.html` と名前が違う) は assets
//! の `html_handling = "auto-trailing-slash"` では解決できず 404 になる。
//! ここでは ASSETS バインディングから同じビルド済み HTML を取り寄せて返す
//! (`build_assets.mjs` が焼き込んだ `?v=` / `__NAROU_RS_WEBUI_BUILD__` を
//! そのまま活かすため `include_str!` は使わない)。
//!
//! 認証は付けない: 静的 UI (`/`・`/settings` 等) は assets が無認証で配って
//! おり、ブラウザ遷移が送れる Bearer ヘッダも無い。中身の API
//! (`/api/settings/{id}` 等) は引き続き個別に認証する。
//!
//! `GET /novels/{id}/download` は `lib.rs` の `api_novel_download_epub`
//! (download-time EPUB ストリーム) をそのまま返す。native は変換済みファイル
//! を探してなければ lite で生成する経路だが、Worker の EPUB は常に要求時
//! 生成なので両者は同じ実装に収束する。

use worker::{Env, Method, Request, Response};

use super::json_error;

/// `lib.rs` のルート表から呼ばれる動的ページ/ダウンロードの振り分け。
pub async fn handle(req: Request, env: Env) -> worker::Result<Response> {
    let path = req.path();

    if path == "/widget/drag_and_drop" {
        return match req.method() {
            Method::Get | Method::Head => serve_asset(&req, &env, "dnd_window.html").await,
            _ => json_error(405, "method_not_allowed", None),
        };
    }
    if path == "/_rebooting" {
        // native `/_rebooting` → rebooting.html。assets 直下のファイル名と
        // URL が一致しないのでここで張る。
        return match req.method() {
            Method::Get | Method::Head => serve_asset(&req, &env, "rebooting.html").await,
            _ => json_error(405, "method_not_allowed", None),
        };
    }

    let Some(rest) = path.strip_prefix("/novels/") else {
        return Response::error("Not Found", 404);
    };
    // native は `Path<i64>` で非数値を弾く。JS が正規表現 `\d+` でしか
    // 開かないので、パースできないパスは 404 (既存 `api_novel` と同じ規則)。
    if let Some(id) = rest.strip_suffix("/setting") {
        return match req.method() {
            Method::Get | Method::Head if id.parse::<i64>().is_ok() => {
                serve_asset(&req, &env, "novel_setting.html").await
            }
            Method::Get | Method::Head => Response::error("Not Found", 404),
            _ => json_error(405, "method_not_allowed", None),
        };
    }
    if let Some(id) = rest.strip_suffix("/author_comments") {
        return match req.method() {
            Method::Get | Method::Head if id.parse::<i64>().is_ok() => {
                serve_asset(&req, &env, "author_comments.html").await
            }
            Method::Get | Method::Head => Response::error("Not Found", 404),
            _ => json_error(405, "method_not_allowed", None),
        };
    }
    if let Some(id) = rest.strip_suffix("/download") {
        return match (req.method(), id.parse::<i64>()) {
            (Method::Get | Method::Head, Ok(id)) => {
                crate::api_novel_download_epub(req, env, id).await
            }
            (Method::Get | Method::Head, Err(_)) => Response::error("Not Found", 404),
            _ => json_error(405, "method_not_allowed", None),
        };
    }
    Response::error("Not Found", 404)
}

/// ASSETS バインディングから `public/<file>` (ビルド済み HTML) を取り寄せる。
/// 生成済みファイルが無い場合は 404 のまま返る (その応答を素通しする)。
async fn serve_asset(req: &Request, env: &Env, file: &str) -> worker::Result<Response> {
    let assets = env.assets("ASSETS")?;
    let mut url = req.url()?;
    url.set_path(&format!("/{file}"));
    assets.fetch(url.to_string(), None).await
}
