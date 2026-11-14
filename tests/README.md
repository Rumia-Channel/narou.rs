# テスト仕様と回帰テスト手順

本ディレクトリおよび各コンポーネントにおける回帰テストの構成と実行手順をまとめます。

## 1. 変換処理の回帰テスト (Conversion regression tests)

リポジトリに含まれる合成データによる変換回帰テストは以下で実行できます。ダウンロード済みの小説データや外部ネットワークアクセスは不要です。

```sh
cargo test -p narou_rs --test convert_parity
```

### カクヨム実データとの完全一致テスト (ignored)

より大規模な `kakuyomu_sample_matches_narou_rb_reference_byte_for_byte` テストは、参照用の小説データが非公開のローカルファイルであるため、既定で `--ignored` に設定されています（※ テストの無視は互換性検証の成功を意味するものではありません）。

実行する場合は、`sample/novel/小説データ/カクヨム/<作品フォルダ>/` 配下に `toc.yaml`、参照される `本文/*.yaml` ファイル、および narou.rb の標準 EPUB デバイス設定で出力された `kakuyomu_jp_*.txt` を配置し、以下を実行します。

```sh
cargo test -p narou_rs --test convert_parity \
  kakuyomu_sample_matches_narou_rb_reference_byte_for_byte -- --ignored --exact
```

ディレクトリや参照用出力が存在しない場合、明示的実行は失敗します。著作権保護の観点から、ダウンロードした小説データや参照出力をリポジトリにコミットしないでください。

### HTML エンティティの変換規則

