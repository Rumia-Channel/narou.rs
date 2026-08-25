# SQLite 管理基盤移行計画 (2026-08-25)

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
| `raw/` 生 HTML | FS/ObjectStore 維持 (diff 用途、体積大) |
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
| **P4 コンテンツ** | sections/toc/novel_outputs の DB 化 (lazy migration)、lite EPUB DL 経路を DB 直読みへ | per-novel 移行が中断再開可能。EPUB byte 等価テスト維持 |
| **P5 清算** | Inventory/IndexStore/legacy_persistence の縮小、AGENTS.md・COMMANDS.md・memory 更新、minor version bump (0.4.0) | cargo check/test/clippy 全 green、docs 整合 |

各 Phase は独立 commit 単位。P2 完了までは既定動作を一切変えない (リスクゼロ着地)。

## 8. リスクと緩和

| リスク | 緩和 |
|---|---|
| Windows の AV/索引による db ファイル掴み | WAL + busy_timeout、open retry (既存 Windows retry ロジックを接続層へ移植) |
| DB 破損 | migrate 前自動 backup (`novels.db.bak-<ts>` 1 世代)、`narou db verify` (=integrity_check)、export-yaml による脱出路 |
| 複数プロセス (tray/web/queue 子プロセス) の書込競合 | 単一 writer 原則 (queue lane 直列を利用) + BEGIN IMMEDIATE |
| D1 方言差で SQL 共有が破綻 | 共有 SQL は両 runtime の CI で dual-execute テスト |
| bundled sqlite の cross compile (armv6/7) | release CI の cross ビルドで早期検出、失敗時は `libsqlite3-sys` の系统調整 |
| 大規模ライブラリでの移行時間 | import は prepared batch + 単一 tx。10k 件 < 30s 目標、超えたら chunked commit に切替 |

## 9. テスト計画

- **golden round-trip**: fixture library → import → 操作 (tag add/remove, freeze, sort, search, convert) → export → 意味論比較
- **crash safety**: migration 中 kill をシミュレート (tx 未確定) → 再起動で完遂
- **並行**: web server + CLI が同時に write するシナリオ (busy_timeout 動作確認)
- **互換**: `--legacy` 読み取り経路で旧 YAML が今も読めることを固定
- 既存 `tests/convert_parity.rs` 等 EPUB/変換系は完全に影響を受けないことを確認

## 10. 影響ファイル (主要)

`src/db/*` (大幅縮小), `src/native/{novel_repository,legacy_persistence}.rs`, `src/queue.rs`, `src/web/{global_settings,misc,jobs}.rs`, `src/application/jobs.rs`, `worker_entry/d1_repository.rs` (SQL 共有化), `Cargo.toml`, `AGENTS.md`, `COMMANDS.md`
