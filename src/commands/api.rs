//! `narou api` — 外部公開 API の Bearer トークン管理。
//!
//! 外部公開 API (`/api/v1`) に入るためのトークンを、名前つきで管理する。
//! 作成したトークンは設定ページの「外部API」タブからも確認できる。

use std::sync::Arc;

use narou_rs::application::api_tokens::ApiTokenService;
use narou_rs::error::Result;

/// `narou api` のサブコマンド。
#[derive(clap::Subcommand, Debug)]
pub enum ApiAction {
    /// 保存されているトークンを一覧する。
    List,
    /// 新しいトークンを作る。
    Create {
        /// トークンに付ける名前 (例: 表生成スクリプト)。
        name: Option<String>,
    },
    /// トークンを削除する。
    Delete {
        /// 削除するトークンの ID (`narou api list` で確認できる)。
        id: String,
    },
    /// 外部 API の情報 (認証の有効・無効とエンドポイント) を表示する。
    Info,
}

/// `narou api` を実行する。
pub fn cmd_api(action: ApiAction) -> Result<()> {
    let service = ApiTokenService::new(Arc::new(
        narou_rs::native::api_tokens::NativeApiTokenStore,
    ));
    match action {
        ApiAction::List => list(&service),
        ApiAction::Create { name } => create(&service, name.as_deref()),
        ApiAction::Delete { id } => delete(&service, &id),
        ApiAction::Info => info(&service),
    }
}

/// トークンを一覧する。
fn list(service: &ApiTokenService) -> Result<()> {
    let tokens = futures::executor::block_on(service.infos())?;
    if tokens.is_empty() {
        println!("保存された外部 API のトークンはありません。");
        println!("  narou api create <名前> で作れます。");
        return Ok(());
    }
    println!("外部 API のトークン: {} 件", tokens.len());
    for token in &tokens {
        println!(
            "  {} [{}] {} (作成: {})",
            token.id, token.name, token.fingerprint, token.created_at
        );
    }
    Ok(())
}

/// トークンを作る。
fn create(service: &ApiTokenService, name: Option<&str>) -> Result<()> {
    let token = futures::executor::block_on(service.create(name))?;
    println!("トークンを作成しました: {}", token.name);
    println!("  ID: {}", token.id);
    println!("  トークン: {}", token.token);
    println!();
    println!("  HTTP ヘッダ: Authorization: Bearer {}", token.token);
    println!("  保存は平文です。失ったら作り直してください。");
    Ok(())
}

/// トークンを削除する。
fn delete(service: &ApiTokenService, id: &str) -> Result<()> {
    if futures::executor::block_on(service.delete(id))? {
        println!("トークンを削除しました: {id}");
        Ok(())
    } else {
        println!("トークンが見つかりません: {id}");
        Ok(())
    }
}

/// 外部 API の情報を表示する。
fn info(service: &ApiTokenService) -> Result<()> {
    let require_token = narou_rs::db::settings::bool_value(
        narou_rs::setting_core::SettingScope::Global,
        narou_rs::application::external_api::REQUIRE_TOKEN_SETTING,
    )
    .unwrap_or(true);
    println!("外部公開 API: /api/v1");
    println!(
        "  Bearer 認証: {}",
        if require_token {
            "必須 (external-api.require-token=true)"
        } else {
            "不要 (external-api.require-token=false)"
        }
    );
    println!("  トークン: {} 件", futures::executor::block_on(service.infos())?.len());
    println!();
    println!("エンドポイント:");
    println!("  GET  /api/v1/novels                     一覧 (条件・並び替え・形式を選べる)");
    println!("  GET  /api/v1/novels/{{id}}                作品 1 件");
    println!("  GET  /api/v1/novels/{{id}}/toc            目次 (バーチャル小説は連結)");
    println!("  GET  /api/v1/novels/{{id}}/sections/{{key}} 1 話の本文");
    println!("  GET  /api/v1/novels/{{id}}/epub           EPUB");
    println!("  GET  /api/v1/tags                        タグ一覧");
    println!("  POST /api/v1/mcp                         MCP (JSON-RPC 2.0)");
    Ok(())
}
