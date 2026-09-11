# SQLite 管理基盤移行計画 (2026-08-25)

## 実装状況 (2026-08-25): **P0〜P4b 完了 / P5 一部**

| Phase | 状態 |
|---|---|
| P0 監査 | ✅ docs/sqlite-p0-record-mapping.md + tests/golden |
| P1 エンジン | ✅ src/native/sqlite/ dual-run テスト9件 |
| P2 メタデータ切替 | ✅ 自動import+rename退避 / narou db verify\|export-yaml\|vacuum / queue・notepad・settings 経由化 |
| P3 既定化 | ✅ Database::new が db.sqlite を既定使用。YAML非書込テスト & perf smoke(1000件) 追加 |
| P4a コンテンツ | ✅ novel_sections/novel_outputs ミラー (convert時)。Web DL時EPUBはDB優先。sectionsの全経路DB化は段階継続 |
| P4b 差分履歴 | ✅ §11テーブル実装 + diff CLI拡張 (--history/--show/--restore/--merge-from/--merge-sections) + prune。**update時自動snapshotはconvertフック経由**、Web api_diff拡張と行レベル3-wayマージは未着手 |
| P5 清算 | ◐ AGENTS/COMMANDS更新・version bump。Inventory削減は残置(legacy import用に維持) |

### 計画からの逸脱 (意図的)
1. `queue.yaml` → worker_jobs 型テーブルではなく app_state('inv','queue') へペイロード格納 (単一ライブラリ前提で十分・既存ロジック無変更)
2. `latest_convert` → novels.last_convert_at 列ではなく app_state マップ
3. HTTPレスポンスは全体バッファ後返却 (Lite自体はチャンク書出対応)

## 0. ポリシー変更の定義

本計画はプロジェクトの互換性ポリシーを以下のように変更する (**AGENTS.md の Porting Policy / 互換性要件の改訂を含む**)。

| 種別 | 従来 | 変更後 |
|---|---|---|
| **後方互換性** (新 narou.rs が旧データを読める) | 維持 | **維持** — 既存の `.narou/*.yaml` ライブラリ・設定からの自動取り込みを保証する |
| **前方互換性** (旧ツール = Ruby 版 narou.rb や旧 narou.rs が新データを読める) | 一部要求 (YAML 意味論一致) | **破棄** — SQLite 化以降、旧ツールからの可読性は保証しない |

- 捨てるのは「データ形式の相互運用」のみ。**CLI 引数・出力メッセージ・終了コード・Web UI の外部挙動は一切変えない**。
- `webnovel/*.yaml` (サイト定義)・`replace.txt`・`setting.ini` 等の**ユーザーが直接編集する意味のあるファイルは対象外**(YAML 駆動サイト定義ポリシーは維持)。移行対象は「プログラム管理の状態」に限定する。

## 1. 対象データと移行先

### 1.1 メタデータ層 (Phase P2)

| 現行ファイル | SQLite 移行先 | 備考 |
|---|---|---|
| `database.yaml` | `novels` (33+ 列) | Worker D1 の `0001_core`〜`0003_status_sort` スキーマをほぼ踏襲 |
| `database_index.yaml` (SHA256 fingerprint) | 廃止 | SQLite 自身がインデックスを持つ (`index_store.rs` 役割終了) |
| ID 採番 | `novel_id_sequence` | D1 と同一 (atomic allocate) |
| `freeze.yaml` (+lock) | `frozen_novels` | fs2 lock 不要化 |
| タグ | `novel_tags` (position, tag, tag_fold) | `database.rs` の tag index 置換 |
| `tag_colors.yaml` | `app_state('tag_colors','colors')` | D1 実装と同一キー |
| `alias.yaml` | `app_state('alias', ncode→id)` | |
| `latest_convert.yaml` | `novels.last_convert_at` 列追加 (0007) | |
| `notepad.txt` | `app_state('notepad','text')` | |
| `local_setting.yaml` | `app_state('local', …)` | 初回起動時に import、以後 DB が truth |
| `~/.narousetting/global_setting.yaml` | `app_state('global', …)` | 同上 (パスは読み取り専用の legacy 入力へ格下げ) |
| `queue.yaml` | `jobs` (worker_jobs と同構造) | `PersistentQueue` を SQLite 実装へ差し替え。retry backoff / lease も列で管理 |
| `.illustration_cache.yaml` (+index) | `novel_illustrations` (0008) | blob 本体は引き続き ObjectStore (FS) |

### 1.2 コンテンツ層 (Phase P4 — 別途詳細設計)

