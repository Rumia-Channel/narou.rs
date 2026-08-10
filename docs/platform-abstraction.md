# Platform Abstraction & Cloudflare Workers 対応設計

> ステータス: Phase 1〜7 完了、Phase 8（Queues / crawler / scheduling）未着手（2026-08-10 時点）
> 対象: narou.rs v0.3.6 以降（2026-08-10 時点の refactor-platform-abstraction）
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
| native 専用として残す | `src/web/{update,worker,scheduler,jobs}.rs` | self-reexec、updater spawn、queue worker、filesystem diff compatibility、server.pid |
| Phase 5 完了 | `src/application/*` | Web use-case、validation、schedule policy、job planning、settings effects、event ports。web/native/framework型を参照しない |
| 抽象化対象 | `src/db/inventory.rs` | 設定 YAML の atomic write / lock（fs2）/ metadata（~12 sites） |
| 抽象化対象 | `src/db/database.rs` | `小説データ/` archive root の create_dir_all 等 |
| Phase 4 完了 | `src/downloader/persistence.rs` | codec + `PersistenceService` は logical `ObjectKey` / async `ObjectStore` 経由。旧 Path API は `src/native/legacy_persistence.rs` の互換ラッパー |
| Phase 4 完了 | `src/downloader/mod.rs` | TOC・section・raw・cache・illustration保存は注入された store。`DownloadResult.novel_dir` とタイトル移行は native互換境界 |
| Phase 4 完了 | `src/illustration_store.rs` | pure index操作と `IllustrationStorageService`（AssetStore）を追加。legacy scan/migration API は当面 native CLI互換呼び出し |
| Phase 4 残存 | `src/downloader/info_cache.rs` | 未使用のnative cache。Worker pathへは持ち込まない |
| Phase 4 残存 | `src/downloader/site_setting/loader.rs` | bundled/user `webnovel/*.yaml` のnative loader。Phase 5以降にsource abstractionを検討 |
| Phase 4 残存 | `src/converter/mod.rs` | pure変換は維持。生成txt、section convert cache、既存画像legacy localizationはnative presentation |
| Phase 4 残存 | `src/converter/{ini,settings,dakuten_font,output,inspector}.rs` | converter設定・外部出力・診断ログのnative I/O |
| 抽象化対象 | `src/mail.rs` | メール設定 YAML / preset コピー / 添付読込 |
| 抽象化対象 | `src/queue.rs` | queue.yaml 読込書込（sentinel / atomic write） |
| 抽象化対象 | `src/logger.rs` | ログファイル append（native のみで良い可能性が高い） |
| Phase 5 残存 | `src/web/{global_settings,novel_settings,misc,jobs}.rs` | replace.txt / notepad.txt と差分表示の既存 layout 互換。application settings/content serviceへ段階移行中 |
| 論理キー化 | `src/db/paths.rs` | `novel_dir_from_components` / `novel_dir_for_record` / `ensure_within_archive_root` |

### 1.2 ネットワーク / プロセス / 並行性

| 分類 | 箇所 | 内容 |
|---|---|---|
| Phase 2 完了 | `src/downloader/http_policy.rs`, `src/native/http.rs` | HTTP policy と native transport を分離。旧 `fetch.rs` は削除済み |
| Phase 2 完了 | `src/downloader/narou_api.rs` | 注入 `HttpClient` 経由のなろう API batch |
| Phase 2 完了 | `src/downloader/novel_info.rs` | 注入 transport 経由の小説情報取得 |
| Phase 2 完了 | `src/downloader/rate_limit.rs` | platform `RateLimiter` 経由の async rate limit |
| Phase 5 残存 | `src/commands/update.rs` | CLI互換の general_lastup API 実行は native command boundary |
| Phase 4 完了 | `src/converter/mod.rs` | 挿絵 fetchは `ConverterCapabilities` の `HttpClient` / `RateLimiter` 経由。native wiringは `src/native/converter.rs` |
| native 専用 | `src/web/misc.rs`, `src/web/update.rs` | GitHub API / 自己更新ダウンロード（async reqwest）。Worker capabilityは後続フェーズ |
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

### ObjectStore / AssetStore（Phase 4 完了）

