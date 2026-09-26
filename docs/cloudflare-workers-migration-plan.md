# Cloudflare Workers 移行計画 (2026-09-26)

v0.4.x 系で進めてきた Worker 対応を「本番で使える状態」まで持っていくための計画。
Phase 1-8 の抽象化（`docs/platform-abstraction.md`）で port の境界は既に引かれており、
本計画はその上に載る**足回り・保存基盤・機能の穴埋め**を扱う。

## 実装状況 (2026-09-26): **計画策定済み / P0 着手**

| Phase | 状態 |
|---|---|
| P0 足回りと保存基盤 | ◐ 進行中 (環境分離テンプレート + provision/render スクリプト完了、S3 adapter / D1→S3 移行 / 契約テスト / CI デプロイは未着手) |
| P1 取得系を閉じる | ❌ 未着手 |
| P2 変換を Worker へ | ❌ 未着手 |
| P3 Web UI 移植 | ❌ 未着手 |
| P4 運用 | ❌ 未着手 |

P0 で追加済みのもの:

- `worker_entry/wrangler.develop.toml` / `wrangler.staging.toml` / `wrangler.production.toml` (プレースホルダ入りテンプレート)
- `worker_entry/ci/render_config.py` (置換 + 形式検証。未解決プレースホルダは失敗)
- `worker_entry/ci/provision_resources.py` (D1 / Queue / DLQ の冪等作成と `GITHUB_OUTPUT`)
- `worker_entry/wrangler.toml` はローカル専用値 (D1 `narou-local` / queue `narou-jobs-local` / S3 prefix `narou/local`) に変更し、デプロイには使わない方針を明記
- `.gitignore` に生成物 (`wrangler.ci.toml` / `build/` / `.wrangler/` / `.dev.vars` / `public/`) を追加

---

## 0. 決定事項 (2026-09-26)

| 項目 | 決定 | 補足 |
|---|---|---|
| **ゴール** | **Web UI ごと Workers へ移行する**。native は CLI と、Workers で代替できない重量処理・ローカル操作のために残す | UI 移植は P3 |
| **重量処理** | **Worker 内の AozoraEpub3_Lite (in-process) で完結**させる。外部プロセス前提の機能（AozoraEpub3 jar / kindlegen / SMTP / 端末送信 / セルフアップデート等）は **明示的に `blocked` / `501`** とし、黙って失敗させない | CF Containers は使わない |
| **保存** | **メタデータは D1、オブジェクトは S3 互換ストレージ**。本番の接続先は Wasabi を想定するが、**実装と設定名は S3 で統一**する | 環境ごとに prefix を分けて同居 |
| **命名** | コード・binding・設定キーは `S3_*` / `s3_*`。ベンダ名（Wasabi 等）を識別子に使わない | 将来 R2 / MinIO / 他 S3 実装へも同じ経路で載せられる |

補足: Worker 成果物は `worker-runtime` が `lite` を含むため **GPL-3.0-only**（`worker_entry/Cargo.toml:5-9`）。

---

## 1. 現状（2026-09-26 調査結果）

### 1.1 既に動く範囲

| 面 | 内容 | 根拠 |
|---|---|---|
| エントリ | `fetch` / `scheduled` / `queue` の 3 ハンドラのみ | `worker_entry/src/lib.rs:23,411,421` |
| HTTP | 7 ルート（health 2 / 読み取り 3 / ジョブ 2）。`/` と `/health/live` 以外は Bearer 認証 | `worker_entry/src/lib.rs:24-40,391-402` |
| queue 実行 | `Download` / `Update` のみ。claim → 実行 → durable terminal → ack、retry は 5/10/20 秒・最大 3 回・以降 `permanent` | `src/application/jobs.rs:64-66`, `worker_entry/src/consumer.rs:32-39,236-270` |
| cron | 毎分 planner が `Update` を enqueue（実行はしない）。generation/lease で多重起動を防ぐ | `worker_entry/src/scheduler.rs:26-156` |
| 保存 | **D1 のみ**（`novels` 系 / `worker_jobs` / `app_state` / `objects`+`object_chunks`）。DO は `SiteRateLimiter` 1 個 | `worker_entry/src/d1_object_store.rs`, `worker_entry/migrations/0008_objects.sql` |
| サイト定義 | build 時に `webnovel/*.yaml` を埋め込み、isolate 初回に compile | `worker_entry/build.rs:20-61`, `worker_entry/src/bundled_sites.rs` |
| EPUB | `epub_lite::build_book_from_source` を in-process で実行（Lite は wasm 可） | `worker_entry/src/lib.rs:196-315` |

