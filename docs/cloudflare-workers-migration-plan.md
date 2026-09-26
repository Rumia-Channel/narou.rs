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
| **保存** | **メタデータと本文は D1（YAML/HTML をそのまま置かず、列に展開した形で保存）**、**挿絵（うごイラ含む）だけ S3 互換ストレージ**（本番の接続先は Wasabi を想定） | 詳細は §1 |
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
5. **EPUB ダウンロード**: Worker は `GET /api/novels/{id}/download.epub`
   (`worker_entry/src/lib.rs:196-315`) で、作品メタ → 変換済み本文 → `挿絵/` を**上限つきで先読み**
   （512 枚 / 64 MiB）→ `epub_lite::build_book_from_source` → `stream_epub` でそのまま返す。
   **EPUB は保存しない**。変換済み本文が無ければ 409（現状の未完成点）。native Web UI は
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
| **P0c 挿絵を S3 へ** | core の `is_illustration_key`（`挿絵` セグメント判定、テスト付き）、`SplitStore`（core）、`composition` の差し替え、`asset_backend`（`d1`/`s3`）、当初の全切替 (`object_backend`) の撤去 | 挿絵だけが S3 に出て、他のキーは D1 に残る（mock 2 系統で固定）。`asset_backend` を戻せば旧経路で読める |
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

- **変換済み本文の持ち方**（§1.4）: セクション行の列に持つ前提で書いている。変換が Worker に載る
  （P2）までは、native で変換した値をどう入れるか（移行時に一緒に取り込む / P2 まで DL を 501 に保つ）を決める。
- **raw HTML を保存しない影響**: `diff`（raw 比較）は Workers では提供しない。本文レベルの差分が
  必要になったら履歴テーブルを足す（native の `diff` とは別物として扱う）。
- **S3 のバケット構成**: 1 バケット + prefix（環境同居）を既定とする。本番だけ別バケットにするかは
  P0e の実測後に決める。
- **D1 の `objects`/`object_chunks` の扱い**: 本文の移行後もしばらく残す。削除（容量回収）は P4 の判断。
- **APNG 挿絵**: `zip` の feature を純 Rust 構成に絞ったため Worker でも組み立て可能（`worker-runtime` が
  `illustration-animation` を有効化済み）。フレーム数 × サイズ分の CPU/メモリを使うため、フレーム数と
  合計バイト数に上限を設けて `cpu_ms` 内に収める（Paid プラン前提で運用）。
- **バージョン履歴 / diff**: D1 にテーブルが無いため、P3 で `diff` を出すなら先にスキーマを追加する。
- **ユーザー YAML の差し替え**: P1 でストア経由の読み込みに戻すが、UI から編集させるかは別判断。
- **未コミットの移行ツール** (`worker_entry/src/object_migration.rs` + `lib.rs` の配線): P0d の
  「テキスト blob → 行」「挿絵 blob → S3」に作り直す前提。それまでの間は作業ツリーに残す。
