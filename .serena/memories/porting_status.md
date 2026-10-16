# narou.rs 移植状況

## 現況（開発版 v0.4.13）

正本はAGENTS.md / COMMANDS.md。narou.rb24コマンド + Rust拡張db/illust/login/authorの計28。既存完了印の集計は24完了/4部分(download/update/convert/web)/未実装0。今回のQAだけでRuby互換の完了印を昇格しない。Cargo/Worker/probe lockとライセンス表示は0.4.13へ統一済み。公開済み GitHub Release は v0.4.12 のまま。今回の版数更新では main 統合・タグ・リリース作成を行わず、リモートの常設ブランチは main / develop だけを維持する。

0.4.13 検証: `cargo check --workspace --all-targets --all-features`、Native + Lite のビルドと実 CLI `version --more` の 0.4.13 表示が成功。標準 release Wasm を作り直し、実 Worker の `/api/version/current.json` が 0.4.13、HTTP 契約 103 件・作者追跡 6 シナリオ・全 S3 保存 smoke・Python 74 件が成功した。版数の非空だけを検査する旧 assertion は削除し、実値は使い捨ての API プローブで確認した。cargo-about 0.9.2 で両 notice を生成し、直接依存 56 件の表も一致。root 419 / non-GPL probe 394 の lock graph は自プロジェクトの版数以外に変更が無い。

共有化: application use-case、settings/setting_core、download_targets、retry_policy、push_events、MessageContext/progress、HTTP RedirectChain/send_paced、ObjectStore::update_small、EPUB EpubPreparation/EpubBuild。同時INI更新はFS lock/SQLite Immediate/D1 primary full raw snapshot CAS、正準保存後のミラー失敗もACKに反映。CRC衝突を同値扱いしない。Workerの出力/debug/表示先をfuture poll単位に束縛し、operation ID/seqで並列進捗を分離。native Web更新もサイト別並列取得。

待機は物理送信・redirect・transport切替ごと。Worker interval/wait-stepsはsite floorと合成し、待機中は設定snapshotの残TTLまでに再確認。60.0がJSON60になる初期値誤競合も修正。AozoraEpub3_Lite v0.1.6 pin381c0b17f91946e22d41e5246d3977c706abe82e。AssetPlanでI/O非依存の逐次画像収集、native/Workerは同じfinish/layout/処理を利用。上流所有repo developへのno-ff統合/push済み。

検証: v0.4.11 Rust full consumer suite(1124 lib + 111 bin + helpers/integration)、実Workerd HTTP/queue/replay104、Python52、JS progress/refresh/link20成功。CI既定Binaryen130で実Workerを再packageし、V8のmodule受付とWorkerd実取得を確認。workflow source文字列とfake curlのwiringテストは廃止。production SORAHOST shellを実HTTP originで検証(200/204/400/401/403各1回、503は6retry+diagnostic、exit0)。pwsh7.6.6で実Windows ZIPを作成し、本体/updater.exe.new/backup/loginと資産を確認(ローカル署名省略のみ、CI署名必須は維持)。

Web実ブラウザ: native並列2本の独立進捗、Worker TOC+8話の進捗、UI保存→実permit、wait-steps2の約370ms/5s交互、60→0.2の取得中反映を確認。到着間隔とpermit間隔は混同しない。履歴race、radio/console keyboard、mobile toolbar/dropdown、Worker行内folder capabilityを修正。個別dirty save/clear、notepad autosave/WS同期、取得中の履歴削除でバーを保持、再接続後終端で残留バー0を確認。Native EPUB raw novel_outputsのBrotli UTF8誤認500を実証しload_outputへ合流。Brotli/none/legacy deflateはvalid EPUB、unknown/corruptは500で拒否。Worker streaming EPUB実ZIPも確認。Workerの挿絵警告は参照あり・有効・実体なしだけに限定し、参照なし/欠落/無効の3ケースを実取得し入力を復元。元のDocuments/WebNovelは読取り専用、全ライブラリはrepo targetのRust TempDir controller所有。実Cloudflare配備/Cloud queue並列の検証をこのローカル結果へ含めない。

