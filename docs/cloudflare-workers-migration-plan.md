# Cloudflare Workers 移行計画 (2026-09-26 / 改訂: 保存先の設計を差し替え)

v0.4.x 系で進めてきた Worker 対応を「本番で使える状態」まで持っていくための計画。
Phase 1-8 の抽象化（`docs/platform-abstraction.md`）で port の境界は既に引かれており、
本計画はその上に載る**足回り・保存基盤・機能の穴埋め**を扱う。

## 実装状況 (2026-09-26)

| Phase | 状態 |
|---|---|
| P0a 足回り (環境分離・プロビジョニング) | ✅ 完了 (`56d5feb`) |
| P0b 署名と S3 アダプタ | ✅ 完了 (`23338ec`, `ed87fcd`) |
| P0c 保存先の振り分け (SplitStore) | ❌ 未着手 (当初の「全オブジェクト切替」を差し替える) |
| P0d D1→S3 移行 (バイナリのみ) | ◐ 実装済みだが未コミット。振り分け前提に作り直す |
| P0e 契約テスト + CI デプロイ | ❌ 未着手 |
| P1 取得系を閉じる | ❌ 未着手 |
| P2 変換を Worker へ | ❌ 未着手 |
| P3 Web UI 移植 | ❌ 未着手 |
| P4 運用 | ❌ 未着手 |

改訂の要点: 保存先を「**オブジェクト全体を D1 か S3 のどちらかに置く**」から
「**データ種別ごとに置き場を固定し、バイナリだけを D1/S3 で切り替える**」へ変更した。
当初の全切替 (`ed87fcd` の `object_backend`) は P0c で置き換える。

---

## 0. 決定事項 (2026-09-26)

| 項目 | 決定 | 補足 |
|---|---|---|
| **ゴール** | **Web UI ごと Workers へ移行する**。native は CLI と、Workers で代替できない重量処理・ローカル操作のために残す | UI 移植は P3 |
| **重量処理** | **Worker 内の AozoraEpub3_Lite (in-process) で完結**。外部プロセス前提の機能（AozoraEpub3 jar / kindlegen / SMTP / 端末送信 / セルフアップデート等）は **明示的に `blocked` / `501`** とし、黙って失敗させない | CF Containers は使わない |
| **保存** | **メタデータは D1、大きいバイナリは S3 互換ストレージ**（本番の接続先は Wasabi を想定） | 詳細は §1 |
| **命名** | コード・binding・設定キーは `S3_*` / `s3_*`。ベンダ名を識別子に使わない | R2 / MinIO でも同じ経路 |

補足: Worker 成果物は `worker-runtime` が `lite` を含むため **GPL-3.0-only**（`worker_entry/Cargo.toml:5-9`）。

---

## 1. データの所在

### 1.1 種別ごとの置き場

| 種別 | 論理キー / テーブル | 置き場 | 理由 |
|---|---|---|---|
| 小説メタデータ | `novels` | **D1** | 一覧・検索・ソート・絞り込みの対象。SQL で引く |
| タグ・凍結・別名・タグ色 | `novel_tags` / `frozen_novels` / `app_state` | **D1** | メタと同じ整合の単位 |
| 設定 (local/global/tag_colors) | `app_state` | **D1** | 小さい。診断時に SQL で見える |
| ジョブ台帳・実行リース | `worker_jobs` | **D1** | claim/ack の原子性が要る |
| 目次 | `novels/<site>/<title>/toc.yaml` | **D1** | 十数 KB。ダウンロード直後に本文と一緒に書く |
| 本文（話単位） | `.../本文/<index> <subtitle>.yaml` | **D1** | 1 話数十 KB。1 話ごとの書き込みと resume を 1 つの境界で完結させたい |
| raw HTML | `.../raw/*.html` | **D1** | 本文と同じ経路（再取得・差分用） |
| 小説固有設定 | `.../setting.ini` / `.../replace.txt` | **D1** | 小さい。編集 UI から読む |
| 変換済みテキスト | `.../novel.txt` | **D1** | EPUB 生成の入力。テキストは D1 に集約する |
| **挿絵** | `.../挿絵/<filename>` | **S3** | 1 枚数 MB になり得る。D1 の容量・行サイズを圧迫する |
| **生成物 (EPUB/ZIP/うごイラ)** | `generated/<namespace>/<filename>` | **S3**（§8 の確認事項） | 1 冊数 MB のバイナリで再生成可能。D1 に置く理由が薄い |
| サイト別レート制限の状態 | Durable Object storage | **DO** | per-site の直列化 |
| ジョブ配送 | Queue `narou-jobs` (+ DLQ) | **Queue** | at-least-once 配送 |
| Web UI の静的資産 | `[assets]` | **Workers Assets** | P3 |