### 1.2 本番化を止めている穴（P0/P1 で埋める）

| # | 穴 | 根拠 |
|---|---|---|
| 1 | **SSRF 検証が DNS 解決を要求**し、全 HTTP 経路が通る。wasm には getaddrinfo が無いため取得系が成立しない | `src/downloader/security.rs:30-36`, `src/downloader/http_policy.rs:214,219,276` |
| 2 | **Convert が Worker に存在しない**（`converter/**` は native gate）。EPUB 配信が前提とする `<prefix>/novel.txt` を書く実装が現ツリーに無く、常に 409 | `src/lib.rs:14-15`, `worker_entry/src/lib.rs:203-211` |
| 3 | **CookieStore 未注入**（`cookies: None`）で、ログイン必須サイトは必ず `Blocked` | `worker_entry/src/composition.rs:125-136`, `src/downloader/mod.rs:136-138` |
| 4 | **設定が Worker に届かない**（`WorkerDownloaderSettings` は全既定値）。subdirectory 命名・UA・timezone 等が native と変わる | `src/downloader/settings.rs:76-80` |
| 5 | **デプロイ設定が未完**（`database_id` 未設定・secret 投入手順なし・環境分離なし・CI にデプロイ無し） | `worker_entry/wrangler.toml:9-17`, `.github/workflows/platform.yml:45-70` |
| 6 | **worker のテストが CI で 1 件も走らない**（`worker-build --release` のみ） | 同上 |

### 1.3 その他の差分（P2/P3 で扱う）

- サイト YAML はビルド時埋め込みのみでユーザー差し替え不可（`SiteDefinitionProvider` は `EmptySiteDefinitionProvider`）。
- `setting_core` の `VarType::Directory` が `fs::canonicalize` を呼ぶ（`src/setting_core.rs:216,357`）。
- APNG 挿絵（うごイラ）は wasm で無効（`Cargo.toml:55`, `src/illustration_animation.rs:60-65`）→ zip のまま保存。
- D1 に content mirror（`novel_outputs` / `novel_sections`）とバージョン履歴テーブルが無い。
- Web UI は約 100 ルート（`src/web/mod.rs:567-640`）と `/ws` push。Worker 側に配信機構が無い。
- 進捗・ログは port 未使用（`NoProgress`、tracing 直）。

---

## 2. 目標アーキテクチャ

```text
Browser ──► Worker (fetch)
             ├── ASSETS        … 現行 frontend (src/web/assets) をそのまま配信
             ├── /api/*        … native Web UI と同名のルート（P3）
             ├── /ws           … PUSH_HUB (Durable Object) でイベント fan-out
             └── /health/*     … 無認証
                    │
  cron(毎分) ───────┤ scheduled: planner / lease 回復
  Queue ────────────┤ queue:     Download / Update / Convert
                    │
   ┌────────────────┼─────────────────┬──────────────────┐
   ▼                ▼                 ▼                  ▼
  D1              S3              DO(RATE_LIMITER)   DO(PUSH_HUB)
  メタ・台帳      section/挿絵     サイト別ペーシング   進捗・ログ配信
  app_state       EPUB
```

### 2.1 binding 名の契約

