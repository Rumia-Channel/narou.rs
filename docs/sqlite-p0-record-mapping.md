# P0: NovelRecord ↔ `novels` 列対応表 (2026-08-25)

SQLite 移行 P0 成果物。列定義は `worker_entry/migrations/0001_core.sql` + `0003` (`status_sort`) + `0004` (`extra_fields_yaml`)、native 側は `src/native/sqlite/migrations/` に同内容を同梱。

## novels テーブル

| NovelRecord フィールド | 列 | 型 | 変換規則 |
|---|---|---|---|
| id | id | INTEGER PK | そのまま |
| author | author / author_fold | TEXT | fold = trim + lowercase |
| title | title / title_fold | TEXT | 同上 |
| file_title | file_title | TEXT | そのまま |
| toc_url | toc_url (+UNIQUE) / toc_url_fold | TEXT | fold |
| sitename | sitename / sitename_fold | TEXT | fold |
| novel_type (u8) | novel_type | INTEGER NOT NULL DEFAULT 0 | i64 |
| end (bool) | end | INTEGER 0/1 | flag |
| last_update (DateTime) | last_update | TEXT NOT NULL | RFC3339 nanos (`Z`)。書込時 None 不可 → 空文字は不可 |
| new_arrivals_date (Option) | new_arrivals_date | TEXT NULL | RFC3339。**UPSERT は plain ? で空文字=NULL 扱い** (読み取り側で空文字→None) |
| use_subdirectory (bool) | use_subdirectory | INTEGER | flag |
| general_firstup (Option) | general_firstup | TEXT NULL | 同上 (空文字規約) |
| novelupdated_at (Option) | novelupdated_at | TEXT NULL | 同上 |
| general_lastup (Option) | general_lastup | TEXT NULL | 同上 |
| last_mail_date (Option) | last_mail_date | TEXT NULL | 同上 |
| tags: Vec<String> | tags_json (NOT NULL '[]') + **novel_tags** 正規化テーブル (position, tag, tag_fold) | TEXT + 行 | JSON 配列は検索用の非正規化ミラー。tag 検索/Status 式は novel_tags を参照。Upsert 時 DELETE+再 INSERT |
| — (派生) | tags_fold / tags_sort | TEXT | tag の fold を `\n` / `\u{1f}` 連結したソートキー |
| ncode (Option) | ncode / ncode_fold | TEXT NULL | **NULLIF(?, '')** — 空文字バインドで NULL 化 |
| domain (Option) | domain / domain_fold | TEXT NULL | 同上 |
| general_all_no (Option<i64>) | general_all_no | INTEGER NULL | **NULLIF(?, -1)** |
| length (Option<i64>) | length | INTEGER NULL | 同上 |
| suspend (bool) | suspend | INTEGER | flag |
| is_narou (bool) | is_narou | INTEGER | flag |
| last_check_date (Option) | last_check_date | TEXT NULL | 空文字規約 |
| convert_failure (bool) | convert_failure | INTEGER | flag |
| extra_fields: BTreeMap<String,YamlValue> | extra_fields_yaml (NOT NULL '{}') + extra_fields_bytes | TEXT + INTEGER(長さ) | serde_yaml 文字列。上限 64 KiB (超過でエラー) |
| — (派生, 0003) | status_sort | TEXT NOT NULL '' | 完結/削除/中断ラベル。tags/freeze/end/suspend 更新時に `UPDATE` で再計算 |

### NovelRecord の残りフィールド

`NovelRecord` (45 フィールド) のうち上記以外は**コンテンツ/派生情報**でありメタデータ DB に含めない:
`toc.yaml` 由来の本文系・`raw/`・挿絵キャッシュ等は P4a の `novel_sections` / `novel_outputs` / FS(ObjectStore) 契約へ。

## その他テーブル

| テーブル | 役割 | D1 migration |
|---|---|---|
| novel_tags | タグ正規化 (PK(novel_id,position), UNIQUE(novel_id,tag)) | 0001 |
| frozen_novels | 凍結 (freeze.yaml 相当)。fs2 lock 不要化 | 0001 |
| app_state (scope,key,value_json,value_yaml@0004) | settings / alias / notepad / tag_colors / scheduler state | 0001+0004 |
| novel_id_sequence | 単一行 (id=1) の atomic 採番 | 0001 |
| worker_jobs | queue.yaml 相当 (P2 で native jobs として共用) | 0005/0006 (Worker) |

## 実装位置

- スキーマ本体: `src/native/sqlite/migrations/0001..0004.sql` (= worker_entry/migrations から移植)
- マッピング実装: `src/native/sqlite/{record_map.rs, query.rs, repository.rs}`
- SQL 断片 (UPSERT/STATUS式): `src/native/sqlite/sql/*.sql` (D1 ソースから機械抽出、byte 等価)
- dual-run/golden テスト: `src/native/sqlite/repository.rs#tests`, `tests/golden/{records-a.json, library-a/}`