リモート旧ブランチ7本は未統合作業をno-ffでdevelopへ取り込み後削除済み(a4524f3)。追加修正も機能単位でdevelopへno-ff統合し、GitHubはmain/developだけを維持する運用。既定YAML、SQLite opt-in、global_settingは常に~/.narousettingファイル。native S3はlocal s3.*優先で未設定項目だけS3_*、挿絵だけS3でfail-closed。SORAHOSTはPteWorker公開/PORT loopback/server-ws-port=0/concurrency=true。loginはbrowser helper/export/import、複数名前/順序/UUID session/暗号化。

0.4.11 最終監査で確認したデータ保全の修正: SQLite のインプレース YAML 書き出しは meta の export_file_sha 指紋を原子的なファイル保存より前に記録し、失敗後の未編集の書き出し残骸を再取り込みしない。管理状態だけでなく database.yaml の作品レコードも対象。ユーザーの編集と narou-compat のファイル優先は維持する。S3 COPY は完全な CopyObjectResult と ETag/LastModified を確認できた場合だけ成功し、200 内 Error・破損応答では元オブジェクトを削除しない。実 HTTP の失敗／成功ケースと、書き出し失敗後の正常保存→再オープンを smoke で確認済み。
指紋は既存の未編集 projection と次回公開予定を最大2件保持し、2回目が公開前に中断しても保護を失わない。指紋更新と公開は同一ファイルロック内。--out が実 .narou を指す場合も対象。最終 workspace 全 target は1342成功/0失敗/1ignored、Clippy 全 feature -D warnings・Rustdoc -D warnings・core/Worker wasm check 成功。Web CSRF 回帰 fixture は並行CWD変更に影響されない絶対パスへ修正し、HTTPの拒否/許可とキュー状態の検証内容は維持した。

## 未リリース: Workers の本文を S3 基本にする

- `composition::build_object_stores` は S3 構成成功後に `content_store::initialize_s3_default` を通す。未設定なら旧本文・履歴の存在に応じて `migrating` / `s3` を primary の単一 SQL で保存し、新しい本文は S3 に置く。明示的な `d1` は非必須環境だけに残し、`NAROU_REQUIRE_S3=true` では同じ規則で進める。`migrating` / `s3` は戻さず、失敗を D1 保存で隠さない。Native は変更なし。
- 自動初期化は既存本文の一括コピー・削除を行わない。管理 API の `plan` / `status` は未設定状態を `d1` と報告する読み取り専用で維持する。旧本文の照合・切替・削除は既存の再開可能な移行処理を使う。
- 標準 release Wasm（wasm-opt 込み）、Python 74 件（本文 SQL 17 件を含む）、実 Worker の既存本文移行・更新失敗・履歴・EPUB・purge・再起動と HTTP 契約 104 件、作者追跡 6 シナリオが成功。追加で未設定の新規ライブラリ、旧本文あり、S3 必須の明示 d1、S3 設定不正、初回 PUT 失敗と復旧の 5 ケースを隔離 D1 + S3 HTTP fixture で実行した。
- develop 統合後、`tests/run.mjs --release` で更新した旧 D1 fixture の準備から実 Worker の起動・HTTP 契約 104 件・作者追跡 6 シナリオまで成功し、本文 SQL 17 件も再実行した。Worker の Wasm Clippy `-D warnings` も成功した。
- 本文移行の保守スクリプトは `copy_done` / `verify_done` / `purge_done` と `backend` を見て完了済み段階を飛ばす。検証途中の再実行でコピーへ戻ってカーソルを消す不具合を、実 Worker の 5 参照・1 件検証済みからの再開で再現した（修正前は検証済みが 6 件へ増える）。修正後は 5 件で完了し、旧 D1 本文削除後の EPUB・全 S3 保存 smoke・HTTP 契約 104 件も成功した。
- 自動初期化が先に `migrating` を保存した場合も、`copy` は予定件数が未記録なら旧本文の件数・圧縮前サイズを台帳へ記録する。旧本文 4 件と新規 S3 本文 1 件の実 Worker で予定件数が 0 になる失敗を再現し、修正後は予定 4 件・検証済み 5 件を再開・切替後も維持した。標準 release Wasm、Wasm Clippy `-D warnings`、全 S3 保存 smoke、HTTP 契約 104 件が成功した。
- 本番の本文・履歴 3810 件を全件照合して `s3` へ切替え、旧 D1 本文を 3810 件削除した。残存旧本文・回収待ちは 0。本文は圧縮前 85,618,942 B / S3 保存後 26,284,224 B、D1 実容量は 3,026,944 B。切替後の S3 LIST も成功。SQL export を SQLite に復元し、目次・設定・挿絵索引等 214 オブジェクト、61 作品の全レコード、移行状態以外の 32 状態行が移行前と一致することを確認した。
- 425 話の本番 EPUB を D1 保存時、併用時、全話 S3 参照時、旧 D1 本文削除後に実取得した。削除後も全 445 XHTML が移行前と byte 一致し、唯一の差は OPF の `dcterms:modified`。移行前後の復元済み DB・EPUB・結果は gitignore 済み `sample/worker-content-backups/` に保持する。保守接続と一時資格情報ディレクトリは終了・削除した。

