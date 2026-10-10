# narou_rs 変更履歴

各バージョンの主な変更点です。網羅的な差分は各節の compare リンクを参照してください。
コマンド互換性や未完了項目は [`COMMANDS.md`](COMMANDS.md)、導入と操作は [`README.md`](README.md) を参照してください。

## v0.4.9 の主な変更

- `narou update` / `narou download` で、小説ごとの処理状況がログファイルに残らない不具合を修正しました（[#36](https://github.com/Rumia-Channel/narou.rs/issues/36)）。ロガーが実行ファイルとライブラリで二重に存在していたことと、ダウンロードのレポート行がロガーを迂回していたことの 2 つが原因です。コンソールに出る行は `log/*.txt` にも記録されるため、`n 件のエラーが発生しました` の内訳をログから追えるようになります。
- 挿絵が EPUB に取り込まれない不具合を修正しました。サイト定義に `illust_grep_pattern` が無いサイト（なろう / R18なろう / カクヨム / Arcadia）でも既定の `<img>` パターンで挿絵を取得し、索引 (`.illustration_cache.yaml`) が知っているのに実体が無い挿絵は取り直します。挿絵ストアの一覧が空を返した場合も、参照された画像を 1 件ずつ確認してから EPUB を組み立てます。
- Web UI / Worker の修正: 変換ジョブの出力をダウンロードのコンソールから分け、Worker では既定で 2 ペイン表示にしました。
- 挿絵まわりのログを整理し、同じ案内が話数だけ繰り返し出る問題と、保存ファイル名に拡張子が二重に付く問題（`挿絵/x.jpg.jpg`）を修正しました。

変更一覧は [v0.4.8...v0.4.9](https://github.com/Rumia-Channel/narou.rs/compare/v0.4.8...v0.4.9) を参照してください。

## v0.4.8 の主な変更

- 新規ダウンロードで ID が 2 つ消費され、`ID:n のDL開始` の ID と実際のレコード ID が食い違う不具合を修正しました。確保した ID をそのままレコードに使うため、一覧の ID が飛ばなくなります（native / Cloudflare Workers 共通。既に登録済みの作品の ID は変わりません）。
- Web UI の進捗バーが、ジョブの終了後に残って積み重なる不具合を修正しました。同じコンソール・同じ種類のバーは 1 本にまとめ、ジョブの終端イベントとキューが空になった時点でスコープ単位に消去します。
- 配備の修正: SORAHOST と Cloudflare Workers が同じホスト名を共有する構成で、動いている側がホスト名を持てるようにしました。Workers が後から配備される場合は、SORAHOST のコネクタが作った DNS レコードに触れずに route へ切り替えます。
- 配備の修正: 前段の Cloudflare Access が答えた 403 を S3 プローブのスキップとして扱い、成功とは区別して報告するようにしました。

変更一覧は [v0.4.7...v0.4.8](https://github.com/Rumia-Channel/narou.rs/compare/v0.4.7...v0.4.8) を参照してください。

## v0.4.7 の主な変更

- SQLite 管理のライブラリで `freeze` を実行すると、それまでの凍結が外れて `.narou/freeze.yaml.imported-*` が増え続ける不具合を修正しました（[#35](https://github.com/Rumia-Channel/narou.rs/issues/35)）。凍結状態の保存先を YAML / SQLite の共通入口に統一し、取り込みを和集合にしたうえで `frozen_novels` へ投影します。既に失われた凍結は更新後に `narou db repair-freeze --dry-run` で確認し、`narou db repair-freeze` で復旧できます。
- Cloudflare Workers の Web コンソールで、ダウンロードの進捗が完了時にまとめて表示される不具合を修正しました。行ごとに送信し、順序・取りこぼし・送信間隔は共有バッファが管理します。
- Worker の EPUB に挿絵が入らない不具合を修正しました。変換後の本文に残る取得元 URL を、ダウンロード時に保存した `挿絵/<file>` へ寄せてから組み立てます。挿絵が見つからない場合は警告を出します。
- `webui.debug-mode` を ON にすると、挿絵の取り込みや EPUB の解決状況などの詳細を `[debug]` 行として Web コンソールへ流し、ブラウザの開発者コンソールにも転送します。
- 配備の修正: Worker 配備後の S3 検証が、プローブの User-Agent がボット判定に掛かって失敗していた問題を修正し、smoke でも同じ検証を行うようにしました。

変更一覧は [v0.4.6...v0.4.7](https://github.com/Rumia-Channel/narou.rs/compare/v0.4.6...v0.4.7) を参照してください。

## v0.4.6 の主な変更

- Cloudflare Workers の小説一覧・全体設定・タグ・キュー表示から、不要な S3・ログイン・ダウンローダ等の初期化を取り除きました。
- タグとキュー件数を D1 側で集計し、画面表示のために全小説・全待機ジョブを転送する処理を削減しました。認証と既存の検索・設定保存の規則は維持しています。
- 対象 API に `Server-Timing` と比較用スクリプトを追加しました。計測方法は `docs/worker-ui-latency.md` を参照してください。EPUB・画像変換の最適化は今回の対象外です。

- Web UI の「リンク」が localhost を開く不具合を修正しました（[#33](https://github.com/Rumia-Channel/narou.rs/issues/33)）。ID・Nコード・別名をページ URL として保存せず、既存の不正な値は取得 URL にフォールバックします。再ダウンロードは不要です。
- 個別メニューの「変換」が要求を送らない不具合を修正しました（[#33](https://github.com/Rumia-Channel/narou.rs/issues/33)）。一覧の選択状態に関係なく、その作品を変換します。
- SQLite + Lite の変換で設定ストアの接続ロックを再取得して停止する問題と、EPUB 用の挿絵取り出しが CLI の Tokio 文脈で panic する問題も修正しました。

変更一覧は [v0.4.5...v0.4.6](https://github.com/Rumia-Channel/narou.rs/compare/v0.4.5...v0.4.6) を参照してください。

## v0.4.5 の主な変更

- 半角カナを含むルビが別の文字に化ける不具合を修正しました（[#31](https://github.com/Rumia-Channel/narou.rs/issues/31)）。既に変換済みの作品は、更新後に再変換してください。
- 話タイトル・章タイトルの変更後に更新が失敗する不具合を修正しました（[#32](https://github.com/Rumia-Channel/narou.rs/issues/32)）。旧タイトルで保存した本文を履歴へ退避してから新しい本文を保存し、その後の更新も継続できます。
- 作者ページの追跡、複数ログインのサイト単位管理、EPUB エンジンの選択を追加しました。詳しい操作は `COMMANDS.md` を参照してください。
- native の S3 互換挿絵ストレージ、SQLite + S3 構成での挿絵重複排除、省容量設定を追加しました。
- Cloudflare Workers 向けの取得・変換・Web UI と、SORAHOST 向けの配備経路を追加しました。

変更一覧は [v0.4.4...v0.4.5](https://github.com/Rumia-Channel/narou.rs/compare/v0.4.4...v0.4.5) を参照してください。