| 現行 | 方針 |
|---|---|
| `toc.yaml` / `本文/*.yaml` (section) | `novel_sections(novel_id, index, subtitle, body_yaml)` へ段階移行。per-novel lazy migration |
| `diff.txt` (単一最新差分) | **バージョン履歴へ置換** → §11 `novel_versions` / `novel_version_sections` / `novel_version_diffs`。既存 diff コマンド出力は互換維持 |
| `novel.txt` ミラー / 生成 txt | `novel_outputs` (BLOB) — lite DL 経路の `converted_text()` キーと統合 |
| 挿絵画像 / 表紙 | FS 維持 (AssetStore 契約は不変) |
| `setting.ini` / `replace.txt` / `cookies-ingest.yaml` / `server.pid` | FS 維持 (ユーザー編集・ランタイム固有) |

## 2. アーキテクチャ

```
src/platform/repository.rs        (trait 不変)
src/native/sqlite/                (NEW: rusqlite ラッパ)
  mod.rs        Connection 管理 (WAL, busy_timeout=5s, foreign_keys)
  schema.rs     migrations (user_version, D1 0001-0006 と整合する番号)
  records.rs    NovelRepository 実装 (D1 d1_repository.rs の SQL を共有モジュール化)
  settings.rs   SettingsStore / TagColorStore 実装
  queue.rs      PersistentQueue 実装
  migrate.rs    legacy YAML → SQLite import (トランザクショナル・冪等)
  export.rs     `narou db export-yaml` (ロールバック用)
```

- **D1 との SQL 共有**: `src/shared_sql.rs`(仮) に UPSERT/検索 SQL を定数化し、native(rusqlite)/Worker(D1) の両アダプタから参照。プレースホルダは両者互換の `?NNN` 形式に統一し、D1 非対応構文 (部分インデックス等) は使用しない。
- `db::DATABASE` singleton と `refresh()` は subprocess 実行後の再読込用途だが、SQLite では接続毎に最新値が見えるため**廃止方向** (P5)。過渡期は `with_database` 内部だけ差し替え。

## 3. 後方互換性の保証 (守るもの)

1. **自動取り込み**: 起動時に `novels.db` が無く legacy YAML があれば、サイレントに 1 回 import (全処理単一トランザクション)。完了後、元 YAML は `*.yaml.imported-<ts>` へ退避 (削除しない)。
2. **外部挙動不変**: CLI の引数/出力/終了コード、Web API、`--multiple` 等のグローバル機能は現状維持。差分は COMMANDS.md で ✅ を維持できなければならない。
3. **ロールバック**: `narou db export-yaml [--out DIR]` で常に legacy 形式を再生成できる。旧バージョンへ戻す場合は export → 旧版起動、という手順を README に明記。
4. **混在検知**: DB 使用中に legacy YAML が再出現した場合は警告して無視 (取り込まない)。逆に `--legacy` フラグ付き起動では DB を読まず YAML を読むデバッグ経路をテスト用に残す。

## 4. 前方互換性の破棄 (捨てるもの)

- `database.yaml` 等への**新規書き出しは廃止** (export-yaml のみ)。Ruby 版 narou.rb・旧 narou.rs からの DB 可読性は保証外。
- `Inventory` の atomic write + fs2 lock パターン、`IndexStore` SHA256 fingerprint、Windows retry ループの YAML 前提コードは削減対象。
- 上記に伴い **AGENTS.md の「Init / Local Data Compatibility」「互換性の要件レベル」節を本計画確定時に改訂**し、Serena memory (`porting_status`) も更新する。

## 5. 技術選定

| 項目 | 決定 | 理由 |
|---|---|---|
| クレート | `rusqlite` (feature `bundled`) | デファクト。bundled でシステム sqlite3 依存を排除 |
| 追加方法 | `cargo add rusqlite --features bundled` → features 節は手編集 (Dependency Policy の例外理由: feature gate 必須のため) | |
| 配置 | `db.sqlite` (名前仮) を archive root 直下 | 単一ファイル配布・backup 容易 |
| ジャーナル | WAL + `synchronous=NORMAL` | tray/web/CLI の複数プロセス同時接続に耐える |
| 同時書込 | busy_timeout + 書込は既存 queue lane の直列性を尊重 | fs2 lock 廃止 |

## 6. マイグレーション設計

- `PRAGMA user_version` による番号管理。migration SQL は `src/native/sqlite/migrations/0001_*.sql`… で D1 の内容を移植しつつ native 固有列 (last_convert_at 等) を 0007 以降に追加。
- 各 migration は冪等 (IF NOT EXISTS) + トランザクション。**クラッシュ安全**: import 中断時は user_version が進まないため次回起動で最初からやり直せる (YAML 原本は退避せず、import 成功後に退避)。
- 起動シーケンス: `init_database()` → open → user_version 確認 → migration → legacy 検知/取込 → ready。失敗時はエラー表示して従来 YAML モードで起動 (起動不能にしない)。

## 7. フェーズ計画と受け入れ条件