| 名前 | 種別 | 用途 |
|---|---|---|
| `DB` | D1 | メタデータ・ジョブ台帳・設定（既存） |
| `NAROU_JOBS` | Queue producer / consumer | ジョブ配送（既存。DLQ は `narou-jobs-dlq`） |
| `RATE_LIMITER` | Durable Object | サイト別 permit 採番（既存） |
| `PUSH_HUB` | Durable Object | `/ws` の接続保持とイベント配信（新規） |
| `ASSETS` | Assets | frontend 配信（新規、`run_worker_first` で `/api/*` と `/ws` を Worker へ） |
| `S3_ENDPOINT` / `S3_REGION` / `S3_BUCKET` / `S3_PREFIX` | vars | S3 接続先。`S3_PREFIX=narou/{target}` で環境同居 |
| `S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY` | secret（Secrets Store 優先、`env.secret` フォールバック） | 資格情報。未設定は fail-closed |
| `NAROU_AUTH_REQUIRED` | var | frontend/API の認証要否（ローカル・テスト用の逃げ道） |

### 2.2 オブジェクトキー設計

- **core の論理キーをそのまま S3 のキーにする**（`NovelObjectKeys`: `src/platform/object_store.rs:229-303`）。例: `novels/ncode.syosetu.com/[作者] タイトル/本文/0001 第一話.yaml`、`.../挿絵/foo.jpg`。
- S3 キー = `{S3_PREFIX}/{logical key}`。`ObjectStore` 実装（`s3_object_store.rs`）が prefix を付けて `PutObject` / `GetObject` / `HeadObject` / `DeleteObject` / `ListObjectsV2` に写像する。
- 一覧は `ListObjectsV2` の `ContinuationToken` を既存の `ObjectListRequest.cursor` に流す（`list_page` の意味論を維持）。
- 単一 PUT を基本とする（section は数十 KB、挿絵は数 MB、EPUB は 1 冊数 MB）。multipart は 64 MiB 超が出てから P4 で扱う。
- `Content-Type` は挿絵/EPUB のみ設定し、それ以外は `application/octet-stream`。
- SigV4 は `x-amz-content-sha256: UNSIGNED-PAYLOAD` を HTTPS で使い、大きい body を再ハッシュしない（クライアント側で body を二度読まない）。

### 2.3 保存先の切替と移行

- `app_state` に `object_backend` = `d1` | `s3` を持ち、**P0 の移行期間だけ両方を読める**ようにする。切替はフラグ 1 つで戻せる（ロールバック手段）。
- 既存の D1 `objects` / `object_chunks` → S3 の移行は、ページング + 再開可能なジョブとして実装する。検証は「キー集合の一致」＋「各オブジェクトのバイト一致（crc32 比較）」。移行完了後の D1 側は削除せず、フラグを戻せば即座に旧経路へ復帰できる状態を一定期間残す。

### 2.4 認証

- API/CI は既存の `NAROU_ADMIN_TOKEN`（Bearer、定数時間比較）を継続。
- ブラウザは token cookie（`narou_api_token`、HttpOnly / SameSite=Lax）を追加し、書き込み系は同一オリジン検査（native の Host/Origin 検証の相当物）を維持する。
- secret 未設定時は **fail-closed**（401 ではなく 500 `authentication_not_configured` として設定不備を可視化する）。

### 2.5 WebSocket push

- `PUSH_HUB`（DO）が 1 ライブラリ 1 インスタンスで接続を保持し、`fetch` ハンドラと queue consumer から HTTP 経由で publish する。
- イベント名・ペイロードは native の push（`echo` / `log` / `table.reload` / `queue` / `progressbar.*` 等）と**同一**にして、frontend JS を無改修で動かす。

### 2.6 Workers で提供しない機能（明示的に拒否する）