```rust
pub trait ObjectStore: PlatformService {
    fn stat<'a>(&'a self, key: &'a ObjectKey)
        -> PlatformFuture<'a, Result<Option<ObjectMetadata>>>;
    fn exists<'a>(&'a self, key: &'a ObjectKey)
        -> PlatformFuture<'a, Result<bool>>;
    fn read_small<'a>(&'a self, key: &'a ObjectKey)
        -> PlatformFuture<'a, Result<Option<Vec<u8>>>>;
    fn write_small<'a>(&'a self, key: &'a ObjectKey, data: Vec<u8>)
        -> PlatformFuture<'a, Result<()>>;
    fn delete<'a>(&'a self, key: &'a ObjectKey)
        -> PlatformFuture<'a, Result<()>>;
    fn list_page<'a>(&'a self, request: &'a ObjectListRequest)
        -> PlatformFuture<'a, Result<ObjectListPage>>;
}

pub trait AssetStore: PlatformService {
    fn stat<'a>(&'a self, key: &'a ObjectKey)
        -> PlatformFuture<'a, Result<Option<ObjectMetadata>>>;
    fn read_stream<'a>(&'a self, key: &'a ObjectKey)
        -> PlatformFuture<'a, Result<Option<AssetStream>>>;
    fn write_stream<'a>(&'a self, key: &'a ObjectKey, body: AssetStream)
        -> PlatformFuture<'a, Result<()>>;
    fn delete<'a>(&'a self, key: &'a ObjectKey)
        -> PlatformFuture<'a, Result<()>>;
    fn copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey)
        -> PlatformFuture<'a, Result<()>>;
    fn move_or_copy<'a>(&'a self, source: &'a ObjectKey, destination: &'a ObjectKey)
        -> PlatformFuture<'a, Result<()>>;
}
```

- `ObjectKey` は `/` 区切りのUTF-8 logical key。`Path` / `PathBuf` / Windows drive / UNC は入れない。
- `read_small` / `write_small` は制御ファイル専用。画像、EPUB、ZIP、PDF等は `AssetStore` の bounded chunk stream を使う。
- `ObjectListRequest` は prefix + cursor + bounded limit。Novel一覧のsource of truthは `NovelRepository` であり、LIST/HEADをmetadata DBの代わりに使わない。
- `NativeObjectStore` は `novels/<site>/<subdir>/<title>/...` を既存 `小説データ/<site>/<subdir>/<title>/...` へ写像する。`.narou` Inventoryは対象外。
- native adapterは `ensure_within_archive_root`、canonicalization、reparse-point検査を維持する。atomic writeとtemp/renameはadapter内部だけで行う。
- Worker実装は同じlogical keyをS3-compatible keyへ写像できる。moveはcopy+deleteへ実装可能。
- `GeneratedAssetKey` は将来のtxt/EPUB等の論理identityであり、native subprocess outputのPathをdomain identityにしない。

### NovelRepository（Phase 3 完了）

`NovelRepository` は `PlatformService` を継承し、target-aware な `PlatformFuture` を返す。検索条件はクロージャではなく値として表現するため、将来の D1 実装で SQL の `WHERE` / `ORDER BY` に直接変換できる。

```rust
pub struct NovelFilter {
    pub ids: Option<Vec<NovelId>>,
    pub keyword: Option<String>,
    pub site: Option<String>,
    pub domain: Option<String>,
    pub tag: Option<String>,
    pub ncode: Option<String>,
    pub is_narou: Option<bool>,
    pub suspend: Option<bool>,
    pub novel_type: Option<u8>,
    pub end: Option<bool>,
    pub terms: Vec<SearchTerm>,       // AND; values は OR
    pub frozen_ids: Option<HashSet<i64>>, // native の freeze inventory hint
}

pub enum NovelSortKey {
    Id, LastUpdate, GeneralLastup, LastCheckDate, Title, Author,
    SiteName, NovelType, Tags, GeneralAllNo, Length, Status, TocUrl,
    NewArrivalsDate,
}

pub struct NovelQuery {
    pub filter: NovelFilter,
    pub sort: NovelSort,
    pub offset: usize,
    pub limit: usize,                // 0 は無効。全件 sentinel には使わない
}

pub enum NovelMutation {
    Upsert(NovelRecord),
    Remove(NovelId),
}

pub trait NovelRepository: PlatformService {
    fn get(&self, id: NovelId) -> PlatformFuture<Result<Option<NovelRecord>>>;
    fn find_by_toc_url(&self, url: &str)
        -> PlatformFuture<Result<Option<NovelRecord>>>;
    fn find_by_title(&self, title: &str)
        -> PlatformFuture<Result<Option<NovelRecord>>>;
    fn find_by_ncode(&self, ncode: &str)
        -> PlatformFuture<Result<Option<NovelRecord>>>;
    fn count(&self, filter: &NovelFilter) -> PlatformFuture<Result<u64>>;
    fn query(&self, query: &NovelQuery)
        -> PlatformFuture<Result<Vec<NovelRecord>>>;
    fn scan_ids(&self, filter: &NovelFilter, after_id: Option<NovelId>, limit: usize)
        -> PlatformFuture<Result<Vec<NovelId>>>;
    fn allocate_id(&self) -> PlatformFuture<Result<NovelId>>;
    fn apply_batch(&self, mutations: Vec<NovelMutation>)
        -> PlatformFuture<Result<()>>;
}
```