| Phase | 内容 | 受け入れ条件 |
|---|---|---|
| **P0 監査** | NovelRecord 45列 ↔ `novels` 列の対応表確定、golden fixture (sample/novel + WebNocel 由来の匿名化ライブラリ) 収集 | 対応表がレビュー済みで tests/golden/ に固定データあり |
| **P1 エンジン導入** | rusqlite 追加、schema migrations、`NativeNovelRepository` の SQLite 実装を trait の裏側として追加 (切替はしない) | 既存全テストが YAML 経由で pass、新実装が同じ golden で同一結果 (dual-run テスト) |
| **P2 メタデータ切替** | settings/freeze/alias/tag colors/queue/notepad/latest_convert も SQLite 化、auto-import 実装、`narou db` サブコマンド (verify/export-yaml/vacuum) | golden library で import → 全コマンド操作 → export が元 YAML と意味論一致。Web UI 全 API の結合テスト pass |
| **P3 既定化** | 新規 `narou init` は SQLite を既定に。YAML writer を export 専用へ。perf 計測 (list/sort/tag 一括 1000 件) | 既定フローで YAML を書かないことが assert され、性能劣化なし (目標: 全操作 YAML 比 2x 以内) |
| **P4a コンテンツ** | sections/toc/novel_outputs の DB 化 (lazy migration)、lite EPUB DL 経路を DB 直読みへ | per-novel 移行が中断再開可能。EPUB byte 等価テスト維持 |
| **P4b 差分履歴** | §11 のバージョン管理実装: update 時の自動 snapshot + unified diff 保存、`narou diff` の履歴/復元/マージ拡張、Web API (`api_diff` 履歴化、restore/merge エンドポイント追加) | 更新→差分→rollback→merge の一連操作が golden 上で可逆。既定 `diff` 出力は現行と同一 |
| **P5 清算** | Inventory/IndexStore/legacy_persistence の縮小、AGENTS.md・COMMANDS.md・memory 更新、minor version bump (0.4.0) | cargo check/test/clippy 全 green、docs 整合 |

各 Phase は独立 commit 単位。P2 完了までは既定動作を一切変えない (リスクゼロ着地)。

## 8. リスクと緩和

| リスク | 緩和 |
|---|---|
| Windows の AV/索引による db ファイル掴み | WAL + busy_timeout、open retry (既存 Windows retry ロジックを接続層へ移植) |
| DB 破損 | migrate 前自動 backup (`novels.db.bak-<ts>` 1 世代)、`narou db verify` (=integrity_check)、export-yaml による脱出路 |
| 複数プロセス (tray/web/queue 子プロセス) の書込競合 | 単一 writer 原則 (queue lane 直列を利用) + BEGIN IMMEDIATE |
| bundled sqlite の cross compile (armv6/7) | release CI の cross ビルドで早期検出、失敗時は `libsqlite3-sys` の系統調整 |
| 大規模ライブラリでの移行時間 | import は prepared batch + 単一 tx。10k 件 < 30s 目標、超えたら chunked commit に切替 |

## 9. テスト計画

- **golden round-trip**: fixture library → import → 操作 (tag add/remove, freeze, sort, search, convert) → export → 意味論比較
- **crash safety**: migration 中 kill をシミュレート (tx 未確定) → 再起動で完遂
- **並行**: web server + CLI が同時に write するシナリオ (busy_timeout 動作確認)
- **互換**: `--legacy` 読み取り経路で旧 YAML が今も読めることを固定
- 既存 `tests/convert_parity.rs` 等 EPUB/変換系は完全に影響を受けないことを確認

## 10. 影響ファイル (主要)

`src/db/*` (大幅縮小), `src/native/{novel_repository,legacy_persistence}.rs`, `src/queue.rs`, `src/web/{global_settings,misc,jobs}.rs`, `src/application/jobs.rs`, `worker_entry/d1_repository.rs` (SQL 共有化), `Cargo.toml`, `AGENTS.md`, `COMMANDS.md`

P4b 追加分: `src/commands/diff.rs` (--list/--show/--restore/--merge-from), `src/commands/update.rs` + `src/downloader/mod.rs` (snapshot フック), `src/web/jobs.rs` (api_diff 拡張・restore/merge エンドポイント), D1 migration 追加

## 11. 差分履歴・マージ・ロールバック (P4b 詳細設計)

### 11.1 スキーマ (native 0009 / D1 migration 同時追加)