## 未リリース: R18 作者の全件列挙と Cookie 範囲

- `SiteSetting::cookie_hosts` に Cookie 送信を許可する追加ホストを YAML で宣言し、共通 `FetchPolicy` が適用する。既定のサイト・保存ログインのホスト範囲は維持し、未宣言の第三者へ Cookie を送らない。R18 専用の Rust 分岐は追加しない。
- `novel18.syosetu.com.yaml` を 2.5 に更新。作者プロフィールではなく `/mypage/novellist/xid/<x ID>/` を取得し、既存 DSL と `author_next_pattern` でページを辿る。作者用 `xmypage.syosetu.com` を Cookie 許可先に追加した。
- `x1525cp` のプロフィールは最新 8 作品、全作品一覧は 10 + 2 の 12 作品。Cookie が無い応答で一覧を隠す回帰 fixture を追加し、実 Native CLI の登録・dry-run で全 12 URL を照合した。実作品のダウンロードはしていない。Native の downloader 回帰 172 件が成功。
- Workers の `over18=false` は明示拒否で `Canceled` / `blocked` になるが、既存の汎用エラーは対話不能と表示する。許可は既存 Web UI「環境設定 → Global」の `over18` へ明示的に保存する。設定変更で既存の blocked ジョブを自動再投入しない。
- 本番移行前の D1 読み取り調査では `content_backend` が未設定（= `d1`）、`content_objects` は 0 件で、本文・履歴は旧 D1 に保存されていた。0.4.12 の配備だけでは新規本文も S3 にならないことを確認した。画像の S3 選択と本文の切替は独立している。
- 標準 release 設定の Wasm（wasm-opt 含む）を実行し、作者追跡 6 シナリオが成功。明示的な `over18=false` で同じ対象が blocked、Web 設定 API で true にした後は対話なしで取得成功し、単発作者取得は継続登録を作らないことも確認した。実 R18 作者は Worker HTTP API の dry-run でも全 12 URL が Native と一致した。検証プロセスと隔離 D1 は終了時に削除した。
- 統合 CI で、CLI テストに旧プロフィール取得 URL を固定する assertion が残っていたため、その実装依存の固定だけを削除した。通常版プロフィールを R18 として認識しない境界と小説情報リンクの除外は維持し、取得先の正否は Cookie 付き複数ページの全件列挙回帰で確認する。補正後の Native 全 workspace / all-targets は単一テストスレッドで 1348 件成功、参照 fixture 1 件 ignored。

## v0.4.12: Worker 本文・履歴の S3 保存