- 表示系は `NovelQuery::page(filter, sort, offset, limit)` を使う。`scan_ids` は `id > after_id ORDER BY id LIMIT n` の keyset pagination で、バックグラウンド一括処理のために使う。
- `allocate_id` は共有 DB ロック内で単調増加カーソルを予約する。呼び出し側が `max(id) + 1` を計算することは禁止。予約された ID の間隔は許容する。
- `apply_batch` は複数の upsert/remove を一回の YAML 更新として永続化する。
- `find_by_title` は IndexStore の完全一致を先に使い、見つからない場合は大文字小文字を無視した完全一致へフォールバックする。`find_by_ncode` は `ncode` と legacy な `toc_url` suffix の双方を受理する。
- Web の検索トークンは `SearchTerm`（field / negated / OR values）へ変換し、AND 条件として repository へ渡す。任意 predicate や `serde_yaml::Value` 条件は使わない。

## 5. native implementation（Phase 1-3 完了）

| 実装 | ファイル | 内容 |
|---|---|---|
| `NativeHttpClient` | `src/native/http.rs` | `HttpClient` trait。3-tier（curl→reqwest→wget）・redirect・UA・cookie を維持し、blocking transport は `spawn_blocking` へ隔離 |
| `RateLimiter` | `src/downloader/rate_limit.rs` | `platform::RateLimiter::acquire` を実装。なろう系の既定 wait-steps も維持 |
| `NativeNovelRepository` | `src/native/novel_repository.rs` | stateless adapter。`db::DATABASE` 内の単一 `Database` を共有し、YAML を二重ロードしない。async trait は runtime 内で `spawn_blocking`、CLI は `_sync` entry point を使う |

`NativeNovelRepository` の record 操作は `get` / `find_*` / `count` / `query` / `scan_ids` / `allocate_id` / `apply_batch` に限定する。`IndexStore` の toc URL・タイトル索引は既存 `Database` 経由で維持する。`apply_batch` は一回の `database.yaml` 更新と index flush を行い、unknown fields、`raw_title`、nilable bool、日時型をそのまま保存する。

`Database::next_id` はロード済み最大 ID と現在の予約値の最大値を保持する。`allocate_id` は共有 DB ロック内で cursor を increment して予約するため、並列 task が同じ ID を受け取らない。`refresh` は未コミット予約を巻き戻さない。

**Phase 2 の注意**: `HttpFetcher`（`src/downloader/fetch.rs`）は削除済み。core の downloader は `Arc<dyn HttpClient>`、`Arc<dyn RateLimiter>`、`Arc<dyn NovelRepository>` を注入され、`http_policy` と repository trait 経由でのみ platform 実装へ触れる。

### Phase 3 で native-only に残したもの

- `Inventory` の設定ファイル、`archive_root`、freeze/lock YAML との atomic 複合更新は Phase 4 の設定/ObjectStore 抽象化対象。`compat::set_frozen_state`、`mark_not_found_and_freeze`、`commands/manage.rs` の freeze 操作は、freeze YAML と record を同時更新するため現時点では native-only のまま。
- `db::refresh()` は Web の subprocess 実行後に native の共有メモリを再読込するため残す。
- `remove_migrated_novel_dir` の path 参照判定は `scan_ids` と `get` を使うが、実ファイル削除自体は Phase 4 の ObjectStore 対象。

## 6. Worker implementation（Phase 6 skeleton / Phase 7 adapters）