| 機能 | 挙動 |
|---|---|
| `send` / `mail`（SMTP）/ MOBI・Kindle 出力 | API は `501`、job は理由付き `blocked`（外部プロセス・TCP SMTP が前提） |
| AozoraEpub3 jar / kindlegen / 外部 diff ツール | 同上。EPUB は Lite で代替済み |
| `folder` / `browser` / `login`（ブラウザ起動）/ タスクトレイ | `501` |
| `shutdown` / `reboot` / self-update / `narou db` 保守 | `501`（Workers のライフサイクル外） |
| APNG 挿絵 | 保存は継続（zip のまま）。native で convert した場合のみ APNG 化 |

---

## 3. 参考実装 (Dantalian) から輸入する型

| 型 | 輸入する内容 |
|---|---|
| 環境分離 | `wrangler.<target>.toml` をテンプレート化し、CI が置換して `wrangler.ci.toml` を生成（生成物は gitignore）。置換前に name 正規表現 / UUID / secret 名 / 未解決 `__` を検証 |
| プロビジョニング | D1 と Queue(+DLQ) を list→create→再 list の冪等手順で用意し `GITHUB_OUTPUT` で受け渡す（`database_id` をリポジトリへ書かない） |
| デプロイ | develop=push / staging=push main / production=`v*` タグ + タグが main の祖先である検証。`needs: [native, worker]`、target 別 `concurrency`（cancel なし）、**デプロイ前に `wrangler d1 migrations apply --remote`**、`wrangler deploy --secrets-file`、`if: always()` で生成物削除 |
| アセット | `[assets]` + `build_assets.mjs`（ハッシュ付与・変更なしスキップ） |
| 契約テスト | `wrangler dev --local` + `node --test tests/*.mjs` を `WORKER_BASE_URL` 駆動。**トークン未設定時に fail-closed になること**も検証。`/cdn-cgi/local/scheduled` で cron を叩ける |
| 自己修復 | `scheduled` でリース期限切れの回収と再 dispatch を回す |
| ドキュメント | `DEPLOYMENT_CHECKLIST` / `MIGRATION_RUNBOOK` の節立てと検証記録 |

輸入しないもの: S3 実装そのもの（Dantalian は Wasabi 専用の SIGv4 クライアントを持つが、narou.rs は S3 互換として汎用に書く）、CF Access を唯一のユーザー境界にする設計、音声向けの instance_type / batch 値、FDK-AAC のライセンス機構。

---

## 4. フェーズ計画と受け入れ条件

| Phase | 内容 | 受け入れ条件 |
|---|---|---|
| **P0 足回りと保存基盤** | ① `wrangler.<target>.toml`（develop/staging/production）+ `ci/render_config.py` + `ci/provision_resources.py` ② CI に worker のテスト・migrations apply・deploy を追加 ③ S3 クライアント（SigV4）+ `ObjectStore`/`AssetStore` の S3 実装 ④ `object_backend` フラグと D1→S3 移行ジョブ ⑤ 契約テスト骨組み（health / 認証 fail-closed / object round-trip） | 3 環境へ `wrangler deploy` でき、契約テストが green。既存 D1 オブジェクトを S3 へ移行して往復バイト一致、フラグで旧経路へ戻せる |
| **P1 取得系を閉じる** | ① SSRF 検証の port 化（core は静的検査、DNS は adapter 側 or 新 port） ② `DownloaderSettings` を D1 読みに ③ `CookieStore`(D1) を注入 ④ `SiteDefinitionProvider` をストア経由に（ユーザー YAML 差し替えの回復） ⑤ `setting_core` の Directory 検証を port 化 | ログイン必須サイトを含む DL/更新が Worker で完走し、native と同じ `小説データ` 内容（キー集合と本文バイト一致）になる |
| **P2 変換を Worker へ** | ① `converter/**` の feature 分割（`device` / `inspector` / `settings` を native gate へ。純粋な変換本体と `ConverterCapabilities` を worker へ） ② `JobKind::Convert` を worker-executable に ③ 変換結果（`novel.txt`）と生成 EPUB を S3 へ保存 ④ 挿絵のローカライズ経路を通す | Worker 単独で download → convert → EPUB が閉じる（409 が消える）。native と同一の変換出力（既存の parity fixture で確認） |
| **P3 Web UI 移植** | ① ルート移植（list / novels / tags / freeze / batch / settings / queue / notepad / log / diff 等） ② `[assets]` で frontend 配信 ③ `/ws` を `PUSH_HUB` で実装（イベント名は native 互換） ④ token cookie 認証 + 同一オリジン検査 ⑤ 非対応機能の `501` | ブラウザから一覧・タグ・凍結・設定・キュー・ジョブ進捗・EPUB DL が native UI と同等に操作でき、非対応操作は明示エラーになる |
| **P4 運用** | ① ロールバック手順（`wrangler versions` + `object_backend` + S3 prefix 切替） ② D1/S3 の容量・コスト設計と監視 ③ バックアップ経路の再定義 ④ `DEPLOYMENT_CHECKLIST` / `MIGRATION_RUNBOOK` 相当の文書 ⑤ multipart アップロード（必要になった場合） | runbook に沿って前バージョンへ戻せる。検証記録が残る |

