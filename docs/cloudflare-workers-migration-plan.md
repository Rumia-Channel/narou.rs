# Cloudflare Workers 移行計画 (2026-09-26 / 改訂: 保存先の設計を差し替え)

v0.4.x 系で進めてきた Worker 対応を「本番で使える状態」まで持っていくための計画。
Phase 1-8 の抽象化（`docs/platform-abstraction.md`）で port の境界は既に引かれており、
本計画はその上に載る**足回り・保存基盤・機能の穴埋め**を扱う。

## 実装状況 (2026-09-26)

| Phase | 状態 |
|---|---|
| P0a 足回り (環境分離・プロビジョニング) | ✅ 完了 (`56d5feb`) |
| P0b 署名と S3 アダプタ | ✅ 完了 (`23338ec`, `ed87fcd`) |
| P0c 保存先の振り分け (SplitStore) | ✅ 完了 (`SplitStore` + `asset_backend`、mock 2 系統で固定) |
| P0d D1→S3 移行 (バイナリのみ) | ✅ 完了 (`/api/admin/object-migration`、core の `migrate_page` にテスト)。本文の行構造化は別項目 |
| P0e 契約テスト + CI デプロイ | ✅ 契約テストは CI (`worker-contract`)、デプロイは develop/main/タグで環境別に自動 (`worker-deploy-*`)。実アカウントでの初回実行のみ未検証 |
| P1 取得系を閉じる | ◐ 実行系は実装済み。SSRF・Cookie・設定の 3 点が未了 |
| P2 変換を Worker へ | ◐ 変換テキスト生成は完了 (`ConvertService`)。残りは device 出力 (MOBI 等) の扱い |
| P3 Web UI 移植 | ❌ 未着手 |
| P4 運用 | ❌ 未着手 |

### P1 の内訳 (2026-09-26 時点の実測)

- ✅ 実装済み: queue 実行系 (`worker_entry/src/consumer.rs`, `executor.rs`)。D1 台帳での claim、予算
  (`WorkerBudget` + section 境界チェックポイント)、bounded retry、`JobKind::is_worker_executable()` による
  種別判定まで動く。