| 実装 | 技術 | 状態 |
|---|---|---|
| portable core crate | `narou_rs` の `worker-runtime` feature | 完了。native-only modules は feature gate |
| `worker_entry` composition root | `AppServices` + D1/Wasabi adapters | 完了。production bindings are required |
| fetch / scheduled / queue handlers | `workers-rs` `0.8.5` event macros | 完了。queue executionはPhase 8 |
| `worker-build` / Wrangler | `worker_entry/wrangler.toml` | 完了。D1 binding/migrationsを定義。Queue consumer登録はPhase 8へ延期 |
| `WorkerHttpClient` | Workers Fetch API | 完了。bounded response body、trait future、redirect policyを維持 |
| `WasabiObjectStore` / `AssetStore` | SigV4 + S3 multipart | 完了。logical key、paged LIST、small/streaming境界を維持 |
| `D1NovelRepository` | D1 prepared statements + migrations | 完了。typed filter/sort、keyset scan、batch mutationをSQLへ変換 |
| authenticated read-only API | `/health/*`, `/api/novels*` | 完了。`NAROU_ADMIN_TOKEN`をconstant-time比較 |

Production binding setup keeps credentials out of the repository. `worker_entry/wrangler.toml` declares the `DB` binding and `migrations_dir`; set the remote D1 `database_id` in an environment-specific Wrangler configuration before deployment. Define `WASABI_ENDPOINT`, `WASABI_BUCKET`, `WASABI_REGION`, and optional `WASABI_PREFIX` as variables, and `WASABI_ACCESS_KEY`, `WASABI_SECRET_KEY`, and `NAROU_ADMIN_TOKEN` as secrets.

`worker_entry/` は fetch / scheduled / queue の3エントリだけを持つ。`composition.rs` が唯一のサービス構成点であり、Worker固有型を application/platform coreへ持ち込まない。
Queue payload は `WorkerJobEnvelope { version: 1, job: ... }` とし、未知 version は処理せず retry する。D1/Wasabiはproduction bindingとして構成し、実ジョブ実行はPhase 8へ残す。

`worker-build --release` の今回の出力は `index_bg.wasm` 1,831,053 bytes、`index.js` 27,134 bytes。生成物は `worker_entry/build/` 以下でgit管理しない。
- Panic policy: application/platform APIs return `Result`; `worker_entry` does not add a blanket `catch_unwind` or convert panics into success. Runtime event wrappers own rejected-event behavior; HTTP handlers reserve explicit 401/404/405/503 responses for boundary failures.
- `.github/workflows/platform.yml` は native check/test/clippy、worker-runtime の wasm check、`worker-build --release` を分離して実行する。Clippy は既存コードに多数の警告が残るため、警告は既存 baseline として扱い、段階的に解消する。

検証コマンド:

```text
npx wrangler dev（cwd: worker_entry、ローカル確認時）
cargo check --workspace --all-targets
cargo check -p narou_worker --target wasm32-unknown-unknown
worker-build --release（cwd: worker_entry）
```

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
| 1（完了） | `src/platform/`（traits + mocks）作成、`NarouError` の reqwest 直依存除去、native ラッパ | build / test / clippy 通過。外部挙動変化なし |
| 2（完了） | downloader を trait 利用へ（fetch_text/fetch_bytes/resolve_final_url を HttpClient 経由に） | downloader から blocking HTTP 直呼びを排除（native 実装内部を除く） |
| 3（完了） | async `NovelRepository`、typed filter/sort/query、keyset `scan_ids`、atomic ID reservation、batch mutation | native YAML compatibility、Memory/mock、Downloader injection、Web list pagination-ready |
| 4（完了） | async `ObjectStore`/`AssetStore`、logical key、NativeObjectStore、downloader persistence、illustration/converter境界 | native compatibility、Memory/native persistence tests、core主要content FS除去 |
| 5（完了） | Web UI service 層化 | Web固有のDB/FSアクセスがapplication service経由 |
| 6（skeleton 完了） | Worker backend skeleton（`worker_entry` + feature 分離 + Wrangler） | workspace native check、portable wasm check、`worker-build --release` が通る |
| 7（完了） | D1 NovelRepository / Wasabi ObjectStore / Worker fetch | authenticated read-only API、D1 prepared query/mutation、Wasabi small/streaming storage |
| 8 | Queues / crawler / scheduling（Cron + Durable Object） | 外部サイト 5 秒間隔制御が Worker で動作 |

各 Phase 終了時: `cargo build && cargo test && cargo clippy`。

### Phase 4 実装メモ