各 Phase は独立 commit 単位。P1 完了までは「Worker はまだ本番運用しない」前提を維持する。

---

## 5. リスクと緩和

| リスク | 緩和 |
|---|---|
| SigV4 実装の誤り（時刻ずれ・payload hash・path encoding） | `UNSIGNED-PAYLOAD` + HTTPS、既知の S3 互換実装（MinIO / 本番 Wasabi endpoint）に対する契約テスト、失敗時は `object_backend=d1` へ即時切替 |
| S3 資格情報の漏洩・ログ混入 | Secrets Store 優先 + `env.secret` フォールバック、`Debug` 実装でマスク、CI のログに値を出さない（`--secrets-file`）、`if: always()` で生成物削除 |
| D1 容量・行サイズ制限 | オブジェクトは S3 へ（本計画の決定）。D1 にはメタと台帳のみ。`objects` テーブルは移行完了後に縮小 |
| Worker の CPU / subrequest 上限（EPUB 生成・大量取得） | `cpu_ms = 300000` を維持、`WorkerBudget` の section 境界 yield を継続、EPUB は prefetch 上限（512 枚 / 64 MiB）を維持 |
| cron 毎分の計画コスト | 100 件ページ + generation/lease の早期 exit を維持。必要なら間隔を緩める |
| 移行中の二重書き・欠落 | `object_backend` フラグ + 移行ジョブの再開カーソル。検証はキー集合と crc32 の一致 |
| GPL 整合（worker は `lite` を含む） | `worker_entry/Cargo.toml` の license 表記と CI の license job を維持。配布条件は P4 で整理 |
| native との挙動差が残ったまま移行する | P1 の受け入れ条件に「native と同じキー集合・バイト一致」を入れる。設定差は P1 で解消 |

---

## 6. 検証コマンド

```bash
# ローカル
npx wrangler d1 migrations apply narou-local --local --config worker_entry/wrangler.toml
npx wrangler dev --local --config worker_entry/wrangler.toml --var NAROU_AUTH_REQUIRED:true
node --test worker_entry/tests/*.mjs        # WORKER_BASE_URL / NAROU_ADMIN_TOKEN を env で渡す

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

## 7. 未解決・後続判断

- **S3 のバケット構成**: 1 バケット + prefix（環境同居）を既定とする。本番だけ別バケットにするかは P0 の実測後に決める。
- **D1 の `objects` テーブルの扱い**: 移行後も一定期間は残す。削除（容量回収）は P4 の判断。
- **APNG 挿絵**: wasm で zip 展開ができないため保留。`miniz_oxide` 直叩きで frames だけ展開する案は P4 以降。
- **バージョン履歴 / diff**: D1 にテーブルが無いため、P3 で `diff` を出すなら先に D1 スキーマを追加する。当面は `501`。
- **ユーザー YAML の差し替え**: P1 でストア経由の読み込みに戻すが、UI から編集させるかは別判断。
