# narou.rs 移植状況

## 現況（v0.4.11）

正本はAGENTS.md / COMMANDS.md。narou.rb24コマンド + Rust拡張db/illust/login/authorの計28。既存完了印の集計は24完了/4部分(download/update/convert/web)/未実装0。今回のQAだけでRuby互換の完了印を昇格しない。Cargo/Worker/probe lockとライセンス表示は0.4.11へ更新済み。main統合/タグ公開は未実施。

共有化: application use-case、settings/setting_core、download_targets、retry_policy、push_events、MessageContext/progress、HTTP RedirectChain/send_paced、ObjectStore::update_small、EPUB EpubPreparation/EpubBuild。同時INI更新はFS lock/SQLite Immediate/D1 primary full raw snapshot CAS、正準保存後のミラー失敗もACKに反映。CRC衝突を同値扱いしない。Workerの出力/debug/表示先をfuture poll単位に束縛し、operation ID/seqで並列進捗を分離。native Web更新もサイト別並列取得。

待機は物理送信・redirect・transport切替ごと。Worker interval/wait-stepsはsite floorと合成し、待機中は設定snapshotの残TTLまでに再確認。60.0がJSON60になる初期値誤競合も修正。AozoraEpub3_Lite v0.1.6 pin381c0b17f91946e22d41e5246d3977c706abe82e。AssetPlanでI/O非依存の逐次画像収集、native/Workerは同じfinish/layout/処理を利用。上流所有repo developへのno-ff統合/push済み。

検証: v0.4.11 Rust full consumer suite(1124 lib + 111 bin + helpers/integration)、実Workerd HTTP/queue/replay104、Python52、JS progress/refresh/link20成功。CI既定Binaryen130で実Workerを再packageし、V8のmodule受付とWorkerd実取得を確認。workflow source文字列とfake curlのwiringテストは廃止。production SORAHOST shellを実HTTP originで検証(200/204/400/401/403各1回、503は6retry+diagnostic、exit0)。pwsh7.6.6で実Windows ZIPを作成し、本体/updater.exe.new/backup/loginと資産を確認(ローカル署名省略のみ、CI署名必須は維持)。

Web実ブラウザ: native並列2本の独立進捗、Worker TOC+8話の進捗、UI保存→実permit、wait-steps2の約370ms/5s交互、60→0.2の取得中反映を確認。到着間隔とpermit間隔は混同しない。履歴race、radio/console keyboard、mobile toolbar/dropdown、Worker行内folder capabilityを修正。個別dirty save/clear、notepad autosave/WS同期、取得中の履歴削除でバーを保持、再接続後終端で残留バー0を確認。Native EPUB raw novel_outputsのBrotli UTF8誤認500を実証しload_outputへ合流。Brotli/none/legacy deflateはvalid EPUB、unknown/corruptは500で拒否。Worker streaming EPUB実ZIPも確認。Workerの挿絵警告は参照あり・有効・実体なしだけに限定し、参照なし/欠落/無効の3ケースを実取得し入力を復元。元のDocuments/WebNovelは読取り専用、全ライブラリはrepo targetのRust TempDir controller所有。実Cloudflare配備/Cloud queue並列の検証をこのローカル結果へ含めない。

リモート旧ブランチ7本は未統合作業をno-ffでdevelopへ取り込み後削除済み(a4524f3)。追加修正も機能単位でdevelopへno-ff統合し、GitHubはmain/developだけを維持する運用。既定YAML、SQLite opt-in、global_settingは常に~/.narousettingファイル。native S3はlocal s3.*優先で未設定項目だけS3_*、挿絵だけS3でfail-closed。SORAHOSTはPteWorker公開/PORT loopback/server-ws-port=0/concurrency=true。loginはbrowser helper/export/import、複数名前/順序/UUID session/暗号化。

0.4.11 最終監査で確認したデータ保全の修正: SQLite のインプレース YAML 書き出しは meta の export_file_sha 指紋を原子的なファイル保存より前に記録し、失敗後の未編集の書き出し残骸を再取り込みしない。管理状態だけでなく database.yaml の作品レコードも対象。ユーザーの編集と narou-compat のファイル優先は維持する。S3 COPY は完全な CopyObjectResult と ETag/LastModified を確認できた場合だけ成功し、200 内 Error・破損応答では元オブジェクトを削除しない。実 HTTP の失敗／成功ケースと、書き出し失敗後の正常保存→再オープンを smoke で確認済み。
指紋は既存の未編集 projection と次回公開予定を最大2件保持し、2回目が公開前に中断しても保護を失わない。指紋更新と公開は同一ファイルロック内。--out が実 .narou を指す場合も対象。最終 workspace 全 target は1342成功/0失敗/1ignored、Clippy 全 feature -D warnings・Rustdoc -D warnings・core/Worker wasm check 成功。Web CSRF 回帰 fixture は並行CWD変更に影響されない絶対パスへ修正し、HTTPの拒否/許可とキュー状態の検証内容は維持した。

