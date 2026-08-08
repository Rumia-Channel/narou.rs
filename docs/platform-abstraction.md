# Platform Abstraction & Cloudflare Workers 対応設計

> ステータス: 進行中（Phase 1・2 完了、Phase 3 以降は未着手）
> 対象: narou.rs v0.3.6 以降（2026-08-08 時点の develop）
> 方針の一次資料: ユーザー提供リファクタリング指示（最重要原則・禁止事項・設計上の優先順位に従う）

## 0. 設計判断の前提

- 外部互換（CLI・設定・`.narou/`・`webnovel/*.yaml`・narou.rb 互換挙動・変換出力）は最優先で守る。抽象化は内部構造の話。
- 逆方向依存禁止: `core → native` / `core → worker` / `core → reqwest` / `core → std::fs` は作らない。依存は常に `core → traits ← native/worker`。
- 巨大な万能 `Platform` trait は作らない。小さな capability trait を必要数だけ。
- 依存注入フレームワークは使わない。手組みのコンストラクタ注入のみ。
- 途中で常に `cargo build` / `cargo test` が通る状態を保つ。

## 1. 現在の platform 依存一覧（2026-08-08 調査結果）

調査: src/ 全 107 ファイル / 112,776 行を grep で走査（read-only）。以下は主要な箇所。

### 1.1 ファイルシステム（std::fs / File / PathBuf / canonicalize）

| 分類 | 箇所 | 内容 |
|---|---|---|
| native 専用として残す | `src/commands/init.rs` | ディレクトリ作成・YAML コピー・AozoraEpub3 設定（~15 sites） |
| native 専用として残す | `src/bin/updater.rs`, `src/updater_promote.rs` | 自己更新: ファイル置換・rename・permissions（Windows rename 問題、unix PermissionsExt） |
| native 専用として残す | `src/converter/device.rs` | AozoraEpub3 / kindlegen 実行、epub/zip 組立、tempdir（~17 sites） |
| native 専用として残す | `src/compat.rs` | dir fsync、backup zip、copy-to、java 解決（~12 sites） |
| native 専用として残す | `src/web/*.rs`（update/worker/scheduler/jobs） | self-reexec、updater spawn、server.pid |
| native 専用として残す | `src/commands/{web,log,convert,csv,manage,clean,illust,send,diff,update}.rs` | cwd 起点の pid / hotentry / ログ / CSV / キャッシュ操作 |
| 抽象化対象 | `src/db/inventory.rs` | 設定 YAML の atomic write / lock（fs2）/ metadata（~12 sites） |
| 抽象化対象 | `src/db/database.rs` | `小説データ/` archive root の create_dir_all 等 |
| 抽象化対象 | `src/downloader/persistence.rs` | section/raw/toc ファイル read/write/rename/read_dir（~13 sites） |
| 抽象化対象 | `src/downloader/mod.rs` | novel データ dir 作成・cache dir 移動・toc 読込 |
| 抽象化対象 | `src/downloader/info_cache.rs` | `.narou/` 下の小説情報キャッシュ |
| 抽象化対象 | `src/illustration_store.rs` | 挿絵ファイル read/write/rename/read_dir/cache YAML（~21 sites） |
| 抽象化対象 | `src/downloader/site_setting/loader.rs` | `webnovel/*.yaml` 読込 |
| 抽象化対象 | `src/converter/mod.rs` | 変換結果 txt 書込・変換キャッシュ・illustration cache 読込 |
| 抽象化対象 | `src/converter/{ini,settings,dakuten_font,output,inspector}.rs` | ini / replace.txt / CSS 読込書込 |
| 抽象化対象 | `src/mail.rs` | メール設定 YAML / preset コピー / 添付読込 |
| 抽象化対象 | `src/queue.rs` | queue.yaml 読込書込（sentinel / atomic write） |
| 抽象化対象 | `src/logger.rs` | ログファイル append（native のみで良い可能性が高い） |
| 抽象化対象 | `src/web/{global_settings,novel_settings,misc,jobs}.rs` | replace.txt / notepad.txt 読込書込 |
| 論理キー化 | `src/db/paths.rs` | `novel_dir_from_components` / `novel_dir_for_record` / `ensure_within_archive_root` |

### 1.2 ネットワーク / プロセス / 並行性