**挿絵を S3 に出す理由**: D1 は 1 データベース 10 GB・行サイズに制限があり、挿絵は
作品あたり数十 MB になる。EPUB 生成時は `挿絵/` を一覧してまとめて読むだけなので、
S3 の `ListObjectsV2` + `GetObject` で足りる。

**本文を D1 に残す理由**: 1 話が小さく、ダウンロード・更新のたびに「話の追加/書き換え +
レコード更新」を近いタイミングで行う。台帳と同じ場所に置くと、途中再開（checkpoint）と
整合確認が SQL 1 か所で済む。S3 に出すと往復と失敗点が増えるだけで得るものがない。

### 1.2 振り分けの仕組み (P0c)

- core に判定関数を置く: `is_bulk_object_key(&ObjectKey)` は
  **パスに `挿絵` セグメントを含む** または **`generated/` で始まる** なら `true`。
  それ以外（本文・toc・raw・setting・`novel.txt`）は `false`。
  → 論理キーは core の `NovelObjectKeys` / `GeneratedAssetKey` が生成するため、
  判定はレイアウトに閉じており、拡張子やサイト名に依存しない（テストで固定する）。
- core に `SplitStore` を置く: `ObjectStore` と `AssetStore` の両方を実装し、
  キーごとに「テキスト側 (D1)」「バイナリ側 (S3)」へ委譲する。
  `composition` は D1 と S3 の実装を渡して `SplitStore` を 1 つ作るだけにする。
- 切替フラグは **バイナリ側だけ**に効かせる: `app_state('inv','asset_backend')` が
  `s3` ならバイナリを S3、それ以外は D1。**本文は常に D1**(フラグに依存しない)。
- `asset_backend=s3` なのに S3 の設定・資格情報が欠けている場合は起動を失敗させる
  (fail-closed。黙って D1 に落とさない)。

### 1.3 移行とロールバック (P0d)

- 移行対象は `is_bulk_object_key` が真のキーだけ（挿絵・生成物）。本文は動かさない。
- 移行は 1 回 `limit` 件で区切る再開可能なジョブ。進捗（カーソル・件数）は
  `app_state('inv','migrate_objects')` に残す。`verify` は D1 と S3 のバイト一致を確認する。
- **ロールバック**は `asset_backend` を `d1` に戻すだけ。D1 側の `objects`/`object_chunks` は
  移行後も消さない（容量回収は P4 で判断）。

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
| EPUB | `epub_lite::build_book_from_source` を in-process で実行 | `worker_entry/src/lib.rs:196-315` |

### 2.2 本番化を止めている穴

| # | 穴 | 根拠 |
|---|---|---|
| 1 | **SSRF 検証が DNS 解決を要求**し、全 HTTP 経路が通る（wasm には getaddrinfo が無い） | `src/downloader/security.rs:30-36`, `src/downloader/http_policy.rs:214,219,276` |
| 2 | **Convert が Worker に存在しない**。EPUB の前提 `<prefix>/novel.txt` を書く実装が無く常に 409 | `src/lib.rs:14-15`, `worker_entry/src/lib.rs:203-211` |
| 3 | **CookieStore 未注入**（`cookies: None`）でログイン必須サイトは必ず `Blocked` | `worker_entry/src/composition.rs:125-136` |
| 4 | **設定が Worker に届かない**（`WorkerDownloaderSettings` は全既定値） | `src/downloader/settings.rs:76-80` |
| 5 | **デプロイ設定が未完**（`database_id` 未設定・secret 投入手順なし・CI にデプロイ無し） | `worker_entry/wrangler.toml`, `.github/workflows/platform.yml:45-70` |
| 6 | **worker のテストが CI で 1 件も走らない** | 同上 |

### 2.3 その他の差分

- サイト YAML はビルド時埋め込みのみでユーザー差し替え不可（`SiteDefinitionProvider` は空実装）。
- `setting_core` の `VarType::Directory` が `fs::canonicalize` を呼ぶ（`src/setting_core.rs:216,357`）。
- APNG 挿絵（うごイラ）は wasm で無効で zip のまま保存（`Cargo.toml:55`）。
- D1 に content mirror（`novel_outputs` / `novel_sections`）とバージョン履歴テーブルが無い。
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
         ├─ テキスト系 → D1                (サイト別 permit)  (進捗配信)
         └─ バイナリ系 → S3
             (挿絵・生成物)
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

### 3.3 Workers で提供しない機能（明示的に拒否する）