合成データの HTML エンティティ変換仕様は narou.rb の [`Helper::ENTITIES`](https://github.com/whiteleaf7/narou/blob/27fb741fd561e2f26aba9fdf9d9b7b4384792be4/lib/helper.rb#L137) に準拠しています。`&nbsp;` は変換パイプライン全体を通じて通常の半角スペースへと変換されます。

## 2. Web UI リンクエスケープの回帰テスト

Web UI のリンク属性エスケープ回帰テストは以下で実行します。最小限の DOM テキストシリアライズスタブを用いてレンダリングヘルパーの挙動を検証します。

```sh
node --test tests/render_links.test.mjs
```

### 複数選択の凍結・解除

```sh
node --test tests/freeze_ui.test.mjs
cargo test -p narou_rs --lib application::novel_actions::tests
cargo test -p narou_rs --test tag_order_http --test freeze_sqlite_state
```

選択範囲内・範囲外の右クリック、全件への明示的な凍結・解除、旧 `frozen` タグだけの凍結、タグの重複防止、選択外の状態保持を確認する。Native の HTTP 検査は YAML / SQLite の両方を実行する。Worker は `node worker_entry/tests/run.mjs --release` 内の `freeze.mjs` が実 workerd / D1 と一覧・タグ API を検証する。

## 3. Worker S3 設定・デプロイの回帰テスト

モック値およびローカルストアを用いた検証です。実際のマイグレーション、Cloudflare への本番デプロイ、本番 S3 へのアクセスは行いません。

```sh
cargo test -p narou_rs --test worker_asset_backend --test worker_storage_probe
python3 -m unittest discover -s worker_entry/tests -p 'test_*s3*.py'
cargo check -p narou_worker --target wasm32-unknown-unknown
```

バックエンドポリシー、デプロイ前の必須検証、サニタイズされた制限付き LIST プローブの挙動を検証します。

配信用 Worker の変更では、source の保持と2つの実行環境の分離も確認する:

```sh
cargo test -p narou_rs --test reader_storage
python3 -m unittest discover -s worker_entry/tests -p 'test_ci_*.py'
```

`worker_entry/` で `worker-build --release` を実行した後、リポジトリルートで
`node worker_entry/tests/read_worker.mjs` を実行する。共有のローカル D1 と HTTP S3
fixture に対して管理・配信の2 Worker を動かし、管理側の重い処理が占有中でも
API / EPUB が配信できること、認証・CORS・編集要求の転送、source の書き込み禁止と
EPUB の一時データの後始末を確認する。実際の本番 OOM や Cloudflare 配備の検証とは区別する。

同じ検証は205話の挿絵なし作品と、同じ欠落挿絵を繰り返す作品を別々に生成し、
各本文の S3 取得が1回であることと、欠落挿絵の重複照会の抑制を確認する。
S3 要求件数・ヘッダ到着時間・完了時間はログへ出し、壁時計の閾値は固定しない。
本文参照の差し替え・削除・初読時の破損を実行中に注入して不正な EPUB の公開を拒否し、
欠落参照の記録上限を超えた後の実在する画像も EPUB に残ることを確認する。

## 4. 共通ランタイム境界の回帰テスト

```sh
cargo test --features lite --test novel_settings_atomic --test worker_object_updates --test http_pacing
cargo test --features lite --lib application::settings::tests
cargo test --features lite --lib native::sqlite::content::tests
```

独立した設定書き込みプロセス、正規保存およびミラー書き込み失敗の検知、完全スナップショットによる D1 の Compare-and-Swap（同一 CRC の衝突を含む）、リダイレクト時のペーシング制御、整数/浮動小数点 JSON の基準値、デコード済み変換出力の保存などを網羅します。

D1 SQL の回帰テストは SQLite 上で本番 SQL 文を実行して検証します。実際の Workerd を用いた契約テストは、`BASE_URL`、`NAROU_ADMIN_TOKEN`、`CONTRACT_QUEUE=1` を指定して稼働中の `wrangler dev` インスタンスに対して `worker_entry/tests/contract.mjs` を実行してください。

## 5. データ保全・ロールバックの回帰テスト

```sh
cargo test --features lite --test yaml_rollback_roundtrip
cargo test --features lite --lib platform::s3_store::tests
```

コンテンツ・メタデータ・マーカーの公開失敗後の再試行と正常オープン、コミット途中での中断、本番管理ディレクトリ宛てのアーカイブエクスポート、外部からのレガシーファイル編集の保護、S3 コピー応答時の完全成功確認前のソース削除防止などを検証します。

## 6. 進捗表示および Pixiv シリーズタグの回帰テスト

```sh
node --test tests/progress_bars.test.mjs
cargo test -p narou_rs --lib progress_tests
cargo test -p narou_rs --lib downloader::toc::tests
cargo test -p narou_rs --test pixiv_series_tags
```

個別試行とピア処理、キャンセル/再接続時の廃棄マーカー（tombstone）、総話数不明の目次や循環ページネーション、31 話シリーズにわたるタグ集計を網羅します。更新テストでは、後半ページのみでタグが変更された場合に手動タグを保持し、変更のない本文セクションを再ダウンロードしないことを確認します。

## 7. ダウンロード ID の予約・割り当て

```sh
cargo test --all-features --test download_new_novel_id
cargo test --all-features --lib id_allocator::tests
cargo test --all-features --lib unused_ids_are_reused_without_freeing_live_or_committed_owners
cargo test --all-features --lib concurrent_allocations_only_reuse_released_unpublished_ids
```

404 や年齢確認キャンセル後の再ダウンロード成功、未公開 ID の再利用、古いリリースの境界分離、インポート済み/コミット済み ID の境界管理、メモリ/SQLite 間での並行予約を検証します。コミットされた ID は削除後も再利用されません。実際のサイトへはアクセスせず、隔離されたモックストアと合成 HTTP を使用します。

## 8. 継続的な作者追跡テスト (Persistent author tracking)

```sh
python -m unittest discover -s worker_entry/tests -p 'test_author_tracking_sql.py'
cd worker_entry
node tests/run.mjs
```

Worker ランナーは実際の Wasm とアセットをビルドして HTTP 契約テストを実行した後、隔離されたローカル D1 とモック HTTP ソースに対して `tests/author_tracking.mjs` を実行します。

サービス再起動を跨ぐ登録の永続化、空ライブラリの更新時やスケジュールイベントでの新着検知、登録済み作品の除外、自動無効化と手動チェックの動作、チェック実行中の削除時に作品データが誤削除されないことなどを網羅します（単発の作者ダウンロードは追跡対象外のままです）。

Worker をビルドした後であれば、`node tests/author_tracking.mjs` 単体で作者追跡のスモークテストを実行することも可能です。Worker 側の作者管理は Web UI の HTTP API 経由で実行され、ネイティブのリモート CLI は不要です。ローカル状態は `finally` で自動削除され、本番の D1 や S3 は一切使用しません。

※ Unix 環境では、スモークテストの各 Wrangler は独立したプロセスグループで実行され、終了時に npx/Wrangler/workerd グループ全体が停止します。Windows 環境では `taskkill /T /F` を使用します。正常に終了したテストはプロセスを確実に停止し、ローカルのフィクスチャ状態を解放します。

## 9. タグ・お気に入り・ライブ設定の回帰テスト

```sh
node --test tests/*.test.mjs
cargo test --features lite --lib application::favorite_tags
cargo test --features lite --lib application::webui::config
cargo test --features lite --test tag_order_http --test cli_server_security_reload
node worker_entry/tests/run.mjs --release
```

Native の HTTP テストは `TempDir` 内の YAML / SQLite ライブラリを実バイナリで開き、旧配列形式・数値文字列のタグ・新旧 ID の混在・全並び順の往復・表示順に依存しない色割当・破損在庫の保全を確認する。CLI の設定変更は実 WebSocket の握手で wildcard / IP の許可・取り消しを確認する。

フロントエンドは実モジュールの要求順序・応答の逆転・保存中の別画面変更・再接続の再取得・アクセシビリティ上の操作状態を検証する。ブラウザでの操作確認はこれらとは別に行う。

Worker ランナーは `.wrangler/contract-*` に設定・資格情報・D1 を隔離し、開発用 `.dev.vars` と `.wrangler/state` を変更しない。`tag_sequence.mjs` は同じローカル D1 に一時トリガーを置き、既存行の CAS 競合と初回 INSERT の競合を確実に発生させ、競合相手の追加が残ることを確認する。旧形式・破損状態・同時追加・並び順の往復も実 HTTP 経由で検証する。SQL を使う `virtual_attributes.mjs` も同じ隔離先を受け取り、単体で開発用 DB を推測しない。`--keep` 指定時を除き、失敗時も所有する子プロセスと状態を削除する。