| 分類 | 箇所 | 内容 |
|---|---|---|
| 抽象化対象 | `src/downloader/fetch.rs` | `HttpFetcher`: `reqwest::blocking::Client` x2、curl crate、wget subprocess、redirect policy、tier fallback、rate limiter、timeout |
| 抽象化対象 | `src/downloader/narou_api.rs` | なろう API batch（blocking GET） |
| 抽象化対象 | `src/downloader/novel_info.rs` | 小説情報 GET（blocking client を受け取る） |
| 抽象化対象 | `src/downloader/rate_limit.rs` | `sleep()` による blocking rate limit + async 版（tokio） |
| 抽象化対象 | `src/commands/update.rs` | general_lastup API 用 blocking GET |
| 抽象化対象 | `src/converter/mod.rs` | 挿絵 fetch（curl crate 直接使用） |
| native 専用 | `src/web/misc.rs`, `src/web/update.rs` | GitHub API / 自己更新ダウンロード（async reqwest） |
| native 専用 | `src/compat.rs` | taskkill / where / explorer / xdg-open |
| native 専用 | `src/commands/{download,update}.rs`, `src/web/{worker,scheduler,jobs}.rs` | 自己 re-exec、relay thread、spawn |
| native 専用 | `src/mail.rs` | SMTP（lettre）、mpsc による結果受信 |
| native 専用 | `src/web/web_tray.rs` | Windows タスクトレイ |
| native 専用 | `src/main.rs` | コンソール wrapper 再実行 |

### 1.3 その他