## 未リリース: Worker 本文・履歴の S3 保存

- `worker_entry/src/content_store.rs` と `0011_content_objects.sql`。話単位の既存 YAML を共通 codec で圧縮し、S3 の不変な版へ PUT 後に D1 参照を公開する。現行本文と cache / .cache 両形式の履歴が対象。本文ストアの目次・参照読み取りは primary に揃える。CRC32・SHA-256・圧縮前後サイズを検証し、S3 の欠落・破損を旧 D1 への fallback で隠さない。native と挿絵の保存方式は変更しない。
- `content_backend=d1→migrating→s3` は `asset_backend` と独立。管理 API `/api/admin/content-migration` の plan/copy/verify/cutover/purge/gc/status と `scripts/migrate-worker-content.mjs`（既定 plan、`--apply` で削除まで）を用意。phase の進捗は D1 に保存し、期限付き lease と SQL ガードで旧書込み・移行競合を拒否する。削除待ちは durable outbox、未完了 PUT は 1 時間保護。
- Worker の更新は `PersistenceService::cache_section` で履歴をコピーし、新本文 PUT 失敗時も現行参照を残す。改題の旧キーは新本文と新 TOC の保存後に削除する。native は従来の履歴移動を維持。旧 API `move_section_to_cache` は全呼出元を移行して除去した。
- ローカル Worker + D1 + S3 HTTP fixture で移行・実圧縮バイト・破損／欠落拒否・再起動・実体削除を確認。実取得・強制更新・正確な旧本文保存・差分一覧／消去・EPUB、履歴 COPY 成功後の新本文 PUT 503 でも現行本文と EPUB を維持すること、改題後の旧キー削除も成功。SQL regression 12 件を含む Python 64 件、release Wasm ビルド、Worker wasm Clippy `-D warnings`、切替後の既存 HTTP 契約 104 件が成功。期限切れ PUT の公開・旧 .cache 履歴の取り残し・新本文失敗時の現行参照消失は failing-before/passing-after を確認。本番の Wasabi 移行・本番データの削除は未実施。旧「本文を D1 の列へ展開する」方針は撤回し、AGENTS.md / COMMANDS.md / 設計資料を更新。
- 標準 release 設定の Worker ビルド（wasm-opt 込み）と、native の既存 cache 回帰 1 件も成功。ローカルではコンパイラ並列数を 1 に抑え、native の C archive は LLVM、テストの debug 情報は無効にして検証した（リポジトリ設定の変更なし）。
- サブディレクトリありで作品フォルダ名が `本文` の場合の目次誤分類を修正。`ContentStore::is_content_key` と `0012_content_metadata_boundary.sql` が `toc.yaml` / `converter.yaml` / `.illustration_cache.yaml` を D1 に残す。SQL の誤分類と、S3 切替後の実 EPUB が TOC 不在で失敗することを failing-before で再現し、修正後は同じ構成で purge・EPUB・再起動・既存 HTTP 契約まで成功。

## 未リリース: 作者の継続追跡・管理