| 機能 | 挙動 |
|---|---|
| `send` / `mail`(SMTP) / MOBI・Kindle 出力 | API は `501`、job は理由付き `blocked` |
| AozoraEpub3 jar / kindlegen / 外部 diff ツール | 同上（EPUB は Lite で代替済み） |
| `folder` / `browser` / `login`(ブラウザ起動) / タスクトレイ | `501` |
| `shutdown` / `reboot` / self-update / `narou db` 保守 | `501` |
| APNG 挿絵 | zip のまま保存（native で convert した場合のみ APNG 化） |

---

## 4. 参考実装 (Dantalian) から輸入する型

| 型 | 輸入する内容 |
|---|---|
| 環境分離 | `wrangler.<target>.toml` をテンプレート化し、CI が置換して `wrangler.ci.toml` を生成（生成物は gitignore）。置換前に形式検証 |
| プロビジョニング | D1 と Queue(+DLQ) を list→create→再 list の冪等手順で用意し `GITHUB_OUTPUT` で受け渡す |
| デプロイ | develop=push / staging=push main / production=`v*` タグ + タグが main の祖先かの検証。`needs: [native, worker]`、target 別 `concurrency`、**デプロイ前に `wrangler d1 migrations apply --remote`**、`--secrets-file`、`if: always()` で生成物削除 |
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
| **P0c 保存先の振り分け** | `is_bulk_object_key`（core、テスト付き）、`SplitStore`（core）、`composition` を SplitStore へ差し替え、`asset_backend` フラグ、当初の全切替 (`object_backend`) の撤去 | 挿絵と生成物だけが S3 に出て、本文・toc・`novel.txt` は D1 に残る（インメモリの mock 2 系統で固定） |
| **P0d 移行** | 未コミットの移行ツールを「バイナリのみ」に限定して作り直し、`verify` とロールバックを備える | 移行 → `verify` 一致 → `asset_backend=s3` で本番稼働、`d1` に戻せば旧経路で読める |
| **P0e 契約テスト + CI** | `worker_entry/tests/*.mjs`（health / 認証 fail-closed / オブジェクト往復）、CI に worker テスト + `d1 migrations apply --remote` + deploy を追加 | ローカルと CI で契約テストが green、デプロイが 3 環境で通る |
| **P1 取得系** | SSRF 検証の port 化、`DownloaderSettings` を D1 読みに、`CookieStore`(D1) 注入、`SiteDefinitionProvider` をストア経由に、`setting_core` の Directory 検証 port 化 | ログイン必須サイトを含む DL/更新が Worker で完走し、native と同じキー集合・本文バイトになる |
| **P2 変換** | `converter/**` の feature 分割（`device`/`inspector`/`settings` を native gate へ）、`JobKind::Convert` を worker-executable に、`novel.txt` と EPUB を保存 | Worker 単独で download → convert → EPUB が閉じる（409 が消える）。native と同一の変換出力 |
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
| Worker の CPU / subrequest 上限 | `cpu_ms = 300000` を維持、`WorkerBudget` の section 境界 yield を継続、EPUB は prefetch 上限（512 枚 / 64 MiB） |
| cron 毎分の計画コスト | 100 件ページ + generation/lease の早期 exit を維持 |
| 移行中の二重書き・欠落 | カーソル再開 + `verify`。D1 側は消さないので即時ロールバック可能 |
| GPL 整合（worker は `lite` を含む） | `worker_entry/Cargo.toml` の license 表記と CI の license job を維持 |
| native との挙動差が残ったまま移行 | P1 の受け入れ条件に「native と同じキー集合・バイト一致」を入れる |

---

## 7. 検証コマンド

```bash
# ローカル
npx wrangler d1 migrations apply narou-local --local
npx wrangler dev --local --var NAROU_AUTH_REQUIRED:true
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

## 8. 未解決・確認事項

- **生成物（`generated/` 配下の EPUB / ZIP / うごイラアーカイブ）の置き場**: 本計画は S3 を既定として
  書いている（大きいバイナリを D1 に置かない）。「画像だけ S3」に限定する場合は §1.2 の判定から
  `generated/` を外すだけで済む。
- **S3 のバケット構成**: 1 バケット + prefix（環境同居）を既定とする。本番だけ別バケットにするかは
  P0e の実測後に決める。
- **D1 の `objects` テーブルの扱い**: 移行後も残す。削除（容量回収）は P4 の判断。
- **APNG 挿絵**: wasm では zip 展開ができないため保留（`miniz_oxide` 直叩きは P4 以降）。
- **バージョン履歴 / diff**: D1 にテーブルが無いため、P3 で `diff` を出すなら先にスキーマを追加する。
- **ユーザー YAML の差し替え**: P1 でストア経由の読み込みに戻すが、UI から編集させるかは別判断。
- **未コミットの移行ツール** (`worker_entry/src/object_migration.rs` + `lib.rs` の配線): P0c の
  振り分けに合わせて「バイナリのみ」へ作り直す前提。それまでの間は作業ツリーに残す。