- ✅ URL 検証・資格情報・設定の 3 点は解消済み (§2.2 #1/#3/#4)。
- ❌ サイト定義はビルド時埋め込みのみ (`EmptySiteDefinitionProvider`, `worker_entry/src/composition.rs`)。

改訂の要点: 保存先を「**オブジェクト全体を D1 か S3 のどちらかに置く**」から
「**データ種別ごとに置き場を固定し、バイナリだけを D1/S3 で切り替える**」へ変更した。
当初の全切替 (`ed87fcd` の `object_backend`) は P0c で `asset_backend` に置き換え済み（挿絵だけが S3、他は D1）。

---

## 0. 決定事項 (2026-09-26)

| 項目 | 決定 | 補足 |
|---|---|---|
| **ゴール** | **Web UI ごと Workers へ移行する**。native は CLI と、Workers で代替できない重量処理・ローカル操作のために残す | UI 移植は P3 |
| **重量処理** | **Worker 内の AozoraEpub3_Lite (in-process) で完結**。外部プロセス前提の機能（AozoraEpub3 jar / kindlegen / SMTP / 端末送信 / セルフアップデート等）は **明示的に `blocked` / `501`** とし、黙って失敗させない | CF Containers は使わない |
| **保存** | **メタデータと本文は D1（YAML/HTML をそのまま置かず、列に展開した形で保存）**、**挿絵（うごイラ含む）だけ S3 互換ストレージ**（接続先は設定値で与える。識別子は `S3_*` / `s3_*` に統一） | 詳細は §1 |
| **EPUB** | **保存しない**。Lite の機能で、Web UI の DL 要求時に保存済みデータからストリーミング生成する | 既存の `GET /api/novels/:id/download.epub` の形を維持 |
| **raw / 余計なもの** | Workers 側では **raw HTML などのキャッシュを一切保存しない** | native は従来どおり（`.narou/` 互換に影響なし） |
| **命名** | コード・binding・設定キーは `S3_*` / `s3_*`。ベンダ名を識別子に使わない | R2 / MinIO でも同じ経路 |

補足: Worker 成果物は `worker-runtime` が `lite` を含むため **GPL-3.0-only**（`worker_entry/Cargo.toml:5-9`）。

---

## 1. データの所在

### 1.1 種別ごとの置き場

| 種別 | 置き場 | 形 | 理由 |
|---|---|---|---|
| 小説メタデータ | **D1** `novels` | 列 | 一覧・検索・ソートの対象 |
| あらすじ・種別・掲載情報 | **D1** (`novels` の列 / 付随テーブル) | 列 | `toc.yaml` の内容をファイルとして持たない |
| 話（本文） | **D1** セクション用テーブル | **列に展開**（`index` / `subtitle` / `chapter` / `subchapter` / 日時 / 本文 / 変換済み本文） | 話単位で読み書きし、EPUB 生成と再変換の入力にする |
| 変換済み本文 | **D1**（同じセクション行の列） | 列 | DL 時にストリーミング生成する入力（§1.4 の確認事項） |
| 小説固有設定 | **D1**（行として保持） | 列 | 小さい。UI から読む |
| タグ・凍結・別名・タグ色・設定 | **D1** `novel_tags` / `frozen_novels` / `app_state` | 列 | メタと同じ整合の単位 |
| ジョブ台帳・実行リース | **D1** `worker_jobs` | 列 | claim/ack の原子性 |
| **挿絵（うごイラ含む）** | **S3** `novels/<site>/<title>/挿絵/<file>` | バイナリ | 1 枚数 MB。D1 の容量・行サイズを圧迫する |
| raw HTML | **保存しない** | — | Workers ではキャッシュを持たない |
| 目次 (`toc.yaml`) / 本文 (`*.yaml`) / `setting.ini` | **保存しない**（上記の列へ展開） | — | YAML をそのまま置かない方針 |
| EPUB / MOBI / ZIP | **保存しない** | — | DL 要求時にストリーミング生成（MOBI は非対応で `blocked`） |
| レート制限の状態 | **DO** storage | — | per-site の直列化 |
| ジョブ配送 | **Queue** `narou-jobs` (+ DLQ) | — | at-least-once |
| Web UI の静的資産 | **Workers Assets** | — | P3 |

保存するのは「小説を再現するのに必要な最小のデータ」だけにする。ファイル形式（YAML/HTML）は
native 側の互換のために残し、**Workers 側の保存形式には使わない**。

### 1.2 保存形式の要点

- **本文は列**。1 話 = 1 行とし、`index` / `subtitle` / `chapter` / `subchapter` / 公開・更新日時 /
  本文 / 変換済み本文 を持つ。行の順序と階層で目次を表現するので、`toc.yaml` は作らない。
- **往復一致を最上位の制約にする**。コアが `SectionFile` / `TocObject` として組み立てる値と、
  テーブルから読み戻した値が一致すること（EPUB と変換出力が native と同一になるため）。
  アダプタは YAML 文字列ではなく**値を分解して列へ入れる**。
- **挿絵は S3 のバイナリ**。既存の `IllustrationStorageService` はそのまま使い、保存先だけ S3 にする。
  うごイラ（フレーム集約 zip / APNG）も「挿絵」として同じ経路に置く。
- **変換済み本文は行に持つ**（§1.4）。EPUB はこの行と S3 の挿絵から、リクエスト時に組み立てて
  ストリーミングで返す。EPUB のバイト列は保存しない。

### 1.3 移行とロールバック

- 既存の D1 には「YAML blob（`objects`/`object_chunks`）」として本文が入っている。移行は 2 つ:
  1. **テキスト blob → 行**: YAML を分解してセクション行へ入れる（再開可能・件数で区切る）
  2. **挿絵 blob → S3**: 既存の挿絵を S3 へ写す（`verify` でバイト一致を確認）
- 移行中は両方を読めるようにし、**フラグ 1 つ**で旧経路（blob）へ戻せるようにする
  （`app_state('inv','asset_backend')` は挿絵用、テキストの移行は `content_backend` を持つ）。
- 移行完了後もしばらく blob は消さない。容量回収は P4 の判断。

### 1.5 取得から EPUB ダウンロードまでの流れ

```text
[サイト] ──HTTP──▶ Worker(queue/fetch) or CLI
                     │ サイト定義 + DSL で抽出
                     ▼
            保存（D1 の行 / S3 の挿絵）── 作品を再現する最小
                     │ 変換（P2）
                     ▼
            変換済み本文（行の列）
                     │ ユーザーが DL を押す
                     ▼
            Lite で EPUB を組み立て → ストリーミング応答
```

1. **起動**: Worker は `POST /api/jobs` → `JobService::plan` → D1 台帳 (`worker_jobs`) に claim →
   Queue → `#[event(queue)]` (`worker_entry/src/lib.rs:421`) → `consumer::process_batch` →
   `executor::execute_job` → `Downloader`。native Web UI はキュー（`.narou/queue.yaml`）経由で
   子プロセス `narou_rs download` を起動し、CLI は直接実行する。
2. **取得と抽出**: サイト定義（native=実行時の `webnovel/*.yaml`、Worker=ビルド時埋め込み）から
   `toc_url` と各パターンを得て、`HttpClient` + `RateLimiter` で取得 → TOC 解析
   （`title`/`author`/`story` + 話一覧）→ 各話 HTML を本文テキスト化 → 挿絵を取得。
   **うごイラはフレーム集約 zip を APNG に組み立ててから保存**する
   (`src/downloader/mod.rs:1015-1035`、失敗時は zip のまま)。
3. **保存**: §1.1 の表のとおり。論理キーは `NovelObjectKeys`（`src/platform/object_store.rs:270-330`）が
   決め、挿絵のファイル名は**内容ハッシュ**（みてみんは作品 ID）で、インデックス
   （元 URL → ファイル名・ハッシュ）を別に持つ (`src/illustration_store.rs:129-175`)。
4. **変換**: Worker では `JobKind::Convert` が `blocked`（`src/application/jobs.rs:64-66`）なので、
   現状は変換済み本文が存在しない。P2 で worker-executable にし、結果をセクション行の
   「変換済み本文」列へ保存する。native は `novel.txt` と出力ファイル（SQLite モードでは
   `novel_outputs` にもミラー）を作る。
5. **EPUB ダウンロード**: Worker は `GET /api/novels/{id}/download.epub` で、作品メタ →
   変換済み本文 → `挿絵/` の**名前とサイズだけ**を列挙（512 枚 / 1 枚 16 MiB を超えたら 413。
   サイズは ObjectStore の small read 上限に合わせる）→ `epub_lite::build_book_from_source`
   （構築は挿絵の**パス一覧しか読まない**）→ `EpubStreamWriter` を 1 エントリずつ進め、
   挿絵を持つエントリの直前でその 1 枚を ObjectStore から読み、`LazyImageSource` に注入して
   `EpubBuild::resolve` をかける → できたチャンクを `Response::from_stream` で流す。
   **完成した EPUB も挿絵の集合も保持しない**（ピークは書き出し中の 1 エントリ + 挿絵 1 枚）。
   挿絵のエントリ名は本文の参照と同じ相対パス (`挿絵/foo.jpg`) にする（プレフィックス付きだと
   Lite が解決できない）。変換済み本文が無ければ 409（現状の未完成点）。native Web UI は
   生成済み EPUB（`novel_outputs` か `output/*.epub`）を返す。

不変条件: 論理キーはコアが決める / 挿絵は内容ハッシュ名 / EPUB と raw は保存しない /
保存するのは作品を再現するのに必要な最小。

### 1.6 確認事項（実装前に決めたい）

- **変換済み本文を行に持つか、毎回変換するか**。DLEPUB の応答時間を考えると行に持つのが自然だが、
  変換（P2 で Worker へ移植）が未実装の間は「変換済み本文」をどう用意するかを決める必要がある。
- **raw HTML を捨てる影響**: `diff`（raw 比較）は Workers では提供しない前提でよいか
  （本文＋メタでの差分は履歴テーブルが要る）。

---

## 2. 現状（2026-09-26 調査結果）

### 2.1 既に動く範囲

| 面 | 内容 | 根拠 |
|---|---|---|
| エントリ | `fetch` / `scheduled` / `queue` の 3 ハンドラのみ | `worker_entry/src/lib.rs:23,411,421` |
| HTTP | health 2 + 読み取り 3 + ジョブ 2 + 移行 1（未コミット）。`/` と `/health/live` 以外は Bearer | `worker_entry/src/lib.rs:24-41,438-448` |
| queue 実行 | `Download` / `Update` のみ。claim → 実行 → durable terminal → ack、retry は 5/10/20 秒・最大 3 回 | `src/application/jobs.rs:64-66`, `worker_entry/src/consumer.rs:32-39,236-270` |
| cron | 毎分 planner が `Update` を enqueue（実行はしない） | `worker_entry/src/scheduler.rs:26-156` |
| サイト定義 | build 時に `webnovel/*.yaml` を埋め込み、isolate 初回に compile | `worker_entry/build.rs:20-61`, `worker_entry/src/bundled_sites.rs` |
| EPUB | `epub_lite::build_book_from_source` + `EpubStreamWriter` で 1 エントリずつ書き出し、挿絵はそのエントリの直前で 1 枚だけ読む（`LazyImageSource`）。チャンクは `Response::from_stream` で流す | `worker_entry/src/lib.rs` (`api_novel_download_epub`) |

### 2.2 本番化を止めている穴

| # | 穴 | 根拠 |
|---|---|---|
| 1 | ~~SSRF 検証が DNS 解決を要求~~ → **解決済み**: 構文検証を `platform::url_policy` に分離し、`HttpClient::validate_url` の実装が DNS の有無に応じて判定する（native=解決先まで、worker=構文とアドレスリテラル） | `src/platform/url_policy.rs`, `src/platform/http.rs`, `src/downloader/security.rs`, `worker_entry/src/http.rs` |
| 2 | ~~Convert が Worker に存在しない~~ → **解決**: `ConvertService`（core）が保存済み TOC と本文から変換テキストを組み、`<prefix>/novel.txt` へ書く。`JobKind::Convert` は worker 実行可能。native の `convert_novel_by_id` が書く固定名ミラーと同じキー | `src/application/convert.rs`, `worker_entry/src/convert.rs`, `src/application/jobs.rs` |
| 3 | ~~CookieStore 未注入~~ → **解決**: `D1CookieStore` の読み書き両方を注入（native と同じ `app_state(inv, login_cookie)`、at-rest 暗号化）。`Set-Cookie` の書き戻しと `/api/login*`（一覧・置き換え・削除）も Worker で動く | `worker_entry/src/d1_cookie_store.rs`, `worker_entry/src/http.rs`, `worker_entry/src/login.rs` |
| 4 | ~~設定が Worker に届かない~~ → **解決**: `SnapshotDownloaderSettings` に `app_state` の `local`/`global`（`update.strong` / `guard-spoiler` / `auto-add-tags` / `download.use-subdirectory` / `over18`）と `inv` の section hash cache を起動時に読んで注入。書き戻しは同期 API と非同期 D1 の都合で no-op（`over18` は Web UI 側で設定） | `src/downloader/settings.rs`, `worker_entry/src/composition.rs` |
| 5 | デプロイ設定: `wrangler.<target>.toml` + `ci/render_config.py` は完成。CI の `worker-deploy`（手動トリガー・environment ゲート）に必要な secret/vars を設定すれば動くが、**実アカウントでの実行は未検証** | `worker_entry/wrangler.develop.toml`, `.github/workflows/platform.yml` |
| 6 | ~~worker のテストが CI で 1 件も走らない~~ → **解決**: `worker_entry/tests/contract.mjs`（HTTP 契約）と `tests/run.mjs`（ローカル workerd 起動）を CI の `worker-contract` ジョブで実行 | `.github/workflows/platform.yml` |

### 2.2.1 URL 検証の分担（2026-09-26 実装）

- 純粋な構文検証（scheme / host / port / アドレスリテラルの公開判定）は `src/platform/url_policy.rs`。
  `HttpClient::validate_url` の**既定実装**がこれを使うので、DNS を持たない wasm でも全 HTTP 経路が通る。
- native は上書きして解決先アドレスまで確認する（`src/native/http.rs`, `downloader::security::validate_public_url`）。
- 同期文脈（挿絵 URL の足切り、DSL の `fetch` ガード）は `is_safe_public_url_syntax` を使う。ホスト名の
  解決先は見ないが、実際の取得時に transport が確認するので防御は 2 段のまま。
- 資格情報の保存形式は変えていない（native が書いた `enc:v1:...` を worker が復号できることを固定ベクタの
  テストで確認: `src/platform/cookie_store.rs` の `decodes_a_pinned_at_rest_payload`）。

### 2.2.2 Convert の現状 (2026-09-26 実装)

- `ConvertService` (`src/application/convert.rs`) が `<prefix>/toc.yaml` と `本文/*.yaml` を
  ObjectStore から読み、`setting.ini` / `replace.txt` / `converter.yaml`（いずれも ObjectStore 上）と
  `SettingsStore` の `default.*` / `force.*` を適用して `NovelConverter::build` → `convert_novel` を実行し、
  `<prefix>/novel.txt` に書く。native の `convert_novel_by_id` と同じ固定名ミラーなので `download.epub`
  がそのまま配信できる（409 が消える）。
- 検証: `src/application/convert.rs` のテストが MemoryObjectStore 上で往復（TOC + 本文 →
  `novel.txt`、本文が無ければ失敗）を固定する。
- 既知の差: 挿絵のローカライズ能力は native の CLI 変換と同じく渡さない（device 側の仕事）。小説ごとの
  `replace.txt` は適用するが、インストール先の全体 `replace.txt` は native 専用のまま。セクション変換
  キャッシュは Worker では持たない（毎回変換する）。
- `JobKind::Convert` は `is_worker_executable` に含めた。Send / Mail / Backup は従来どおり `blocked`。

### 2.2.3 契約テスト (2026-09-26 追加)

- `worker_entry/tests/contract.mjs` は HTTP だけを叩くので、ローカルの `wrangler dev` でもデプロイ済み環境でも
  同じものを使える。検査内容: `/health/live` `/health/ready`、認証 fail-closed (未認証・誤トークン)、
  `/api/novels` のページ形、405/404/400 の境界、`/api/jobs` の受理と `blocked` の区別、そして
  **queue consumer が実際に回って Convert ジョブが終端状態になること**（`CONTRACT_QUEUE=0` で無効化）。
- `worker_entry/tests/run.mjs` がビルド → ローカル D1 へ migration → `wrangler dev` 起動 → 契約テストまでを
  1 コマンドで行う。`.dev.vars` と `wrangler.test.toml` は gitignore 済みで、後者は `wrangler.toml` から
  `[build]` を外して生成する（毎回 release ビルドを走らせないため）。
- 実測 (2026-09-26, wrangler 4.141 / worker-build 0.8.5): 15 チェックすべて green。ログに
  `job j... failed permanently: Platform error: 小説 999999999 がありません` が出ており、Convert の
  実行経路 (plan → queue → claim → ConvertService → 終端) が workerd 上で動くことを確認した。

### 2.2.4 保存先の振り分け (P0c, 2026-09-26 実装)

- `SplitStore` (`src/platform/split_store.rs`) が「挿絵（うごイラの APNG を含む）のバイナリだけ S3、
  それ以外は D1」を 1 箇所で決める。`ObjectStore` / `AssetStore` の両方を実装し、キーで経路を選ぶ。
- 挿絵判定は **末尾 2 セグメントが `挿絵/<画像拡張子>`**。小説ディレクトリ名が偶然 `挿絵` のとき
  (`novels/<site>/挿絵/toc.yaml`) を巻き込まないため、拡張子まで見る。`.illustration_cache.yaml` は
  メタデータなので D1 に残る。
- 一覧は `<小説プレフィックス>/挿絵` の形だけ挿絵側へ回す（セグメント数でも区別）。
- ストアをまたぐ `copy` / `move_or_copy` は経路が決まらないので明示的に失敗させる。
- 切り替えは `app_state('inv','asset_backend')` (`d1` | `s3`)。`s3` で資格情報が欠けていれば起動を
  失敗させる (fail-closed)。旧 `object_backend` は撤去した。
- 検証: `src/platform/split_store.rs` のテストが振り分け（書き込み・読み出し・一覧・交差コピー拒否）と
  判定規則を `MemoryObjectStore` 2 系統で固定する。

### 2.2.5 挿絵の D1→S3 移行 (P0d, 2026-09-26 実装)

- `asset_backend` を `s3` に切り替える**前に** `POST /api/admin/object-migration {"action":"copy"}` を
  繰り返し呼ぶ。1 回 `limit` 件 (既定 100・上限 500) で区切り、`app_state` のカーソルで再開する。
  `verify` は 1 件ずつ突き合わせ、欠落・不一致・上限超過を `failed` に集める。
- 移行後も D1 側は消さない。`asset_backend` を `d1` に戻せば即座に元の経路へ戻せる。
- コアは `narou_rs::platform::store_migration`。`MemoryObjectStore` 2 系統で「挿絵だけ動く」
  「verify が欠落と不一致を報告する」を固定している。
- 既知の制約: `verify` は 16 MiB を超えるオブジェクトを「上限超過」として報告する (bounded read と同値)。

### 2.2.6 ログイン資格情報の書き込み (P1, 2026-09-26 実装)

- wasm でも at-rest 暗号化ができるようにした: `getrandom` を `worker-runtime` に足し、
  `wasm_js` バックエンド (`crypto.getRandomValues`) を**ターゲット限定の依存**で有効にする。
  native 側には `wasm-bindgen` が入らない (`cargo tree` で確認済み)。
- `D1CookieStore::save_all` は native と同じく「JSON 配列 → XChaCha20-Poly1305 (host を AAD)」で
  保存する。鍵 (`NAROU_RS_LOGIN_KEY`) が無ければ平文で書かずに失敗させる (fail-closed)。
- `WorkerHttpClient` が `Set-Cookie` を書き戻す (native の `persist_set_cookie` と同じ規則:
  送信した資格情報だけを更新し、別アカウントのセッションを壊さない)。
- 管理 API: `GET /api/login`（値は伏せて一覧）、`POST /api/login/set`、`DELETE /api/login/{host}`。
  応答形は native Web UI と同じ `{success, data|message}`。
- 検証: 契約テストが「登録 → 暗号化されている (`encrypted: true`) → 値が応答に現れない →
  一覧 → 削除」を workerd 上で確認する。乱数が wasm で動くことも同時に固定される。
- 残り: `POST /api/login/import`（書き出しファイルの取り込み。argon2 依存のため
  `login::transfer` は native のみ）と `add` / `order`。

### 2.2.7 サイト定義の差し替え経路 (P1, 2026-09-26 実装)

- モデルは全プラットフォーム共通: **bundle が種（seed）+ フォールバック**、**ユーザー定義が同名で上書き**。
  名前は `webnovel/` のファイル名そのもの（`ncode.syosetu.com.yaml`）で、拡張子を省いて渡しても補完する。
- 置き場は保存方式に従う:
  - native / YAML モード … ライブラリの `webnovel/` フォルダ（narou.rb と同じ場所）
  - native / SQLite モード … オブジェクトストア（`objects` テーブル）の `webnovel/<name>.yaml`。
    切り替え時に既存の `webnovel/` から一度だけ取り込む（元ファイルは残す）。以後ファイルを読まないので
    SQLite 構成は YAML に依存しない。
  - Worker … 同じオブジェクトストア（D1）
- 実体は `src/application/site_definitions.rs` の `SiteDefinitions`（+ `SiteDefinitionStore` port）。
  `put` は保存前に必ずコンパイル検証するので、壊れた定義で readiness を落とせない。
- 起動時に実効定義を 1 回だけ確定させ (`install_effective_site_settings`)、以後の同期コード
  （CLI のターゲット解決など）は `effective_site_settings()` を読む。
- API は native の Axum と Worker の両方に同じ形で用意した:
  `GET /api/sites` / `GET /api/sites/{name}`（実効定義の本文つき） / `PUT /api/sites/{name}` /
  `DELETE /api/sites/{name}`。応答は `{success, data|message}`。
- 残り: `AppServices.site_definitions`（`EmptySiteDefinitionProvider`）は旧 API で、いまは誰も中身を
  使っていない。`AppServices` の差し替え時に撤去する。
- **副産物のバグ修正**: `D1ObjectStore::list_keys` の範囲上限が `{prefix}0` で、`0` より後ろの文字で
  始まるキー（`本文/…` や英字名）が一覧から漏れていた。`platform::prefix_upper_bound` に置き換え、
  CJK・空プレフィックスを含むテストを追加した（挿絵の一覧も同じ理由で壊れていた）。

### 2.2.8 CI からのデプロイ (P0e, 2026-09-26 実装)

`worker_entry/ci/deploy_worker.py` が 1 環境分のデプロイを最後まで行う:

1. `ci/provision_resources.py` で D1 と Queue(+DLQ) を冪等に用意（名前は環境名から導出）
2. `ci/render_config.py` で `wrangler.ci.toml` をレンダリング（未置換プレースホルダは失敗）
3. `wrangler d1 migrations apply <db> --remote` でリモート D1 を更新
4. `wrangler deploy --secrets-file <json>` で `NAROU_ADMIN_TOKEN` と `NAROU_RS_LOGIN_KEY` を投入してデプロイ
   （secret ファイルは `finally` で必ず削除。Zero Trust が境界で両方とも不要な場合はファイルを作らない）
5. デプロイ先の URL を wrangler の出力から拾い、契約テストを smoke として流す（`NAROU_SMOKE=0` で省略）

トリガーは `.github/workflows/platform.yml`:

| きっかけ | GitHub Environment | デプロイ先 (`NAROU_DEPLOY_TARGET`) |
|---|---|---|
| `develop` へ push | `Cloudflare` | develop |
| タグ push (`v*` / 数字始まり) | `Cloudflare` | production (custom domain) |
| 手動 `workflow_dispatch` (target 選択) | `Cloudflare` | 選択した target |

- 各ジョブは `needs: [native, native-gpl, wasm, worker, worker-contract, license]` で、テストが緑のときだけ動く。
- Cloudflare の資格情報が未設定のリポジトリ (fork など) では **理由を出してデプロイだけ省略**する
  (`check Cloudflare credentials` ステップ)。テストは通常どおり走る。
- GitHub Environments は**用途で 2 つ**に分ける（target では分けない）:
  - `Cloudflare` … デプロイ 2 ジョブ用（下の表）
  - `CodeSining` … `release.yml` の Windows 署名。`CERTUM_USERNAME` / `CERTUM_OTP_URI`（secrets）と
    `CERTUM_KEY_ID`（var）。Workers のデプロイからは参照しない。

#### `Cloudflare` に置く secret

| 名前 | 必須 | 用途と挙動 |
|---|---|---|
| `CLOUDFLARE_API_TOKEN` | 必須 | `wrangler` 用 API トークン。D1 / Queue の provision、`d1 migrations apply --remote`、`deploy` に使う。未設定だと「理由を出してデプロイだけ省略」（ジョブは緑のまま） |
| `CLOUDFLARE_ACCOUNT_ID` | 必須 | アカウント ID。Dantalian 綴りの `CLOUDFLARE_ACCOUT_ID` でも動く（workflow が `\|\|` で受ける） |
| `SERVICE_DOMAIN` | production 必須 | production の custom domain（ホスト名のみ。scheme / path 不可）。`[[routes]] pattern=… custom_domain=true` の生成と smoke の宛先に使い、**ログと step summary では伏せる** |
| `DEVELOP_DOMAIN` | develop 任意 | 同様。未設定なら route を足さず workers.dev で動く |
| `NAROU_ADMIN_TOKEN` | Zero Trust が境界なら不要 | Worker API の Bearer。`--secrets-file` で Worker secret として投入。auth 有効で未設定ならデプロイを失敗させる（fail-closed） |
| `NAROU_RS_LOGIN_KEY` | 任意 | 保存したログイン Cookie の AEAD 鍵（base64 32 バイト）。未設定なら平文行だけを読み、notice を出す |
| `NAROU_S3_ACCESS_KEY_ID` / `NAROU_S3_SECRET_ACCESS_KEY` | 任意 | S3 資格情報。`S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY` という Worker secret として投入する。未設定なら Secrets Store か `wrangler secret put` で別途投入する（notice を出す） |
| `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET` | 任意 | Access の service token。smoke を custom domain 越しに流す。未設定で Access に弾かれた場合は smoke を省略する |

#### `Cloudflare` に置く var

| 名前 | 既定 | 用途と挙動 |
|---|---|---|
| `NAROU_AUTH_REQUIRED` | `true` | `[vars]` に焼き込み、Worker の Bearer 検査を on/off する。`false` は Zero Trust 前提（`NAROU_ADMIN_TOKEN` 不要）。`true` でトークン未設定なら 500 `authentication_not_configured` |
| `NAROU_WORKERS_DEV` | 「route があれば `false`」 | workers.dev の開閉。レンダラ専用（Cloudflare へは渡さない）。Access だけが境界のときに迂回口を残さないため |
| `NAROU_S3_ENDPOINT` | (a) モードで必須 | `https://…`（query / fragment 不可）。`[vars] S3_ENDPOINT` に焼き込む |
| `NAROU_S3_REGION` | (a) モードで必須 | 小文字のリージョン名（例 `us-east-1`） |
| `NAROU_S3_BUCKET` | (a) モードで必須 | S3 のバケット名規則を検証する |
| `NAROU_S3_PREFIX` | `narou/<target>` | キー前置。**設定しない**（両 target で同じバケットを prefix で共有する） |
| `NAROU_SECRETS_STORE_ID` + `NAROU_S3_*_SECRET_NAME`（5 つ） | — | (b) モード。値ではなく Cloudflare 側の secret 名を渡す。片方だけ設定すると失敗（5 つ揃える） |
| `NAROU_ADMIN_TOKEN_SECRET_NAME` / `NAROU_RS_LOGIN_KEY_SECRET_NAME` | — | トークン・鍵も Secrets Store に置く。設定すると `--secrets-file` を作らない |
| `NAROU_D1_BASE_NAME` / `NAROU_JOB_QUEUE_BASE` | `narou-rs` / `narou-jobs` | provision が `<base>-<target>` と `<queue>-dlq` を作る。target サフィックスを含めないこと |
| `NAROU_DEPLOY_URL` | — | smoke の宛先を明示（workers.dev 以外は伏せる） |
| `NAROU_SMOKE` | `1` | `0` で smoke を省略 |

- 手で置かない派生値: `NAROU_DEPLOY_TARGET`（workflow が設定）と
  `NAROU_D1_DATABASE_NAME` / `NAROU_D1_DATABASE_ID` / `NAROU_JOB_QUEUE` / `NAROU_JOB_DLQ`
  （`ci/provision_resources.py` が算出して `GITHUB_OUTPUT` で渡す）。
- Cloudflare 側に現れる形: `[vars]` は `S3_ENDPOINT` / `S3_REGION` / `S3_BUCKET` / `S3_PREFIX` /
  `NAROU_AUTH_REQUIRED`、Worker secret は `NAROU_ADMIN_TOKEN` / `NAROU_RS_LOGIN_KEY` /
  `S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY`、(b) モードでは `<NAME>_STORE` バインディング。
- デプロイ後の疎通先は `NAROU_DEPLOY_URL`（任意）→ custom domain → wrangler が報告した URL の順。
  custom domain が Access の内側にある場合は service token を渡すか smoke を省略する。
- 検証: `ci/render_config.py` を両モード・2 target（domain あり/なし、`NAROU_AUTH_REQUIRED` /
  `NAROU_WORKERS_DEV` の明示と不正値）で実行し、`tomllib` で読み戻して `[vars]`・
  `[[secrets_store_secrets]]`・`[[routes]]`・`workers_dev`・prefix を確認（ローカル、Cloudflare 不要）。

### 2.3 その他の差分

- サイト YAML はビルド時埋め込みのみでユーザー差し替え不可（`SiteDefinitionProvider` は空実装）。
- `setting_core` の `VarType::Directory` が `fs::canonicalize` を呼ぶ（`src/setting_core.rs:216,357`）。
- APNG 挿絵（うごイラ）は **Worker でも組み立てる**。`zip` を `default-features = false` + 純 Rust の deflate バックエンドに絞ることで wasm32-unknown-unknown でビルドでき、`worker-runtime` から `illustration-animation` を有効にした（実測: `image` (jpeg/png) + `zip` + `miniz_oxide` が wasm でコンパイル通過。native のユニットテスト 6 件も green）。
- D1 に content mirror（`novel_outputs` / `novel_sections`）とバージョン履歴テーブルが無い。
  ※ 新設計では本文そのものをセクション行として持つ（P0d）ので、`novel_sections` 相当は必須になる。
- Web UI は約 100 ルート（`src/web/mod.rs:567-640`）と `/ws` push。Worker 側に配信機構が無い。

---

## 3. 目標アーキテクチャ

```text
Browser ──► Worker (fetch)
             ├── ASSETS        … 現行 frontend (src/web/assets) をそのまま配信 (P3)
             ├── /api/*        … native Web UI と同名のルート (P3)
             ├── /ws           … PUSH_HUB (Durable Object) でイベント fan-out (P3)
             └── /health/*     … 無認証
                    │
  cron(毎分) ───────┤ scheduled: planner / リース回復
  Queue ────────────┤ queue:     Download / Update / Convert
                    │
              ┌─────┴───────────────────────────┬──────────────┐
              ▼                                 ▼              ▼
        SplitStore                        DO/RATE_LIMITER   DO/PUSH_HUB
         ├─ 挿絵 (うごイラ含む) → S3         (サイト別 permit)  (進捗配信)
         └─ それ以外 → 構造化 D1
             (セクション行 / メタ列)
```

### 3.1 binding 名の契約

| 名前 | 種別 | 用途 |
|---|---|---|
| `DB` | D1 | メタ・台帳・設定・テキストオブジェクト |
| `NAROU_JOBS` | Queue producer / consumer | ジョブ配送（DLQ は `narou-jobs-<target>-dlq`） |
| `RATE_LIMITER` | Durable Object | サイト別 permit 採番（既存） |
| `PUSH_HUB` | Durable Object | `/ws` の接続保持とイベント配信（P3） |
| `ASSETS` | Assets | frontend 配信（P3、`run_worker_first` で `/api/*` と `/ws` を Worker へ） |
| `S3_ENDPOINT` / `S3_REGION` / `S3_BUCKET` / `S3_PREFIX` | vars | S3 接続先。`S3_PREFIX=narou/{target}` で環境同居 |
| `S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY` | secret | 資格情報。未設定は fail-closed |
| `NAROU_AUTH_REQUIRED` | var | 認証要否（ローカル・テスト用の逃げ道） |

### 3.2 認証

- API/CI は既存の `NAROU_ADMIN_TOKEN`（Bearer、定数時間比較）を継続。
- ブラウザは token cookie（`narou_api_token`、HttpOnly / SameSite=Lax）+ 同一オリジン検査を追加（P3）。
- secret 未設定時は fail-closed（401 ではなく 500 `authentication_not_configured` として設定不備を可視化）。
- **Zero Trust (Cloudflare Access) を境界にする場合**は CI の `NAROU_AUTH_REQUIRED=false` で
  Bearer トークン検査を切れる（`NAROU_ADMIN_TOKEN` は不要）。このときは workers.dev を閉じて
  Access を通らない入口を残さないこと（route を入れた環境では既定で閉じる）。
- CI の smoke は前段の Access に弾かれた場合（`tests/contract.mjs` が exit 3 で通知）に「省略」とし、
  `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET`（Access の service token）を渡せば内側まで流せる。
  `NAROU_AUTH_REQUIRED=false` の環境では未認証で通し、認証系の検査だけを自動で省略する。

### 3.3 Workers で提供しない機能（明示的に拒否する）

| 機能 | 挙動 |
|---|---|
| `send` / `mail`(SMTP) / MOBI・Kindle 出力 | API は `501`、job は理由付き `blocked` |
| AozoraEpub3 jar / kindlegen / 外部 diff ツール | 同上（EPUB は Lite で代替済み） |
| `folder` / `browser` / `login`(ブラウザ起動) / タスクトレイ | `501` |
| `shutdown` / `reboot` / self-update / `narou db` 保守 | `501` |

---

## 4. 参考実装 (Dantalian) から輸入する型

| 型 | 輸入する内容 |
|---|---|
| 環境分離 | `wrangler.<target>.toml` をテンプレート化し、CI が置換して `wrangler.ci.toml` を生成（生成物は gitignore）。置換前に形式検証 |
| プロビジョニング | D1 と Queue(+DLQ) を list→create→再 list の冪等手順で用意し `GITHUB_OUTPUT` で受け渡す |
| デプロイ | develop=push / production=`v*` タグ + タグが main の祖先かの検証。`needs: [native, worker]`、target 別 `concurrency`、**デプロイ前に `wrangler d1 migrations apply --remote`**、`--secrets-file`、`if: always()` で生成物削除 |
| アセット | `[assets]` + ハッシュ付きアセット生成（P3） |
| 契約テスト | `wrangler dev --local` + `node --test tests/*.mjs`。**トークン未設定時に fail-closed になること**も検証。`/cdn-cgi/local/scheduled` で cron を叩ける |
| 自己修復 | `scheduled` でリース期限切れの回収と再 dispatch |
| ドキュメント | `DEPLOYMENT_CHECKLIST` / `MIGRATION_RUNBOOK` の節立てと検証記録 |

輸入しないもの: S3 実装そのもの（Dantalian は Wasabi 専用クライアント。narou.rs は S3 互換として汎用に書く）、
CF Access を唯一のユーザー境界にする設計、音声向けの instance_type / batch 値、FDK-AAC のライセンス機構。

---

## 5. フェーズ計画

| Phase | 内容 | 受け入れ条件 |
|---|---|---|
| **P0a 足回り** ✅ | 環境分離テンプレート、`ci/render_config.py`、`ci/provision_resources.py`、ローカル設定の分離 | 3 環境がレンダリングでき、未解決プレースホルダと不正値を拒否する（検証済み） |
| **P0b 署名と S3 アダプタ** ✅ | `s3_sigv4`（署名）、`s3_request`（URL + ListObjectsV2）、`s3_object_store`（ObjectStore/AssetStore） | botocore と一致する署名 9 ケース、wasm ビルド通過（検証済み） |
| **P0c 挿絵を S3 へ** ✅ | core の `is_illustration_key`（`挿絵` セグメント + 拡張子判定、テスト付き）、`SplitStore`（core）、`composition` の差し替え、`asset_backend`（`d1`/`s3`）、`object_backend` の撤去 | 挿絵だけが S3 に出て、他のキーは D1 に残る（`MemoryObjectStore` 2 系統で固定）。`asset_backend` を `d1` に戻せば旧経路で読める |
| **P0d 本文の構造化と移行** | セクション用 D1 テーブル（migration 追加）、**YAML を列へ分解して保存するアダプタ**（往復一致テスト）、raw を書かない経路、既存 YAML blob → 行の移行（再開可能 + `verify`） | 同じ小説で native と同一の EPUB / 変換出力が得られる。移行後も旧 blob 経路へ戻せる |
| **P0e 契約テスト + CI** | `worker_entry/tests/*.mjs`（health / 認証 fail-closed / オブジェクト往復）、CI に worker テスト + `d1 migrations apply --remote` + deploy を追加 | ローカルと CI で契約テストが green、デプロイが 3 環境で通る |
| **P1 取得系** | SSRF 検証の port 化、`DownloaderSettings` を D1 読みに、`CookieStore`(D1) 注入、`SiteDefinitionProvider` をストア経由に、`setting_core` の Directory 検証 port 化 | ログイン必須サイトを含む DL/更新が Worker で完走し、native と同じキー集合・本文バイトになる |
| **P2 変換** | `converter/**` の feature 分割（`device`/`inspector`/`settings` を native gate へ）、`JobKind::Convert` を worker-executable に、変換結果をセクション行の「変換済み本文」として保存（**EPUB は保存せず、DL 要求時にストリーミング生成**） | Worker 単独で download → convert → EPUB が閉じる（409 が消える）。native と同一の変換出力 |
| **P3 Web UI** | ルート移植、`[assets]` 配信、`/ws` を `PUSH_HUB` で実装、token cookie 認証、非対応機能の `501` | ブラウザから一覧・タグ・凍結・設定・キュー・進捗・EPUB DL が native UI と同等に操作できる |
| **P4 運用** | ロールバック手順（`wrangler versions` + `asset_backend` + prefix 切替）、D1/S3 の容量・コスト設計、バックアップ経路の再定義、runbook | runbook に沿って前バージョンへ戻せる。検証記録が残る |

各 Phase は独立 commit 単位。P1 完了までは「Worker はまだ本番運用しない」前提を維持する。

---

## 6. リスクと緩和

| リスク | 緩和 |
|---|---|
| SigV4 実装の誤り（時刻ずれ・payload hash・path encoding） | 署名は botocore 生成の 9 ケースで一致を固定。`UNSIGNED-PAYLOAD` は使わず常にボディのハッシュを渡す |
| S3 資格情報の漏洩・ログ混入 | Secrets Store / `env.secret` 優先、CI は `--secrets-file`、`if: always()` で生成物削除。値はリポジトリに置かない |
| 振り分けの判定ミス（本文が S3 に出る / 挿絵が D1 に残る） | 判定は core の純関数にしてテストで固定。移行は `verify` でバイト一致を確認してからフラグを切り替える |
| D1 容量・行サイズ | バイナリは S3 へ（本計画）。D1 はメタ・テキストのみ |
| Worker の CPU / subrequest 上限 | `cpu_ms = 300000` を維持、`WorkerBudget` の section 境界 yield を継続、EPUB は 1 エントリずつ書き出し（挿絵は 512 枚 / 1 枚 16 MiB 超で 413） |
| cron 毎分の計画コスト | 100 件ページ + generation/lease の早期 exit を維持 |
| 移行中の二重書き・欠落 | カーソル再開 + `verify`。D1 側は消さないので即時ロールバック可能 |
| GPL 整合（worker は `lite` を含む） | `worker_entry/Cargo.toml` の license 表記と CI の license job を維持 |
| native との挙動差が残ったまま移行 | P1 の受け入れ条件に「native と同じキー集合・バイト一致」を入れる |

---

## 7. 検証コマンド

```bash
# ローカル (ビルド → ローカル D1 へ migration → wrangler dev → 契約テスト)
cd worker_entry
node tests/run.mjs              # debug build / --release で CI と同じ / --keep で dev を残す
# NAROU_CHECK_UNCONFIGURED=1 で、トークンを外した構成の fail-closed も確認する（CI は有効）

# 既に動いている環境に対する契約テスト (デプロイ後の smoke test もこれ)
BASE_URL=https://... NAROU_ADMIN_TOKEN=... node worker_entry/tests/contract.mjs
# queue consumer が回る前提の検査 (Convert の終端待ち) は既定で有効、CONTRACT_QUEUE=0 で無効

# 型・ビルド
cargo check -p narou_rs --target wasm32-unknown-unknown --no-default-features --features worker-runtime
cargo check -p narou_worker --target wasm32-unknown-unknown
worker-build --release          # cwd: worker_entry

# デプロイ（CI と同じ順序）
python worker_entry/ci/provision_resources.py
python worker_entry/ci/render_config.py
npx wrangler d1 migrations apply <db> --remote --config wrangler.ci.toml
npx wrangler deploy --config wrangler.ci.toml --secrets-file <json>
```

---

## 8. 未解決・確認事項

- **変換済み本文の持ち方**（§1.4）: セクション行の列に持つ前提で書いている。変換が Worker に載る
  （P2）までは、native で変換した値をどう入れるか（移行時に一緒に取り込む / P2 まで DL を 501 に保つ）を決める。
- **raw HTML を保存しない影響**: `diff`（raw 比較）は Workers では提供しない。本文レベルの差分が
  必要になったら履歴テーブルを足す（native の `diff` とは別物として扱う）。
- **S3 のバケット構成**: 1 バケット + prefix（環境同居）を既定とする。本番だけ別バケットにするかは
  P0e の実測後に決める。
- **EPUB 応答のメモリ (2026-09 解消)**: `download.epub` は Lite の `EpubStreamWriter`
  （`next_entry` / `write_current` / `finish`）で 1 エントリずつ書き出し、挿絵はそのエントリの
  直前で 1 枚だけ ObjectStore から読んで `LazyImageSource` に注入する。チャンクは
  `worker::Response::from_stream` で流す（isolate は単一スレッドだが、`wasm-streams` が HWM 0 で
  1 プルずつ取り出すためストリーミングできる）。ピークは「書き出し中の 1 エントリ + 挿絵 1 枚」で、
  完成アーカイブ・応答コピー・挿絵の集合をどれも保持しない。
  当初は先読みが要ると見ていたが、narou の構築経路 (`build_book_from_source`) は挿絵の
  **パス一覧しか読まない**（`read_image` を呼ぶのは書き出し時の `resolve` だけ）ため不要だった。
  そのため Lite 側に逐次収集 API を足す必要も無い。
  挿絵の読み出しに失敗すると応答は途中で切れる (ストリーム開始後にステータスは変えられない) ため、
  列挙時に判定できるもの (枚数・1 枚のサイズ) は先に 413 で断る。
- **D1 の `objects`/`object_chunks` の扱い**: 本文の移行後もしばらく残す。削除（容量回収）は P4 の判断。
- **APNG 挿絵**: `zip` の feature を純 Rust 構成に絞ったため Worker でも組み立て可能（`worker-runtime` が
  `illustration-animation` を有効化済み）。組み立ては**フレームを 1 枚ずつ復号 → 符号化 → 追記して即解放**し、
  全フレームを溜めない（`src/illustration_animation.rs` の `build_apng`）。実測で
  **ピーク 48.7 MiB**（Pixiv 69642452: 19 フレーム 1920×1080、zip 4.3 MiB → APNG 20.3 MiB）/
  **23.5 MiB**（合成 60 フレーム 1600×900）。旧実装（全フレーム保持）は復号だけで 150〜330 MiB になり
  Workers の 128 MiB に収まらない。上限はフレーム 512 / 1 フレーム 1920×1080 画素 / 出力 56 MiB で、
  超えたら OOM ではなく明示エラーにし、呼び出し側は取得したアーカイブをそのまま保存する。
- **バージョン履歴 / diff**: D1 にテーブルが無いため、P3 で `diff` を出すなら先にスキーマを追加する。
- **ユーザー YAML の差し替え**: P1 でストア経由の読み込みに戻すが、UI から編集させるかは別判断。
- **移行ツール (挿絵)**: `worker_entry/src/object_migration.rs` + `lib.rs` の
  `/api/admin/object-migration`。挿絵だけを D1 から S3 へ写し、`copy` / `verify` / `status` を持つ。
  進捗は `app_state('inv','migrate_illustrations')` のカーソルで再開できる。
- **未着手 (P0d の残り)**: 本文の行構造化 (「テキスト blob → セクション行」) と raw を書かない経路。