```sql
CREATE TABLE novel_versions (
  id          INTEGER PRIMARY KEY,
  novel_id    INTEGER NOT NULL REFERENCES novels(id),
  parent_id   INTEGER REFERENCES novel_versions(id),
  origin      TEXT NOT NULL CHECK (origin IN ('update','manual','rollback','import')),
  note        TEXT,
  section_count INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL
);
CREATE INDEX novel_versions_novel_idx ON novel_versions(novel_id, id DESC);

CREATE TABLE novel_version_sections (
  version_id INTEGER NOT NULL REFERENCES novel_versions(id) ON DELETE CASCADE,
  idx        TEXT NOT NULL,
  subtitle   TEXT,
  body_yaml  TEXT NOT NULL,           -- 既存 SectionFile codec を再利用
  PRIMARY KEY (version_id, idx)
) WITHOUT ROWID;

CREATE TABLE novel_version_diffs (
  version_id     INTEGER PRIMARY KEY REFERENCES novel_versions(id) ON DELETE CASCADE,
  prev_version_id INTEGER,
  unified_diff   TEXT NOT NULL         -- `similar` の unified 出力をそのまま保存 (diff コマンド互換)
) WITHOUT ROWID;
```

- 作業セットは P4a の `novel_sections` (現行の「今の本文」)。version 行は**イミュータブル**な過去スナップショット。
- **保持数上限**: 設定 `diff.history-limit` (既定 10)。新規 version 挿入時に超過分を古い方から削除。`unified_diff` は自己完結しているため参照先 version が消えても表示可能。
- サイズ感: 小説 1 本の section 総量 ≈ 0.1〜5 MB、既定 10 版で最大 ~50 MB/本。上限設定と `narou db vacuum` で管理。

### 11.2 更新フロー (downloader 統合)

```text
update 実行 → 差分検出 (既存ロジック)
  └─ 変更あり:
       BEGIN IMMEDIATE
         1. 現行作業セットを snapshot として INSERT (origin='update', parent=前回head)
         2. 新 sections を作業セットへ適用
         3. unified diff を計算して novel_version_diffs へ
       COMMIT            -- diff.txt への書き出しは廃止 (読み取りは DB)
```

### 11.3 CLI / Web 表面 (後方互換)

| 操作 | 従来 | 変更後 |
|---|---|---|
| `narou diff <id>` | diff.txt の最新差分表示 | **同一出力**を head version の unified_diff から表示 |
| `narou diff <id> --list` | (なし・追加) | version 一覧 (`id, origin, created_at, sections, summary`) |
| `narou diff <id> --show <ver>` | (追加) | 指定版の unified diff 表示 |
| `narou diff <id> --restore <ver>` | (追加) rollback | 対象版の sections を**コピーして新 head にする** (非破壊。`--drop-newer` で後続版 pruning を明示指定時のみ) |
| `narou diff <id> --merge-from <ver> [--section N,...]` | (追加) merge | 指定版 (省略時は全指定 section) を現行へ上書き複写し新 head 作成 |
| Web `POST api_diff` | 単一差分 JSON | 最新差分 + 履歴リスト (フィールド追加のみ・既存キー不変) |
| Web `api_diff_clean` | diff.txt 削除 | 履歴全削除 (= prune all)。挙動名は維持 |

- **マージの粒度**: v1 は section 単位の選択的複写 (競合概念なし・常に新 head 生成で取り消し可能)。行レベル 3-way マージ (`similar::merge` 等) は v2 検討項目。
- **ロールバックも copy-forward**: head を付け替える破壊的移動は行わないため、誤操作は直ちに再 rollback で復元できる。

### 11.4 Worker / D1

同じ 3 テーブルを D1 migration として追加。Worker crawler が update 時に自動 snapshot (§11.2 と同フロー) を書き、Web DL 時 EPUB は head version を使えるため「DL するだけで過去版 EPUB」も可能になる (`?version=` クエリ拡張は任意)。

### 11.5 追加テスト

- 更新 2 回 → versions=2、head diff が旧 `diff.txt` 生成物と文字列一致
- rollback → 本文が対象版と一致、かつ新 version が作られていること
- merge (--section) → 非指定 section が不変であること
- history-limit 超過で最古 version が削除され、残存 diff 表示が壊れないこと

## P6: 0.4.0 デュアルモード + 移行ツアー (2026-08-25 追加実装)

ユーザー要件により P3 の「SQLite 既定」を改変:

- **既定 = 従来どおり YAML 管理**。Lite(SQLite) は完全なオプトイン
- 切替は `.narou/storage-backend` マーカーファイル(`sqlite`/`yaml`)で表現され、抽象化層 `state::active_for(narou_dir)` が全経路(database/inventory/queue/notepad/compat/EPUB-DL/converter hook)の単一判定点となる
- **移行プロンプト**: Web UI 機能ツアー(0.4.0 エントリ)表示時に一度だけ「Lite版へ移行 / YAML継続」を問う。`GET/POST /api/storage/mode` が状態照会・切替(marker書込+即時再init+自動import)を担う。CLI でもマーカー作成で同等
- `POST mode=sqlite` はその場で `init_database()` をやり直し、レガシー import(元ファイル rename 退避)まで完了する
