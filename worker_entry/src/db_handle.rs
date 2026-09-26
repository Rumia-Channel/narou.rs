//! D1 のハンドル。
//!
//! UI の読み取り経路を read replica に逃がせるよう、primary への直接実行と
//! session (`first-unconstrained`) 経由の実行を同じ型で扱う。
//!
//! - `Primary` — すべてのクエリが primary に行く。ジョブ実行系
//!   (consumer / executor / scheduler / ledger) はこちら。
//! - `Session` — `first-unconstrained` セッション。読み取りは read replica に
//!   載り、書き込みは自動的に primary へ転送される。セッション内では
//!   read-your-writes が保証される (bookmark による順序制御)。
//!
//! read replication が有効でない D1 でもセッションは primary にフォールバック
//! する仕様のため、未設定のデータベースでも安全に動く。

use std::sync::Arc;

use worker::{D1Database, D1DatabaseSession, D1PreparedStatement, D1Result, D1SessionConstraint};

/// `D1Database` (primary 直行) と `D1DatabaseSession` (制約付き) の共通口。
/// 両者とも同じ `D1PreparedStatement` を返すので、ストア側は `prepare` /
/// `batch` だけを意識すればよい。
#[derive(Debug, Clone)]
pub enum DbHandle {
    /// すべてのクエリが primary に行く経路。
    Primary(Arc<D1Database>),
    /// read replication 対象。読み取りは replica、書き込みは primary。
    Session(Arc<D1DatabaseSession>),
}

impl DbHandle {
    /// primary 直行ハンドル。
    pub fn primary(db: Arc<D1Database>) -> Self {
        Self::Primary(db)
    }

    /// `first-unconstrained` セッション。read replication 未設定の DB でも
    /// 安全に動く (その場合は従来どおり primary が応答する)。
    pub fn unconstrained(db: &D1Database) -> worker::Result<Self> {
        db.with_session_constraint(D1SessionConstraint::FirstUnconstrained)
            .map(|session| Self::Session(Arc::new(session)))
    }

    /// UI 経路用のハンドル。セッションを優先し、生成に失敗した場合は
    /// (セッション API に未対応のローカル D1 など) primary に落とす。
    /// 本番で read replication が無効な場合もセッション内部で primary に
    /// フォールバックするため、この fallback は常に安全側に働く。
    pub fn ui(db: Arc<D1Database>) -> Self {
        match Self::unconstrained(&db) {
            Ok(handle) => handle,
            Err(error) => {
                worker::console_warn!(
                    "D1 session unavailable ({error}); falling back to the primary binding"
                );
                Self::Primary(db)
            }
        }
    }

    pub fn prepare<T: Into<String>>(&self, query: T) -> D1PreparedStatement {
        match self {
            Self::Primary(db) => db.prepare(query),
            Self::Session(session) => session.prepare(query),
        }
    }

    pub async fn batch(
        &self,
        statements: Vec<D1PreparedStatement>,
    ) -> worker::Result<Vec<D1Result>> {
        match self {
            Self::Primary(db) => db.batch(statements).await,
            Self::Session(session) => session.batch(statements).await,
        }
    }
}