- `application::authors` の `AuthorStore` / `AuthorService` と portable `author` codec を共通境界にした。Native は Inventory の原子更新、Workers は primary D1 `app_state('inv','author')` の全 JSON スナップショット CAS。作者 URL → サイト名だけを保存し、小説の作者表示名とは区別する。
- Workers の更新 API・Update 投入・定期更新の最終ページから `author_check` を投入し、既存の実効 YAML / Cookie / 待機を使って新作だけを通常 Download ジョブへ流す。登録済み作品は `contains_toc_urls` の bounded primary 索引検索で除外する。小説 0 件でも動作し、自動停止設定は手動チェックを止めない。解除済みジョブと取得中の解除を再確認するが、投入済み通常 Download はキャンセルしない。
- Native / Workers 共通の「環境設定 → 作者」で登録・一覧・解除・手動確認。`GET/POST/DELETE /api/authors` と `POST /api/authors/check`。Native CLI の 0 番削除を拒否し、`author --server <URL>` でライブラリ外から Workers を操作できる。認証は環境変数の admin / Access token。通常の作者 URL ダウンロードは単発のまま。
- 実 Worker + 隔離 D1 + 制御した HTTP origin で再起動後の登録、新作の継続取得、既知作品の除外、停止設定、dry-run、手動チェック、実 scheduled handler、取得中の解除、古いジョブのスキップ、全 remote CLI 操作を確認。Firefox で Native / Workers の作者タブから登録・解除し、永続 API 状態を確認した。Worker の取得済み 6 作品は解除後も残る。
- Native は実 CLI の YAML 登録 → SQLite 移行、CLI / Web 双方向の一覧・解除、空ライブラリの CLI / Web 更新からの作者確認を実行し、WebSocket の実出力も確認。ユーザーの今回の許可により repo 内 TempDir を使用し、検証ライブラリ・プロセス・一時ソースを削除した。本番 D1 / S3 のデータ操作はしていない。
- 検証: Native 既定全 target 1331 成功、Native + Lite は単一テストスレッドで 1347 成功（どちらも参照 fixture 1 件 ignored）。Lite の並列全件実行はタイムアウトし、原因は未特定。Native + Lite / Worker の Clippy `-D warnings` と標準 release Wasm ビルド、Python 69 件、S3 保存・移行・EPUB の実 smoke と切替後 HTTP 契約 104 件が成功。既存の SQL ファイル複製を byte 比較するテストは削除し、移行後データ保全の実動作テストは維持した。
- develop 統合後も実 Worker の作者シナリオと Python 69 件を再実行し成功。Native は現在のソースからテスト実行ファイルをビルドし、作者関連 11 件・既存 DB の移行保全 4 件が成功した。統合後の Eval 経由全件待ちはタイムアウトし完走として数えていない。別の Cargo 実行も C 依存再ビルド中に中断されたため、最後はビルドとテスト実行を分離して確認した。

## 過去の記録（以下は現況ではない）

最新の正本はリポジトリの `AGENTS.md` と `COMMANDS.md`。古い日付の監査表ではなく、各コマンドの当該節と実装を確認する。