- `worker_entry/src/content_store.rs` と `0011_content_objects.sql`。話単位の既存 YAML を共通 codec で圧縮し、S3 の不変な版へ PUT 後に D1 参照を公開する。現行本文と cache / .cache 両形式の履歴が対象。本文ストアの目次・参照読み取りは primary に揃える。CRC32・SHA-256・圧縮前後サイズを検証し、S3 の欠落・破損を旧 D1 への fallback で隠さない。native と挿絵の保存方式は変更しない。
- `content_backend=d1→migrating→s3` は `asset_backend` と独立。管理 API `/api/admin/content-migration` の plan/copy/verify/cutover/purge/gc/status と `scripts/migrate-worker-content.mjs`（既定 plan、`--apply` で削除まで）を用意。phase の進捗は D1 に保存し、期限付き lease と SQL ガードで旧書込み・移行競合を拒否する。削除待ちは durable outbox、未完了 PUT は 1 時間保護。
- Worker の更新は `PersistenceService::cache_section` で履歴をコピーし、新本文 PUT 失敗時も現行参照を残す。改題の旧キーは新本文と新 TOC の保存後に削除する。native は従来の履歴移動を維持。旧 API `move_section_to_cache` は全呼出元を移行して除去した。
- ローカル Worker + D1 + S3 HTTP fixture で移行・実圧縮バイト・破損／欠落拒否・再起動・実体削除を確認。実取得・強制更新・正確な旧本文保存・差分一覧／消去・EPUB、履歴 COPY 成功後の新本文 PUT 503 でも現行本文と EPUB を維持すること、改題後の旧キー削除も成功。SQL regression 12 件を含む Python 64 件、release Wasm ビルド、Worker wasm Clippy `-D warnings`、切替後の既存 HTTP 契約 104 件が成功。期限切れ PUT の公開・旧 .cache 履歴の取り残し・新本文失敗時の現行参照消失は failing-before/passing-after を確認。本番の Wasabi 移行・本番データの削除は未実施。旧「本文を D1 の列へ展開する」方針は撤回し、AGENTS.md / COMMANDS.md / 設計資料を更新。
- 標準 release 設定の Worker ビルド（wasm-opt 込み）と、native の既存 cache 回帰 1 件も成功。ローカルではコンパイラ並列数を 1 に抑え、native の C archive は LLVM、テストの debug 情報は無効にして検証した（リポジトリ設定の変更なし）。
- サブディレクトリありで作品フォルダ名が `本文` の場合の目次誤分類を修正。`ContentStore::is_content_key` と `0012_content_metadata_boundary.sql` が `toc.yaml` / `converter.yaml` / `.illustration_cache.yaml` を D1 に残す。SQL の誤分類と、S3 切替後の実 EPUB が TOC 不在で失敗することを failing-before で再現し、修正後は同じ構成で purge・EPUB・再起動・既存 HTTP 契約まで成功。

## v0.4.12: 作者の継続追跡・管理

