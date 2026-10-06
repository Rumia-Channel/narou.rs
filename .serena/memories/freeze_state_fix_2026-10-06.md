# Freeze state fix (2026-10-06, issue #35)
- `narou freeze` / `download --freeze` / Web UI の freeze 操作は `Inventory::update_yaml("freeze", InventoryScope::Local)` を通す。YAML 管理は従来どおり `.narou/freeze.yaml`、SQLite 管理は `app_state('inv','freeze')`。ファイル直接書きは SQLite 側の状態を素通りし、次回起動のレガシー取込が断片で在庫を置換して他の凍結を失わせ、`freeze.yaml.imported-*` を増やし続けていた。
- 取込は和集合 (`state::merge_freeze_payload`)。`set_raw_db("inv","freeze")` が payload を `frozen_novels` へ投影し (`repository::sync_frozen_novels`)、`Database::with_inventory` がレコード取込後に `StateDb::resync_frozen_novels` で再投影する (初回 reconcile は `novels` が空の時点で走るため)。
- `frozen_novels` を読むのは状態 *検索* 式だけ。`novels.status_sort` は 凍結 を含まない設計なので freeze で status_sort を更新する必要はない。
- `narou db repair-freeze [--dry-run]` は `freeze.yaml.imported-*` の和集合を保存済み payload へ足す (解除はしない)。`narou db verify` は断片があればこのコマンドを案内する。回帰テストは `tests/freeze_sqlite_state.rs` (CLI / SQLite / YAML / 復旧 / アップグレード経路)。