- `std::thread`: commands/download(28), update(101), send(304), backup(22), mail(384), device(465), fetch(480), web/*(多数), web_tray(14)
- `std::sync::mpsc`: mail.rs(382), fetch.rs(479), device.rs(464)
- `std::env`: USERPROFILE/HOME（home 解決）、LOCALAPPDATA/ProgramFiles（Kindle）、JAVA_HOME、NAROU_*（テスト・隠し設定）、HIDE_CONSOLE_ENV、NO_COLOR、COMPUTERNAME/HOSTNAME
- グローバル状態: `db::DATABASE`（`Mutex<Option<Database>>`）、`rate_limit::STATE`（`LazyLock<Mutex<RateLimitState>>`）、logger、web の AppState
- socket: `commands/web.rs` の TcpStream（port-wait）、axum bind（`0.0.0.0:port`）
- tempfile: 本番は `db/inventory.rs`（atomic write 用 temp）、`commands/diff.rs`（diff ビューア）、`converter/device.rs`（AozoraEpub3 scratch dir）の 3 箇所のみ。他はテスト用
- Clock: `chrono::Utc::now()` / `chrono::Local::now()` がロジック内に直接散在

### 1.4 既に platform 非依存（そのまま core 化できる）

- `src/downloader/preprocess/*`（pest パーサ + インタプリタ）
- `src/downloader/html.rs`（HTML→青空変換）
- `src/downloader/site_setting/mod.rs` の compile / 補間ロジック（loader のみ FS 依存）
- `src/downloader/security.rs`（URL 検証・SSRF 防止）
- `src/downloader/toc.rs` / `section.rs` の parse 系
- `src/converter/converter_base/*`（文字変換・行処理・ルビ）※ `render.rs` も pure
- `src/error.rs` の `NarouError` は reqwest::Error に直接 `#[from]` している（要修正）
- `src/db/novel_record.rs` / `index_store.rs` / `ruby_time.rs`
- `src/title.rs` のタイトル変換（rename のみ FS）

## 2. 新しい module 構成

既存ツリーに `src/platform/` を追加し、既存 module は原則そのまま残す（無理な引っ越しはしない）。

```text
src/
├─ platform/              # 新規: platform abstraction（core から参照可）
│   ├─ mod.rs             #   モジュール再 export
│   ├─ http.rs            #   HttpClient trait + 型 + Mock
│   ├─ clock.rs           #   Clock trait + SystemClock + FakeClock
│   ├─ object_store.rs    #   ObjectStore trait + ObjectKey + Mock
│   ├─ rate_limiter.rs    #   RateLimiter trait + 型
│   ├─ repository.rs      #   NovelRepository trait + NovelQuery（Phase 3 で利用開始）
│   └─ mocks.rs           #   MockHttpClient / MemoryObjectStore / MemoryNovelRepository / FakeRateLimiter
├─ native/                # 新規: native 実装（core から参照しない）
│   ├─ mod.rs
│   ├─ http.rs            #   NativeHttpClient（現 HttpFetcher の transport 部分をラップ）
│   ├─ object_store.rs    #   LocalObjectStore（現 DB/Inventory/downloader persistence をラップ）
│   ├─ rate_limiter.rs    #   NativeRateLimiter（現 RateLimiter を async 化してラップ）
│   └─ clock.rs           #   SystemClock（platform::clock と統合するかは Phase 1 で判断）
├─ cli/                   # （将来）main.rs のコマンド実行層を分離したい場合の行き先。現状は commands/ をそのまま使う
├─ web/                   # 既存のまま（service 層経由化は Phase 5）
├─ commands/              # 既存のまま
├─ downloader/            # 既存のまま（Phase 2 で fetch を trait 利用へ）
├─ converter/             # 既存のまま（Phase 4 で FS 除去）
├─ db/                    # 既存のまま（Phase 3 で Repository 化）
└─ worker_entry/          # （Phase 6）worker build 用エントリ。`worker-runtime` feature でのみコンパイル
```

依存方向（コンパイル単位）:

```text
worker_entry ──> platform traits <── native (NativeHttpClient, LocalObjectStore, ...)
     │                  ▲
     │                  │
     └───── core（downloader / converter / db / commands / web のロジック）
```

- `platform/` は `reqwest` / `curl` / `std::fs` を **持たない**。
- `native/` は `reqwest` / `curl` / `std::fs` を自由に使える。core からは参照されない。
- `worker_entry/` は `worker` crate のみ依存し、platform traits を実装した Worker アダプタを置く。

## 3. trait 一覧（Phase 1 で導入するもの）

| trait | ファイル | 責務 |
|---|---|---|
| `HttpClient` | platform/http.rs | 最小 HTTP 送受信（GET/POST、redirect は実装任せ、size limit、timeout、status、content-type、bytes/string）。URL 検証は呼び出し側 or 共通ヘルパー |
| `RateLimiter` | platform/rate_limiter.rs | `async fn acquire(scope) -> Result<()>`。サイト間隔制御の抽象 |
| `Clock` | platform/clock.rs | `now_utc()`。テストで固定時刻を注入可能に |
| `ObjectStore` | platform/object_store.rs | 論理キーによる read/write/exists/delete/list（Phase 4 で本格利用。Phase 1 では型のみ） |
| `NovelRepository` | platform/repository.rs | 小説レコード CRUD + query（Phase 3 で本格利用。Phase 1 では型のみ） |

将来 phase で追加予定: `JobQueue`, `MailSender`, `ProcessRunner`, `TempStorage`, `Scheduler`（cron/durable object 用）, `PlatformCapabilities`。

## 4. 各 trait の責務（詳細）

### HttpClient

```rust
pub struct HttpRequest {
    pub url: String,
    pub method: HttpMethod,          // Get | Post
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,       // POST 用
    pub redirect: RedirectMode,      // Follow (default) | Manual
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,               // bytes 完結（デコードは core 側）
}

pub trait HttpClient: Send + Sync {
    fn send<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse>>;
}
```

- 「reqwest API のコピー」にしない。narou.rs が必要とする最小仕様（GET/POST + headers + body + status + timeout + size limit）。
- `HttpRequest` は所有型（`spawn_blocking` へ move できる）。`RedirectMode::Manual` は 3xx をそのまま返し、`resolve_final_url` が `Location` を追跡する。
- `HttpResponse` は bytes のみ。文字デコード（Shift_JIS 等）は core の `downloader::http_policy::decode_with_encoding` が行う。
- ステータス→ドメインエラー変換（404→NotFound / 503→SuspendDownload）は `http_policy::ensure_success_response` に一元化。transport は生ステータスを返す。
- 将来の future は `Send`（`tokio::spawn` で並列 update を駆動するため）。wasm32 では全型が自動 Send なので Workers は影響なし。

### RateLimiter

```rust
pub struct RateLimitScope { pub site: String, pub narou: bool }  // サイトキー（ホスト名）+ なろう系フラグ

pub trait RateLimiter: Send + Sync {
    fn acquire<'a>(&'a self, scope: &'a RateLimitScope) -> BoxFuture<'a, Result<()>>;
}
```

- 現行 `RateLimiter::wait_for_url` / `wait_async_for_url` / `wait` を `acquire` に写像した。native 実装は現行 STATE（ホスト別 next_allowed）を内部に保持。
- `scope.narou` が true のとき native 実装は `normalize_wait_steps(.., true)`（既定 10）を適用する。なろう系サイトの礼儀を維持するため、downloader は `setting.is_narou` を http_policy ヘルパーへ渡す。
- core の downloader は `self.rate_limiter.wait_for_url(url)` を直接呼ばず、注入された trait 経由にする（Phase 2 で完了）。

### Clock

```rust
pub trait Clock: Send + Sync {
    fn now_utc(&self) -> chrono::DateTime<chrono::Utc>;
}
```

- 既存の `chrono::Utc::now()` をすべて置き換えるわけではない（Phase 1 では trait を定義し、テスト容易性が必要な箇所から注入）。
- `SystemClock` は native / worker 共通で使える（chrono は wasm32 でも動作する）。

### ObjectStore

```rust
pub struct ObjectKey(pub String);      // 論理キー。native では PathBuf へ変換、worker では S3 key へ変換
pub struct ObjectMetadata { pub key: ObjectKey, pub size: u64 }

pub trait ObjectStore: Send + Sync {
    async fn exists(&self, key: &ObjectKey) -> Result<bool>;
    async fn read(&self, key: &ObjectKey) -> Result<Option<Vec<u8>>>;
    async fn write(&self, key: &ObjectKey, data: &[u8]) -> Result<()>;
    async fn delete(&self, key: &ObjectKey) -> Result<()>;
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMetadata>>;
}
```

- 「1 つの巨大 Storage trait」にしない。設定・DB・本文・挿絵・生成物は用途ごとに個別の store を作ってもよい（Phase 4 で判断）。
- native 実装は `LocalObjectStore`（ルートパスを注入）。key→パス変換は `db::paths` のロジックを利用。
- streaming は必要な経路（画像・epub・backup）から Phase 4 以降に `stream_read` / `stream_write` を追加する。最初から複雑にしない。

### NovelRepository

```rust
pub struct NovelId(pub i64);
pub struct NovelQuery { pub keyword: Option<String>, pub sort_by: Option<String>, pub offset: usize, pub limit: usize, ... }

pub trait NovelRepository: Send + Sync {
    async fn get(&self, id: NovelId) -> Result<Option<NovelRecord>>;
    async fn find_by_toc_url(&self, url: &str) -> Result<Option<NovelRecord>>;
    async fn find_by_title(&self, title: &str) -> Result<Option<NovelRecord>>;
    async fn insert(&self, record: &NovelRecord) -> Result<()>;
    async fn update(&self, record: &NovelRecord) -> Result<()>;
    async fn remove(&self, id: NovelId) -> Result<()>;
    async fn query(&self, query: NovelQuery) -> Result<Vec<NovelRecord>>;
}
```

- Phase 1 では型定義のみ。既存 `db::DATABASE` + `with_database` は native 側の実装ディテールとして残し、`YamlNovelRepository` を被せる（Phase 3）。
- 全件ロード前提の `all_records()` は Repository 実装内部に閉じ込め、core からは query 経由にする。

## 5. native implementation（Phase 1・2 で作成済み）

| 実装 | 元コード | 内容 |
|---|---|---|
| `NativeHttpClient` | `downloader/fetch.rs` の HttpFetcher の transport 部分（`src/native/http.rs`） | `HttpClient` trait を実装。内部で現行の 3-tier（curl→reqwest→wget）・redirect・UA・cookie を維持。`send` は `tokio::task::spawn_blocking` で blocking トランスポートを隔離。`RedirectMode::Manual` では 4xx/5xx 時に curl probe（CDN チャレンジ対策）を実行 |
| `RateLimiter`（`downloader/rate_limit.rs` 内の `impl platform::RateLimiter`） | `downloader/rate_limit.rs` | 現行 `RateLimiter` の `reserve_wait_duration` を async `acquire` へ写像。`tokio::time::sleep` で待機 |
| `SystemClock` | — | `chrono::Utc::now()` を返すだけ |
| `LocalObjectStore` | `db/inventory.rs` 等 | Phase 4 で。Phase 1 では型定義と `MemoryObjectStore` のみ |
| `YamlNovelRepository` | `db/database.rs` | Phase 3 で |

**Phase 2 の注意**: `HttpFetcher`（`src/downloader/fetch.rs`）は削除済み。tier fallback のステート（`tier_failures` / `prefer_curl`）は `NativeHttpClient` の `Arc` 共有フィールドとして native 側に残る。core の downloader は `Arc<dyn HttpClient>` + `Arc<dyn RateLimiter>` を注入され、`http_policy` ヘルパー（`fetch_text` / `fetch_bytes` / `resolve_final_url`）経由でのみ HTTP に触れる。

## 6. Worker implementation（Phase 6-8 の予定）

| 実装 | 技術 | 備考 |
|---|---|---|
| `WorkerHttpClient` | `worker::Fetch`（Web Fetch API） | redirect は Fetch API のデフォルト（follow）を使い、必要なら手動追跡。UA/header は FetchInit |
| `WasabiObjectStore` | S3-compatible（WebARENA Wasabi） | `aws` 系クレート or 手書き SigV4。bucket/endpoint/region/keys は Worker binding / secrets から注入し、core へ漏らさない |
| `D1NovelRepository` | `worker::D1` | novels / tags / novel_tags / settings / jobs / crawl_state テーブル + index（Phase 7） |
| `WorkerRateLimiter` | Durable Object / D1 永続化 | サイト別間隔を永続化（crawl_state） |
| `WorkerClock` | `Date::now()` を chrono に変換 | SystemClock で足りる可能性が高い |
| `WorkerJobQueue` | Cloudflare Queues | Job enum を JSON 化して enqueue |
| `CronScheduler` | `#[event(scheduled)]` + Durable Object Alarm | 5 秒間隔の外部アクセス制御は scheduler 側で |

Worker entrypoint（`worker_entry/`）は fetch / scheduled / queue の 3 エントリだけを持ち、business logic は core service を呼ぶだけにする。

## 7. data flow（目標形）

```text
CLI (main.rs)
  └─ command 層 (commands/) ──> core service (Downloader / Converter / NovelService)
                                    │  │  │  │
                          HttpClient  ObjectStore  NovelRepository  RateLimiter  Clock
                                    │  │  │  │
                    ┌───────────────┘  │  │  └─────────────┐
              native impl        worker impl           mock impl (tests)

Worker (worker_entry)
  └─ fetch / scheduled / queue ──> 同じ core service
```

- `update_novel` の business logic は core に一つだけ（`Downloader::download_novel_with_force` を service 化するのは Phase 2-3）。
- download の flow: `parse → fetch（trait 経由）→ diff（core）→ persist（ObjectStore / Repository 経由）`。

## 8. dependency direction

```text
禁止: core → native / worker / reqwest / curl / std::fs / std::process
許可: core → platform (traits) / serde / chrono / regex / pest / tracing / thiserror
許可: native → platform + reqwest + curl + std::fs ...
許可: worker → platform + worker crate + aws sdk ...
```

- `platform/` から native 固有型（`reqwest::Response`, `curl::Easy`, `std::path::Path`）を trait interface に漏らさない。
- `NarouError` は `reqwest::Error` の直接 `#[from]` をやめ、`Platform(String)` のような variant + context に置き換える（全 match 箇所の修正が必要。Phase 1 で実施）。

## 9. migration phases

| Phase | 内容 | 完了条件 |
|---|---|---|
| 1（完了） | `src/platform/`（traits + mocks）作成、`NarouError` の reqwest 直依存除去、`HttpFetcher` に `HttpClient` 実装、native ラッパ | build / test / clippy 通過。外部挙動変化なし |
| 2（完了） | downloader を trait 利用へ（fetch_text/fetch_bytes/resolve_final_url を HttpClient 経由に） | downloader から blocking HTTP 直呼びを排除（native 実装内部を除く） |
| 3 | database を Repository 化。`with_database`/`all_records` 依存を core から除去 | 主要コマンドが Repository 経由 |
| 4 | converter / illustration / command から直接 FS アクセス除去（ObjectStore / TempStorage 経由） | core から主要 FS 依存が除去 |
| 5 | Web UI を service 層経由に | web から DB/FS 直アクセスが service 経由に |
| 6 | Worker backend skeleton（worker_entry + feature 分離） | `cargo build --features worker-runtime` が通る |
| 7 | D1 NovelRepository / Wasabi ObjectStore / Worker fetch | Worker で単純 HTTP fetch + D1 + Wasabi が動作 |
| 8 | Queues / crawler / scheduling（Cron + Durable Object） | 外部サイト 5 秒間隔制御が Worker で動作 |

各 Phase 終了時: `cargo build && cargo test && cargo clippy`。

## 10. breaking internal APIs（許可された破壊的変更）

- `NarouError::Http(#[from] reqwest::Error)` → 廃止。`Platform(String)` 等へ。`#[from]` を外すため `?` での暗黙変換は消え、`map_err` が必要になる箇所が増える。
- `HttpFetcher` の public フィールド（`client` / `manual_redirect_client` 等）は維持しつつ、将来的に private 化。
- `db::with_database` / `with_database_mut` / `all_records` は Phase 3 で廃止予定（現時点では維持）。
- `RateLimiter::wait()` 等の同期メソッドは native 内部用として維持し、core は async trait 経由にする。

## 11. preserved external compatibility（守るもの）

- CLI コマンド・引数・エラー文・終了コード（narou.rb 互換）
- `.narou/` 配下の全ファイル形式（database.yaml / local_setting.yaml / queue.yaml / alias.yaml / freeze.yaml / tag_colors.yaml / latest_convert.yaml / notepad.txt / section_hash_cache）
- `webnovel/*.yaml`（ユーザー編集可能な site 定義）
- `小説データ/` ディレクトリレイアウト（`novel_dir_for_record` のまま）
- converter 出力（byte-for-byte parity fixture: カクヨム 25,273 行等）
- Web UI の外部仕様（API エンドポイント・レスポンス形式）
- hidden setting 群

## 12. unresolved issues

1. **~~`HttpFetcher` の tier fallback をどこに置くか~~（解決）**: transport（trait 実装）と policy（core）を分離した。tier fallback のステート（`tier_failures` / `prefer_curl`）は `NativeHttpClient`（`src/native/http.rs`）の `Arc` 共有フィールドとして native 側に残る。core は `http_policy` ヘルパー経由でしか HTTP に触れない。
2. **`NarouError` の variant 追加方針**: `reqwest::Error` を String 化すると原因追跡が落ちる。`Platform { context: String, source: Option<String> }` 的な形を検討。curl の `Error` / wget の `io::Error` も同様に String 化してよいか要判断。
3. **~~`std::thread` / `mpsc` の扱い~~（一部解決）**: core からは排除した。update.rs の domain 別並列 worker は `tokio::spawn` に移行済み。mail.rs は native 専用として残す。
4. **`Clock` の導入範囲**: 全 `chrono::Utc::now()` を置き換えると変更が膨大になる。テスト容易性が必要な箇所（crawler / scheduler / 更新判定）から段階的に注入する。
5. **wasm32 での chrono**: `chrono` は wasm32-unknown-unknown でデフォルト機能だと `std::time` に依存する箇所があるため、Worker build 時に feature 調整（`wasmbind` or `clock` feature 無効）が必要になる可能性。Phase 6 で検証。
6. **`webnovel/*.yaml` の loader**: `site_setting/loader.rs` は `read_dir` + `read_to_string` で native のファイル探索（exe parent / CARGO_MANIFEST_DIR / current_dir）をしている。Worker ではバンドル or D1 等から読むことになる。loader を trait 化するかは Phase 4-6 で判断。
7. **`illustration_store.rs` の扱い**: 論理キー化の単位。現行は `挿絵/` ディレクトリ直書き + `.illustration_cache.yaml`。native 互換を保ちつつ ObjectStore に被せるのは Phase 4。
8. **queue.yaml の atomic write**: `db::inventory` の atomic write は native の fs2 lock + tempfile に依存。Worker では D1 + Queues が置き換えるため、queue 永続化層は Phase 8 で再設計。
9. **logger**: tracing subscriber は native / worker で切り替える。`logger.rs` のファイル出力は native 専用にできる。Phase 1 では触らない。
10. **`converter/mod.rs` の curl 直接使用**: 挿絵 fetch は Phase 4 で `HttpClient` 経由に移行予定（Phase 2 の対象外として残置）。
11. **`web/misc.rs` / `web/update.rs` の async reqwest**: 自己更新・GitHub API は native 専用パスとして残置（Phase 2 の対象外）。
