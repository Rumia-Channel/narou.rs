# S3 挿絵 dedup (2026-10)

## 概要
SQLite+S3 モードで挿絵を作品横断のグローバルプール `illustrations/<base64url(sha256)>.<ext>` に寄せる重複除去を実装した。native 専用 (Worker は `illustration_dedup: false` 固定)。

## 条件
`native::sqlite::state::illustration_dedup_enabled()` = storage-backend=sqlite + `s3.asset-backend=s3`。

## 主要な変更
- `S3Location::with_illustration_dedup` (src/platform/s3_request.rs): `storage_key` が `…/挿絵/<sha256-hex>.<ext>` を `illustrations/<b64url>.<ext>` へ写像。
- `legacy_storage_key` / `has_pool_alternate`: 移行前の `挿絵/` 実オブジェクトを指すための旧配置キー。
- `S3Store::read`/`stat`/`read_stream`: pool に無ければ legacy にフォールバック (src/platform/s3_store.rs)。
- `S3Store::delete`: legacy のみ。`pool_stat`/`pool_exists`: dedup プール確認用 (fallback 無し)。
- `SplitStore::is_illustration_prefix`: `illustrations/` 始まりも挿絵側へ回す。
- `IllustrationStorageService::with_dedup`: dedup ON で新規名を `<sha256>.<ext>` に揃える (mitemin iNNN も hex 化)。
- `dedup_active()` (src/illustration_store.rs): native のみ `illustration_dedup_enabled()`、worker は false。

## narou illust s3-dedup
旧 `挿絵/` 配置の S3 オブジェクトを pool へ copy、確認後 legacy delete。既定 dry-run、`-f` で実行。hex 名でない挿絵は対象外。s3-push とは別物 (s3-push は新規書込みが既に pool に出るので、s3-dedup は dedup ON 以前の分の後処理)。

## 対象外・境界
- mitemin 名・hash 以外の挿絵名は pool に出ず小説ごとの `挿絵/` 配置を保つ。
- pool は delete されない (小説削除時は legacy 配置だけ消す)。
- SQLite+S3 が揃わない場合は従来どおり小説ごとの挿絵名。