- `src/downloader/persistence.rs`: pure codec (`serialize_*` / `deserialize_*` / hash / default text) と `PersistenceService` に分離。section、TOC、raw、setting、replace、cacheをObjectStoreへ保存する。保存時刻は `Clock` 注入。
- `src/native/object_store.rs`: `novels/<site>/<subdir>/<title>/...` を既存 archive root 以下へ写像。small objectは16 MiB上限、assetは64 KiB chunkのstream。root traversal、絶対/UNC、reparse-point escapeを拒否する。
- `src/native/legacy_persistence.rs`: CLI/WebのPath互換API、legacy filename scan、atomic file wrapperをnative-onlyへ隔離。既存layoutの移行は不要。
- `src/illustration_store.rs`: `IllustrationIndex`（透明serdeでlegacy cacheをbyte-compatibleに保つpure metadata型）と `IllustrationStorageService` を追加。blob write成功後にcache indexを更新し、未知画像ごとのexists/HEAD loopを要求しない。既知cache mappingは欠損blobをrepairするため単一statを許可する。legacy `IllustrationStore` のmigration/orphan scanはnative compatibility APIとして残る。
- `src/converter/mod.rs`: pure変換は維持。生成txt、section convert cache、既存画像legacy localizationはnative presentation。illustration localizationを使う場合は`ConverterCapabilities`へ`HttpClient`、`RateLimiter`、`ObjectStore`、`AssetStore`、pure `IllustrationIndex`、logical prefix、必要なら`NovelRecord` resolverを明示注入する。
- `src/native/converter.rs`: zero-argument `NovelConverter::new` / `with_user_converter` のnative capability wiringを隔離。converter coreからNativeHttpClientを参照しない。
- `src/native/downloader.rs`: `Downloader::new` / `with_user_agent` / `with_platform` のnative HTTP、repository、ObjectStore wiringを隔離。coreのfilesystem-free constructorは`with_platform_and_storage_and_settings`。
- 通常download/update pathのObjectStore callsiteは `PersistenceService::{load_toc,save_toc,load_section,save_section,save_raw}` と `IllustrationStorageService::store_bytes`。`exists`をsection loopの判定に使わず、TOC metadata + section readで解決する。`list_page`はNative compatibility testと将来のmaintenance用途のみ。
- `MemoryObjectStore` はasync small API、paged list、delete、chunked AssetStore fakeを提供する。`NativeObjectStore` compatibility testsはTOC/section/raw/setting/replace/cache/illustrationと既存legacy sectionを確認する。
- Inventory/settings、site definition loader、Web固有Path API、downloader info cache、converter/settings/ini/inspector/user-converter/section-convert-cache、converter/device subprocess/tempdir、backup/update/loggerはObjectStoreへ統合しない。これらはconfigurationまたはnative-only capabilityであり、Phase 5/6の境界として明示する。

## 10. breaking internal APIs（許可された破壊的変更）

- `NarouError::Http(#[from] reqwest::Error)` → 廃止。`Platform(String)` 等へ。`#[from]` を外すため `?` での暗黙変換は消え、`map_err` が必要になる箇所が増える。
- `NativeHttpClient` のcurl/reqwest/wget tier fallback stateはnative transport内に閉じ、coreから直接参照しない。
- `db::with_database` / `with_database_mut` / `all_records` は core の NovelRecord 操作では使わない。native repository 内部、Inventory/settings、archive-root、freeze YAML との atomic 複合操作、テスト fixture のみ残る。
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
7. **`illustration_store.rs` の扱い**: 現行legacy filesystem APIはnative互換層として維持し、新規binary保存は`IllustrationStorageService` + `AssetStore`へ移行した。pure index型の完全分離は既存cache APIとの互換制約が残るため、後続で段階的に分離する。
8. **queue.yaml の atomic write**: `db::inventory` の atomic write は native の fs2 lock + tempfile に依存。Worker では D1 + Queues が置き換えるため、queue 永続化層は Phase 8 で再設計。
9. **logger**: tracing subscriber は native / worker で切り替える。`logger.rs` のファイル出力は native 専用にできる。Phase 1 では触らない。
10. **`converter/mod.rs` のcurl直接使用**: 解消済み。挿絵fetchは`ConverterCapabilities`の`HttpClient` / `RateLimiter`経由へ移行し、native wiringは`src/native/converter.rs`に隔離した。
11. **`web/misc.rs` / `web/update.rs` の async reqwest**: 自己更新・GitHub API は native 専用パスとして残置（Phase 2 の対象外）。
