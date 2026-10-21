//! 上流取得のトランスポート。
//!
//! リレーは TLS / HTTP の実装を持たず、システムの `curl` と `wget` に委ねる。
//! これは Cloudflare の判定がクライアント (TLS 指紋・HTTP 版・ヘッダ) と
//! 出口 IP の両方に依存するためで、旧 Node 版と同じ実測に基づく。Node 版の
//! 「Node の HTTP(S) クライアント」代替経路は、リレーの配備先が VPS と自宅回線
//! に限定され、どちらにも curl か wget があるため廃止した。
//!
//! 接続先は [`crate::security::Target`] が検証済みのアドレスに固定する。
//! リダイレクトは追わない (3xx は `location` として返し、追跡は Worker 側の
//! 共通規則が行う)。子プロセスの stderr は常に捨て、URL・Cookie・認証ヘッダを
//! ログや応答へ出さない。

pub mod curl;
pub mod wget;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

pub use crate::security::Target;

/// 取得順の 1 段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Curl,
    Wget,
}

impl TransportKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Curl => "curl",
            Self::Wget => "wget",
        }
    }
}

/// トランスポートの失敗理由。旧 Node 版 `RelayTransportError` と同じ語彙。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportErrorKind {
    NotInstalled,
    NotExecutable,
    TlsVerification,
    CaBundle,
    TimedOut,
    Cancelled,
    TooLarge,
    Incomplete,
    Failed,
}

impl TransportErrorKind {
    /// ログに出す理由 (`narou relay curl: tls-verification` の後半)。
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotInstalled => "not-installed",
            Self::NotExecutable => "not-executable",
            Self::TlsVerification => "tls-verification",
            Self::CaBundle => "ca-bundle",
            Self::TimedOut => "timed-out",
            Self::Cancelled => "cancelled",
            Self::TooLarge => "too-large",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
        }
    }

    /// 呼び出し側 (Worker) に返すメッセージ。
    pub fn message(self) -> &'static str {
        match self {
            Self::NotInstalled => "curl is not installed",
            Self::NotExecutable => "curl is not executable",
            Self::TlsVerification => "upstream TLS verification failed",
            Self::CaBundle => "relay CA bundle could not be read",
            Self::TimedOut => "upstream request timed out",
            Self::Cancelled => "request cancelled",
            Self::TooLarge => "response too large",
            Self::Incomplete => "incomplete upstream response",
            Self::Failed => "upstream request failed",
        }
    }

    /// 次のトランスポートへ進まず、リクエストを確定させる理由。
    pub fn is_fatal(self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::TooLarge | Self::TlsVerification
        )
    }
}

/// トランスポート失敗。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{}", self.kind.message())]
pub struct TransportError {
    pub kind: TransportErrorKind,
}

impl TransportError {
    pub fn new(kind: TransportErrorKind) -> Self {
        Self { kind }
    }

    pub fn reason(&self) -> &'static str {
        self.kind.reason()
    }

    pub fn message(&self) -> &'static str {
        self.kind.message()
    }

    pub fn is_fatal(&self) -> bool {
        self.kind.is_fatal()
    }
}

/// 上流応答。メタデータは [`crate::wire::ResponseMetadata`] と同じ形。
#[derive(Debug, Clone)]
pub struct TransportResponse {
    pub status: u16,
    pub content_type: String,
    pub location: Option<String>,
    pub set_cookie: Vec<String>,
    pub body: Vec<u8>,
    pub via: &'static str,
}

impl TransportResponse {
    pub fn metadata(&self) -> crate::wire::ResponseMetadata {
        crate::wire::ResponseMetadata {
            status: self.status,
            content_type: self.content_type.clone(),
            location: self.location.clone(),
            set_cookie: self.set_cookie.clone(),
            via: self.via,
        }
    }
}

/// curl トランスポートの設定。
#[derive(Debug, Clone)]
pub struct CurlOptions {
    pub bin: OsString,
    pub cacert: Option<PathBuf>,
    pub timeout: Duration,
}

/// wget トランスポートの設定。
#[derive(Debug, Clone)]
pub struct WgetOptions {
    pub bin: OsString,
    pub cacert: Option<PathBuf>,
    pub timeout: Duration,
}

/// 取得順の 1 段を実行する。
pub async fn fetch(
    kind: TransportKind,
    target: &Target,
    headers: &[(String, String)],
    curl: &CurlOptions,
    wget: &WgetOptions,
) -> Result<TransportResponse, TransportError> {
    match kind {
        TransportKind::Curl => curl::via_curl(target, headers, curl).await,
        TransportKind::Wget => wget::via_wget(target, headers, wget).await,
    }
}

/// 子プロセスを未来の drop で確実に止めるためのガード。
///
/// リクエストの期限切れ・クライアント切断でサーバーが処理 future を drop
/// したとき、`start_kill` で取得中の上流接続を閉じる。
// トランスポート実装 (別ファイル) が使うまで dead_code になる。
#[allow(dead_code)]
pub(crate) struct KillOnDrop {
    child: Option<tokio::process::Child>,
}

#[allow(dead_code)]
impl KillOnDrop {
    pub fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    pub fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child.as_mut().expect("child present until drop")
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}