- `application::authors` の `AuthorStore` / `AuthorService` と portable `author` codec を共通境界にした。Native は Inventory の原子更新、Workers は primary D1 `app_state('inv','author')` の全 JSON スナップショット CAS。作者 URL → サイト名だけを保存し、小説の作者表示名とは区別する。
- Workers の更新 API・Update 投入・定期更新の最終ページから `author_check` を投入し、既存の実効 YAML / Cookie / 待機を使って新作だけを通常 Download ジョブへ流す。登録済み作品は `contains_toc_urls` の bounded primary 索引検索で除外する。小説 0 件でも動作し、自動停止設定は手動チェックを止めない。解除済みジョブと取得中の解除を再確認するが、投入済み通常 Download はキャンセルしない。
- Native / Workers 共通の「環境設定 → 作者」で登録・一覧・解除・手動確認。`GET/POST/DELETE /api/authors` と `POST /api/authors/check`。CLI は Native のローカルライブラリ専用とし、Workers 向けリモート CLI・専用引数・認証処理・初期化例外を撤去した。Native CLI の 0 番削除は引き続き拒否する。通常の作者 URL ダウンロードは単発のまま。
- 実 Worker + 隔離 D1 + 制御した HTTP origin で再起動後の登録、新作の継続取得、既知作品の除外、停止設定、dry-run、手動チェック、実 scheduled handler、取得中の解除、古いジョブのスキップを HTTP API 経由で確認。Firefox で Native / Workers の作者タブから登録・解除し、永続 API 状態を確認した。Worker の取得済み 6 作品は解除後も残る。
- Native は実 CLI の YAML 登録 → SQLite 移行、CLI / Web 双方向の一覧・解除、空ライブラリの CLI / Web 更新からの作者確認を実行し、WebSocket の実出力も確認。ユーザーの今回の許可により repo 内 TempDir を使用し、検証ライブラリ・プロセス・一時ソースを削除した。本番 D1 / S3 のデータ操作はしていない。
- 検証: Native 既定全 target 1331 成功、Native + Lite は単一テストスレッドで 1347 成功（どちらも参照 fixture 1 件 ignored）。Lite の並列全件実行はタイムアウトし、原因は未特定。Native + Lite / Worker の Clippy `-D warnings` と標準 release Wasm ビルド、Python 69 件、S3 保存・移行・EPUB の実 smoke と切替後 HTTP 契約 104 件が成功。既存の SQL ファイル複製を byte 比較するテストは削除し、移行後データ保全の実動作テストは維持した。
- develop 統合後も実 Worker の作者シナリオと Python 69 件を再実行し成功。Native は現在のソースからテスト実行ファイルをビルドし、作者関連 11 件・既存 DB の移行保全 4 件が成功した。統合後の Eval 経由全件待ちはタイムアウトし完走として数えていない。別の Cargo 実行も C 依存再ビルド中に中断されたため、最後はビルドとテスト実行を分離して確認した。
- CLI の対象を Native のみに修正後、実 Native バイナリで作者の登録・一覧・番号 0 の拒否・番号指定解除・保存状態を確認し、旧リモート引数は終了コード 127 で拒否され help からも消えた。Native + Lite の bin / CLI 境界回帰 116 件と、HTTP API だけで操作する実 Worker の作者追跡 smoke が成功。Web UI の作者管理・定期追跡・追跡解除後の作品保持は維持し、リモート CLI 専用の実装・初期化分岐・テスト経路を残していない。

## v0.4.12 リリース前検証

- Cargo / Worker / probe lock / ライセンス表示を 0.4.12 に統一し、依存クレートのバージョン・Git revision は維持した。ライセンス文書は CI と同じ cargo-about 0.9.2 で再生成し、直接依存 56 件の表も一致した。
- `cargo check -p narou_rs --features lite` が成功。Native + Lite の lib / bin / `cli_compat_boundaries` は計 1255 件成功し、実バイナリの `version --more` が 0.4.12 を表示した。
- `worker-build --release` で narou_rs / narou_worker 0.4.12 の release Wasm をビルドし、Python の SQL・配備回帰 69 件も成功した。
- 上記の release Wasm を実行し、隔離 D1 + 制御 HTTP origin による作者追跡 6 シナリオ、S3 本文移行・実圧縮データ・破損/PUT 失敗・改題・履歴・EPUB・purge・再起動・削除の実 smoke、S3 切替後の HTTP 契約 104 件が成功した。本番データの移行は実行していない。
- Linux CI で HTTP 104 件と作者 6 シナリオが成功した後も検証プロセスが終了しないログを確認した。検証用 Wrangler を Unix では独立プロセスグループで起動し、終了時は npx だけでなく Wrangler/workerd も対象にする実装へ変更した。Windows の `taskkill /T /F` は維持し、変更後も作者・S3 の実 smoke と HTTP 104 件が完走した。
- 修正後の Linux CI `38852697821` が完走した。Native 通常版・Lite の check/tests、全 feature Clippy、Wasm、ライセンス生成差分、Worker HTTP/作者追跡・S3 lifecycle が成功し、relay と develop Worker への配備も成功した。

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