- narou.rb 由来 24 コマンドと Rust 拡張 `db` / `illust` / `login` の計 27 コマンド。✅ 23、🟡 4（`download` / `update` / `convert` / `web`）、❌ 0。`db` はトップレベル help 一覧の 26 件には載っていない。完了印は Ruby 版との全側面の突き合わせが必要であり、実装があるだけで昇格させない。
- Cargo.toml の現行バージョンは 0.4.9（リリース済み。最新の変更点は最終行を参照）。0.4.5 は issue #31/#32 を含む修正版で、新機能ツアーの追加は無し (0.4.2 と同じ扱い)。
- issue #31: converter の半角カナ変換を NKF 相当の対応表・濁点/半濁点合成へ修正（0.4.5 でリリース済み）。HTML ルビからの変換回帰テストと実 CLI で `ＲＰＧ《ロールプレイングゲーム》` を確認。EPUB 生成前の問題であり Lite 側の変更は不要。
- issue #32: 各話の題名変更時は旧目次のファイル名で本文を履歴へ退避し、保存先を Ruby版・`narou diff` と同じ `本文/cache/` に統一（0.4.5 でリリース済み）。SQLite（ミラーあり/なし）とファイル保存で、題名変更・差分なし再更新・章題変更・旧本文の履歴保存を確認。サイト固有処理・Lite 側の変更は無し。
- issue #33: `original_url` は HTTP(S) ページ URL だけを記録・表示し、旧データの ID/Nコード/別名は `toc_url` へフォールバック。個別「変換」ハンドラを登録。SQLite + Lite の設定再読み出しによる接続デッドロックを本文のみ／端末出力の両経路で除去し、EPUB の挿絵取り出しを Tokio 外へ分離（未リリース）。Firefox で選択 0 件から ID 0 の `queue_complete`、36 話の変換・EPUB 出力を確認。既存 SQLite 回帰テストは両経路と current-thread runtime からの挿絵実体化・ガード削除をカバーする。
- 既定ストレージは YAML。SQLite は Web UI のツアーまたは `.narou/storage-backend` による選択制。ローカル設定などは SQLite 時 `app_state`、`~/.narousetting/global_setting.yaml` は SQLite 選択時もファイルのまま。`narou-compat=true` はローカル YAML の前方互換モード。
- 挿絵のバイナリだけを S3 互換ストレージ (Wasabi など) へ置ける。実装は core の `platform::s3_store::S3Store` を native と Worker が共有し、native の設定は `s3.*` (環境変数 `S3_*` が優先)、切替は `s3.asset-backend` (`local` | `s3`、環境変数 `NAROU_RS_ASSET_BACKEND`)。接続情報が欠けたまま `s3` を選ぶと fail-closed で失敗する。移行は `narou illust s3-push` (既定 dry-run、`-f` で実行) と `s3-verify`。EPUB 生成時だけ S3 からローカルへ取り出し、スコープを抜けると削除する。話ごとの変換キャッシュ (`convert.section-cache`, local 設定, 既定 true) は容量の厳しい環境で切り、無効時は既存ファイルも削除する (環境変数 `NAROU_RS_SECTION_CACHE=0` でも可。SORAHOST の `start.sh` は無効にする)。既定時は brotli 圧縮 + 1 行ヘッダで保存する。生データ (raw HTML) は `DownloaderSettings::save_raw_html()` が唯一の判定で、Worker は常に保存せず、native は `economy: nosave_raw` で切る (挿絵のローカライズは取得時のメモリ上の HTML を使うので影響なし)。変換済みテキストは `convert.keep-txt` (既定 true、`NAROU_RS_KEEP_TXT=0`) で切れ、native は txt を書かず/消し、Worker は保存せず EPUB 取得時に `ConvertService::convert_only` で組み立て直す。SORAHOST の `start.sh` は economy=nosave_diff,nosave_raw / convert.no-epub / convert.keep-txt=false / sqlite.mirror-files=false を初回に入れる。`sqlite.mirror-files=false` (環境変数 `NAROU_RS_MIRROR_FILES=0`) では SQLite モードでも `小説データ/` へ実ファイルを書かず、保存先は `objects`/`object_chunks` だけになる。変換が要求するファイル (toc.yaml / 本文/*.yaml / setting.ini / replace.txt) は `NativeStore::materialize_novel_files` が変換中だけ実体化し、スコープを抜けると自分が書いた分だけ消す (`MaterializedNovelFiles`)。実ファイル前提の `narou illust` orphan/rebuild/fix-ext と `narou clean` は実質何もしない。backup と `db export-yaml --in-place` は従来どおり。SORAHOST (Pterodactyl) への配備は `sorahost/` の `start.sh` + README (cloudflared トンネルで公開するので受信ポートは不要)。
- `login` は複数資格情報・順序変更・セッション ID・暗号化保存・Web UI 管理に対応し、`narou login -h` も `add` / `order` / `clear --index` を掲載済み。`clear --index` は host 必須（省略時はエラー）。配布 zip には署名済みの `narou_rs_login` を同梱し、`scripts/package-release.ps1` が Windows 実行ファイルの Authenticode 署名を検証する。
- `COMMANDS.md`、`AGENTS.md` の変更と同時に、このメモリの要約も同期する。`sample/narou` はローカルの Ruby 参照ソースで、必ず存在するとは限らない。
- **現行バージョンは 0.4.8（2026-10-08 リリース済み、詳細は `porting_status/release_0_4_8_2026-10-08.md`）**。0.4.8 の修正: (1) 新規ダウンロードで ID を二重採番していた不具合（DL開始行の ID と実レコード ID の食い違い、一覧 ID の飛び。native / Worker 共通の core `src/downloader/mod.rs`）、(2) Web UI の進捗バーがジョブ終了後に残って積み重なる不具合（`src/web/assets/js/main.js`。同一コンソール・同一 topic のバーを 1 本化し、終端イベントとキューが空になった時点でスコープ単位に消去）、(3) SORAHOST と Workers の同一ホスト名の持ち合い、(4) 前段 Access の 403 を S3 プローブのスキップとして扱う。
- **現行バージョンは 0.4.9（2026-10-10 リリース済み、詳細は `porting_status/release_0_4_9_2026-10-10.md`）**。0.4.9 の修正: (1) CLI のログに小説ごとの行が出ない不具合（issue #36）。`src/logger.rs` が bin と lib の両方にコンパイルされて `LoggerState` が 2 つ存在し、`logger::init()` が bin 側しか初期化していなかったことと、Downloader のレポート行が logger を迂回する sink へ出ていたことの 2 つが原因。bin は lib のロガーを共有し、Downloader も `console_sink()` に統一した。回帰は `tests/cli_logging.rs`。(2) 挿絵が EPUB に入らない / 取り直されない不具合（`illust_grep_pattern` が無いサイトでも既定 `<img>` パターンで取得、索引にあるのに実体が無い挿絵は取り直し、EPUB 側は参照画像を 1 件ずつ stat）。(3) Worker の convert 出力をダウンロードのコンソールから分離（既定オン）。(4) `-D warnings` の clippy 5 件を修正。