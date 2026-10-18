# narou.rs 移植状況

## 現況（v0.4.14 公開済み・Workers 本番は未反映）

GitHub 最新公開 Release は v0.4.14（2026-10-18 07:00:43 UTC、ID416269164）。`/releases/latest` のタグ・draft=false・配布16ZIPを確認した。ユーザーがこの番号でのタグ作成・公開を明示承認している。main のリリースマージ8e52bb8へタグを作成し、developへもno-ff合流済み（41d58b7）。公開済みタグと配布物は差し替えない。

Cargo / Worker / 両 lock / 両 notice を0.4.14へ同期し、依存バージョン・Git revisionは変更なし。VERSION.mdに全未リリース修正を確定。cargo check、Worker wasm32 check、Native workspace/all-featuresのlib/bin/integration1399成功・参照fixture1ignored、実CLIの0.4.14を確認。main統合後も取得/JSON/ログイン/ID回帰26件とbudget回帰4件が成功。cargo-about0.9.2で両noticeを生成した。

ドラフトで生成を待ち、全16ZIPのGitHub SHA-256、CRC、PE/ELF/Mach-O形式、4実行ファイル、commitversion、ライセンス、Pixiv YAML/資産を照合してから公開した。Windowsの署名16件は全Valid、x64通常/GPLの実バイナリは0.4.14。target配下での実行は既存仕様でlocal-build表示になるため、署名済み配布物の検証はソースのtarget外のRust TempDirで実施した。

Workers本番は保護付きservice binding経由の実API（GET200）で0.4.13を確認。公開済み0.4.14は変更しない。SORAHOSTリレー配備失敗で本番反映が止まっていたが、ユーザーの指示でCIの提供元依存を除去する方針へ切り替えた。本番サービスやSecretの切り替え・削除は行っていない。

最後の旧CI39112462468は、送信予定2912769 bytesに対しsocket.bytesWritten=131420（ヘッダ等を含み、相手の受信量ではない）で停止し、writableLength=65536・writableFinished=falseのまま約35.3秒で本文なしHTTP408。TCP接続はIPv4で成立していた。ユーザー報告のPteWorkerログは空であり、再要求・無変更リトライはしない。原因をホスティング事業者の障害と断定しない。

CIからSORAHOST/run_workersの切替、専用配備workflow、relay-deployとWorkerの依存、提供元別DNS所有権判定を削除。Native/GPL/Wasm/Worker/licenseの通常ゲートと一般的なDNS競合時route切替は維持する。外部リレーはscripts/relay/、設定はNAROU_RELAY_ENDPOINT/NAROU_RELAY_KEYに統一し、旧名の別名は置かない。/proxy、X-Proxy-Token、JSON/base64、全Set-Cookie、非follow・上限とRust公開codec/with_relayは維持する。新名のSecretを用意する前に本番の旧リレーを無効化しない。旧ホストの手動native資料は残すがCIから切り離す。

検証: 実Node/ローカルworkerdを実HTTP上流へつないだ既存回帰29件、Rust relay codec9件、配備/DNS回帰26件が成功。Worker wasm32 cargo check成功。新名の設定で手動verify_auth.pyを実行しhealth200・誤認証403・正しい認証で上流200を確認し、直接リレー取得の本文一致・Cookie転送・2つのSet-Cookie保持も観測した。workflow YAML解析ではtargetのみの入力と通常アプリゲートへの依存を確認。新ホストへの実配備の証明ではない。

ユーザーは反映先に「developだけ」を明示選択。mainへの統合・本番配備は今回行わず、既定mainのCI入力表示も次の統合までは旧設定が残る。Cloudflare環境の新名NAROU_RELAY_ENDPOINT/NAROU_RELAY_KEYは照会時点で未登録だったため、旧リレーを継続利用する場合も本体の次回配備前に新名へ設定する必要がある。Secret値は読み出せないので勝手な自動移植はしない。

Wrangler D1のDB UUID指定照会は書き込み0件。旧ジョブj000001a14cc3660f66881bdbはcancelled by user via Web UIで取消済みで、同じtargetの後続ジョブは無かった。取消を勝手に復活させない。旧配備の診断用公開プローブは認証付きGET限定で使用後に停止・削除し、保存済みのローカルログイン資格情報は保持した。

## v0.4.13 公開時点の記録（現況ではない）

正本はAGENTS.md / COMMANDS.md。narou.rb24コマンド + Rust拡張db/illust/login/authorの計28。既存完了印の集計は24完了/4部分(download/update/convert/web)/未実装0。今回のQAだけでRuby互換の完了印を昇格しない。Cargo/Worker/probe lockとライセンス表示は0.4.13へ統一済み。GitHub の最新公開 Release は v0.4.13（ID415887473、2026-10-17 17:16:07 UTC 公開）。現行の16配布ZIPの SHA-256 が、事前に内容・CRC・対象アーキテクチャ・ライセンスとWindows署名を確認したファイルと一致することも再確認した。公開済みタグ・配布物は変更しない。追加の Web UI / Workers 修正は次版向けとして機能単位で develop へ no-ff 統合する。GitHub の常設ブランチは main / develop を維持する。

版数の判断基準は GitHub の公開済み Release。今回 `/releases/latest` は v0.4.13 を返す。manifest の版数が先行していても公開済みとは見なさず、次の版数を決める際もその時点の GitHub 最新公開版から判断する。現在の追加修正は `VERSION.md` の「未リリース」に記録し、次版の公開依頼前に勝手な版数更新・タグ作成・公開済みタグの差し替えを行わない。GitHub Release は全配布 ZIP の確認まではドラフトにし、公開完了は GitHub の実データで確認する。

0.4.13 検証: `cargo check --workspace --all-targets --all-features`、Native + Lite のビルドと実 CLI `version --more` の 0.4.13 表示が成功。標準 release Wasm を作り直し、実 Worker の `/api/version/current.json` が 0.4.13、HTTP 契約 103 件・作者追跡 6 シナリオ・全 S3 保存 smoke・Python 74 件が成功した。版数の非空だけを検査する旧 assertion は削除し、実値は使い捨ての API プローブで確認した。cargo-about 0.9.2 で両 notice を生成し、直接依存 56 件の表も一致。root 419 / non-GPL probe 394 の lock graph は自プロジェクトの版数以外に変更が無い。

共有化: application use-case、settings/setting_core、download_targets、retry_policy、push_events、MessageContext/progress、HTTP RedirectChain/send_paced、ObjectStore::update_small、EPUB EpubPreparation/EpubBuild。同時INI更新はFS lock/SQLite Immediate/D1 primary full raw snapshot CAS、正準保存後のミラー失敗もACKに反映。CRC衝突を同値扱いしない。Workerの出力/debug/表示先をfuture poll単位に束縛し、operation ID/seqで並列進捗を分離。native Web更新もサイト別並列取得。

待機は物理送信・redirect・transport切替ごと。Worker interval/wait-stepsはsite floorと合成し、待機中は設定snapshotの残TTLまでに再確認。60.0がJSON60になる初期値誤競合も修正。AozoraEpub3_Lite v0.1.6 pin381c0b17f91946e22d41e5246d3977c706abe82e。AssetPlanでI/O非依存の逐次画像収集、native/Workerは同じfinish/layout/処理を利用。上流所有repo developへのno-ff統合/push済み。

検証: v0.4.11 Rust full consumer suite(1124 lib + 111 bin + helpers/integration)、実Workerd HTTP/queue/replay104、Python52、JS progress/refresh/link20成功。CI既定Binaryen130で実Workerを再packageし、V8のmodule受付とWorkerd実取得を確認。workflow source文字列とfake curlのwiringテストは廃止。production SORAHOST shellを実HTTP originで検証(200/204/400/401/403各1回、503は6retry+diagnostic、exit0)。pwsh7.6.6で実Windows ZIPを作成し、本体/updater.exe.new/backup/loginと資産を確認(ローカル署名省略のみ、CI署名必須は維持)。

Web実ブラウザ: native並列2本の独立進捗、Worker TOC+8話の進捗、UI保存→実permit、wait-steps2の約370ms/5s交互、60→0.2の取得中反映を確認。到着間隔とpermit間隔は混同しない。履歴race、radio/console keyboard、mobile toolbar/dropdown、Worker行内folder capabilityを修正。個別dirty save/clear、notepad autosave/WS同期、取得中の履歴削除でバーを保持、再接続後終端で残留バー0を確認。Native EPUB raw novel_outputsのBrotli UTF8誤認500を実証しload_outputへ合流。Brotli/none/legacy deflateはvalid EPUB、unknown/corruptは500で拒否。Worker streaming EPUB実ZIPも確認。Workerの挿絵警告は参照あり・有効・実体なしだけに限定し、参照なし/欠落/無効の3ケースを実取得し入力を復元。元のDocuments/WebNovelは読取り専用、全ライブラリはrepo targetのRust TempDir controller所有。実Cloudflare配備/Cloud queue並列の検証をこのローカル結果へ含めない。

リモート旧ブランチ7本は未統合作業をno-ffでdevelopへ取り込み後削除済み(a4524f3)。追加修正も機能単位でdevelopへno-ff統合し、GitHubはmain/developだけを維持する運用。既定YAML、SQLite opt-in、global_settingは常に~/.narousettingファイル。native S3はlocal s3.*優先で未設定項目だけS3_*、挿絵だけS3でfail-closed。SORAHOSTはPteWorker公開/PORT loopback/server-ws-port=0/concurrency=true。loginはbrowser helper/export/import、複数名前/順序/UUID session/暗号化。

0.4.11 最終監査で確認したデータ保全の修正: SQLite のインプレース YAML 書き出しは meta の export_file_sha 指紋を原子的なファイル保存より前に記録し、失敗後の未編集の書き出し残骸を再取り込みしない。管理状態だけでなく database.yaml の作品レコードも対象。ユーザーの編集と narou-compat のファイル優先は維持する。S3 COPY は完全な CopyObjectResult と ETag/LastModified を確認できた場合だけ成功し、200 内 Error・破損応答では元オブジェクトを削除しない。実 HTTP の失敗／成功ケースと、書き出し失敗後の正常保存→再オープンを smoke で確認済み。
指紋は既存の未編集 projection と次回公開予定を最大2件保持し、2回目が公開前に中断しても保護を失わない。指紋更新と公開は同一ファイルロック内。--out が実 .narou を指す場合も対象。最終 workspace 全 target は1342成功/0失敗/1ignored、Clippy 全 feature -D warnings・Rustdoc -D warnings・core/Worker wasm check 成功。Web CSRF 回帰 fixture は並行CWD変更に影響されない絶対パスへ修正し、HTTPの拒否/許可とキュー状態の検証内容は維持した。

## 未リリース: Pixiv の閲覧範囲・作者列挙と通常エラーの再開（2026-10-18）

- Pixiv YAML を 1.5 に更新。isMasked / viewableType・明示的な閲覧制限・許可した作品404を区別し、一覧途中で落ちた話も page.total / seriesContents から検出する。候補ログインごとに最初の目次から取り直し、閲覧可能分だけ取得する。公開うごイラの original:null と Cookie の存在だけでは閲覧可否を断定しない。HTTP403は非公開扱いせず元の取得エラーにする。
- 保存ログイン後も0話なら Canceled とし、新規レコード・本文更新・自動変換を行わずID予約を戻す。Pixiv の preserve_missing_sections:true は非空の可視一覧と、話番号/URLが一致する旧本文を統合する。旧本文・TOCの読み取り失敗や曖昧な対応は成功にしない。forceでも権限を失った保存済み本文を消さない。
- 必須 fetch_json_required(url,[404]) と任意 fetch_json を分離。同じ資格情報/ヘッダの1小説内で最大256件FIFOを保持し、成功時の元JSONテキスト・値・失敗情報を1エントリに束ねる。本文再利用でキー順や空白を変えず、匿名/A/Bを跨いで共有しない。失敗はArcで保持し、Sharedエラーも元の失敗分類へ委譲する。NativeもHTTP数値状態を失わず既存transport fallbackを維持する。
- 作者の所属除外は author_series_episode_work_url / author_comic_series_page_work_url またはURL captureを使い、URL完全一致へ切替。数値IDが同じ別種の小説・イラスト・親シリーズを消す url_mentions_id は削除した。作者確認成功は列挙/子ジョブ投入完了であり、子の取得失敗/再開とは別。
- WorkerBudget は全ての話境界を記憶し、通常のretryableエラーでも100話未満から最終位置をawait保存する。旧周期書込みを回収してtoken fenceを維持し、破損再開の0位置も同じ実行文脈へ渡す。Downloaderは通常エラーでもdirty挿絵索引を保存する。失敗回数・backoff・上限は従来どおり。
- 転送先で元サイトの設定Cookieを混ぜる経路も修正。別サイトは転送先の同意/保存ログインだけを使い、レコードの古いrequires_login/sessionも消す。同一サイトの選択済みログインは保持する。
- 検証: Native workspace + Lite の lib/bin/integration は1399成功/参照fixture1ignored。Native全target/allfeatureとWorker wasmのClippy -D warnings、標準release Wasm + wasm-optが成功。追加のNative transport回帰29件も成功した。
- 実release WasmをWorkerd/隔離D1・DO・制御HTTP/S3 fixtureで実行。全話不可の見送り/ID再利用/自動変換なし、匿名公開分2話→権限付き3話、権限喪失後の旧S3本文保持とEPUBを確認。5話目の403でcursor4と画像4件を保存し、実1秒backoff前の配送を止め、期限後のrunningを観測、既存4話/画像を取り直さず5話を完了した。作者親成功→子retryable→子成功、実scheduled2回の新作追加と既知URL除外まで完走。最終5作品、本文・挿絵ともS3。実MyPixivアカウント権限やCloudflare本番を変更/検証した結果ではない。
- 本番は読み取りのみ。報告403の応答本文/headersは未保存で原因断定不可。API142766131の現在値はrestrict0/xRestrict1でR18、MyPixiv限定の証拠ではない。該当retryは5分期限後約1.4秒でrunningへ戻ったため、UI不具合を直したとは扱わない。版数はGitHub公開v0.4.13基準の未リリースのまま。CI監視・本番デプロイ/設定変更/ジョブ操作は行わない。

## 未リリース: Web コンソールの高さ調整と高解像度表示

- 共有 frontend に `console_size.js` を追加。左右ペイン共通の高さを下端ハンドルの Pointer Events と上下キーで変え、vh 単位で既存 localStorage helper に保存する。「表示」の専用リセットと全表示設定リセットは拡大状態も解除するが、ログ・進捗 DOM・過去ログの読書位置を保持する。キャンセル・blur・capture 解放を処理し、CSS から計算した範囲へ制限する。
- 主因は px 直書きではなく、コンソール10rem、本文104em、ヘッダー80em、文字87.5%の上限。FHD 超では root の相対文字サイズを最大125%へ段階拡大し、メイン幅85vwと初期高さ18vhを使う。幅広モードのコンソールだけを狭くする規則を除去し、phone の初期高さ6remをペイン側へ適用した。
- Native + Lite のビルドと変更 JS の構文確認が成功。Chrome DevTools の隔離タブで実ドラッグ194→574px、再読込で復元、表示メニューのリセット、上下キー、拡大から通常カスタム高さへの遷移を実操作した。100行と4本の進捗 DOMを置いた表示fixtureで、リセット後も内容・同一ノード・scrollTop=100が維持された。FHDは本文14px/幅1456px/高さ194px、WQHDは16.33px/2176px/259px、4Kは17.5px/3264px/389px。390幅でも左右96px・横溢れなし。モバイル emulation はページを再読込するため、DOM fixture の保持とは混同しない。Firefox と共有 browser broker は起動失敗、独立 Chrome DevTools で確認した。
- 初期化は main.js から呼び、既存のキャッシュ済みエントリポイントも更新されるようにした。最終 Native 配信で更新された main.js とハッシュ付き import を確認し、4K の実画面で上下キー389→414px・表示メニューのリセット389px／保存値解除・横溢れなしを再確認した。Worker は同じ38資産をハッシュ付きで生成する。ライブラリは Documents/WebNovel 内の Rust TempDir controller 所有で、元ライブラリやログイン資格情報は変更しない。

## 未リリース: cargo local-build の GPL 版

- `--gpl` を既存 Options に追加し、本体だけを `--features lite` 付きで release build する。updater / backup / login・updater SHA-256 の埋め込み・資産コピー・`narou/` の再作成は既存経路を共用する。GPL 時は `Third-Party-License.md`、通常時は `Third-Party-License-non-GPL.md` を選び、梱包内ではどちらも `Third-Party-License.md` に揃える。`--locked` / `--target` の転送は維持する。
- 既存の repo 直下 `narou/` を削除しないよう、Rust TempDir 所有のローカル副本で実 `cargo local-build --gpl --locked` を実行した。全バイナリ・preset・webnovel の梱包と GPL notice の byte 一致を確認し、AozoraEpub3 未設定で梱包本体から Lite を強制して実変換した。出力の Kobo EPUB は CRC 正常・正しい mimetype で、XHTML に保存済みの日本語本文を保持した。引数パーサーや転送だけを固定する恒久テストは追加しない。
- 続けて同じ副本で実 `cargo local-build --locked` を実行し、通常版の4バイナリと non-GPL notice の byte 一致、梱包本体の起動を確認した。Lite 強制時は組み込みエンジンを含まないエラーを表示し EPUB を生成しないため、直前の GPL feature が残らない。変換コマンド全体の exit0 は既存挙動で変更しない。repo 直下の既存 `narou_rs.exe` の SHA-256 は検証前後で一致した。Cargo helper は28個の narou コマンドには含めず、COMMANDS の完了数は変更しない。

## 未リリース: Worker のサイト別直列化と変換枠の分離

- `NAROU_JOBS` は取得4枠、`CONVERT_QUEUE` は変換1枠と独立DLQ。`CONVERT_QUEUE_NAME` は物理レーンを識別する。CI の provision / render / deploy、ローカル・develop・production の TOML、cron / 手動再投入 / 旧レーン転送を同じ kind 判定へ揃え、単一 D1 台帳を維持する。新しい secret の手動設定は不要。
- 実効 YAML の `domain` を site key として Download / Update / AuthorCheck を直列化する。host の文字列比較やサイト固有の分岐は使わない。競合は durable admission waiting + ACK で、provider retry・業務上の attempts を消費しない。終端・cron の waker はサイトごと1件と独立した変換を選び、配送予約後の消失は既存 reaper で回復する。
- 既存作品を別URLで取得中、数値IDの変換が同時に成功してしまう境界を実 Workerd で再現した。migration0014 の `normalized_target` に YAML 解決後の TOC URL を持ち、元の target / 計画 / checkpoint を変えず原子的 claim と waker で照合する。新規作品の採番後も保護する。旧 owner の同一性が未解決なら drain / lease 回復まで保守的に新規 claim を止め、キャンセル中も実 HTTP が終わるまで token / lease を保持する。
- real Wasm / D1 / DO と透明な実 Queue producer observer を使う使い捨てプローブで、4サイトの本文ストリームを全て保持したまま別作品の実変換と `novel.txt` を確認。同一サイトの別hostは5回配送しても待機・ACK・attempts0・originアクセス0。旧レーン転送の両方向、失われた変換配送の実cron回復、同一作品の Update / Convert 排他、別URL Download / 数値ID Convert 排他、実キャンセルAPIとHTTP drain後の後続起動も確認した。Cloudflare 本番の autoscaler を試した結果ではない。
- 1000話を実取得し、850/851 subrequest の2回の区切り（最終プローブの再開位置411/823）を同一 job ID・attempts0 で続行し、全1000話が各1回の本文取得で揃った。600秒 wall-clock の実待機は行っていない。同じ continuation 経路の checkpoint / token fence を SQL 回帰でも固定し、上限値を上げず、日本語で自動再開を案内する。
- Native の queue lane と保存方式は変更しない。Worker の Convert 実行は従来どおり数値 ID を要求し、URL Convert の追加対応はしない。変換だけの invocation は Downloader を構築しない。`concurrency` はコンソールの表示先で、Worker の実行枠数を変える設定ではない。
- 最終の実 Worker で作者追跡6シナリオ、HTTP / Queue / WebSocket / reload 契約103件、全 S3 本文保存・移行・失敗注入・EPUB・再起動 smoke が成功。Native workspace（lib / bins / integration + Lite）は1,373成功・参照fixture1 ignored。Native / Worker wasm の Clippy `-D warnings`、標準 release Wasm + wasm-opt も成功した。develop への no-ff 統合後も SQL claim 回帰14件と Python74件が成功し、実 repo のビルドドライバを再生成した。検証用3サーバ・Rust TempDir・ソース副本・target junction・使い捨てスクリプトは終了・削除済み。次版の CI 監視はユーザー指示どおり行わない。


## 未リリース: 挿絵を含む Worker 取得の実行枠再開

- `Downloader` は話境界で `DownloadBudgetExpired` を返す前に dirty な `.illustration_cache.yaml` を保存する。本文だけ保存して画像 URL → ファイル名の対応を失い、再開時に同じ画像を取り直して再び実行枠を消費する経路を修正した。旧中断データの索引復元は残し、復元だけで再度予算に達してもその対応表を保存して次の実行へ渡す。
- 実体一覧も取得開始時の snapshot のままだったため、再開後に新しく保存した画像を本文 / 取得時 HTML の重複参照で二度取っていた。`IllustrationStorageService::store_bytes` が返す成功済み filename を既存の集合へ move し、保存失敗時は追記しない。別の「取得済み」集合やサイト固有分岐は追加しない。
- 回帰 `budget_resume_keeps_illustrations_without_refetching` は fresh HTTP / section cache で、取得済み画像の再取得による同位置中断と、新しい画像の二重取得をそれぞれ修正前に再現した。修正後は旧画像 0 回・新画像 1 回で続きの本文を保存し、索引の EPUB 参照も解決する。中断時に索引が無いという旧 implementation assertion は削除した。
- 標準 release Wasm の実 Workerd / ローカル D1・DO / S3 HTTP fixture で、9 話・450 画像を既定 850 soft subrequest budget の話境界（cursor 6）で分割し、同じ job ID・attempts 0・画像各 1 回で完了した。索引だけ欠落させた旧状態も cursor 6 → 6 → 完了へ進み、復元の 1 巡だけ画像を再取得して以後のループを止めた。600 秒 wall-clock の実待機や本番デプロイの検証ではない。
- 手動 Download と実 cron Update で、公開取得が匿名のまま、404 / 部分一覧が保存ログインへ fallback し、成功した session を次回の最初から使うことを実 Worker で確認した。保存済み年齢同意 true は双方で許可、false は双方で停止し現行本文を保持する。`download.interval=0.15` と YAML 下限 0.3 秒、`download.wait-steps=3` の 5 秒休止も同じ取得経路で確認した。自動更新用の認証・速度の別実装は不要で、Pixiv の 5 秒下限は変更しない。
- 最終 Native workspace + Lite は 1,373 成功 / 参照 fixture 1 ignored、Native 全 target / Worker Wasm の Clippy `-D warnings` と標準 release Wasm + wasm-opt が成功。本番 D1 は読み取りだけで、該当 Pixiv ジョブの checkpoint 7 と現行本文 7 話を確認した。直前の last_error には別途 Pixiv 追加 API の HTTP 403 も記録されており、この修正で 403 を解消したとは扱わない。CI 監視・本番の設定変更・ジョブ操作は行わない。
- 最終 Wasm でも認証・速度・年齢拒否と両方の挿絵再開ケースを通し直した。検証状態は Documents/WebNovel 内の Rust TempDir 所有で、元ライブラリは変更しない。実 Workerd は終了し、全検証ディレクトリ・使い捨てソース・専用実行ファイルを削除済み。Cloudflare の一時認証参照も解除し、ユーザーの Wrangler ログインは保持する。

## v0.4.13: 既存作品の個別変換設定を正準ストアへ統一（issue #37）

- ミラー無効の SQLite ライブラリで、Web UI は正準 INI を更新するが `NovelSettings::load_for_novel_with_options` は移行前の実ファイルを読んでいた。`native::converter::load_novel_ini` に読み出しを揃え、残存ファイルより SQLite の値を優先する。旧ファイルを上書きして整合させず、YAML / ライブラリ外のファイル読み出しは従来どおり。共有コンストラクタの呼出元にも反映され、公開シグネチャや default / force の優先順は変えない。
- Documents/WebNovel 配下の Rust TempDir に旧ファイルが残る作品と DB のみの作品を用意。実 Web API が新しい題名・著者名を保存しても、修正前は旧作品だけ生成 txt の先頭 2 行が旧設定、新作品は新設定になることを確認した。
- 修正後は実ブラウザの個別設定画面で題名・著者名を編集・保存し、実 CLI が `[更新した著者] 更新した題名.txt` を生成、本文の先頭も新しい題名・著者名になった。旧 `setting.ini` は元の 2 行のままで、読み出しによる上書きは無い。Firefox と browser broker は起動できず、独立 Chrome DevTools の検証タブで実画面・保存を確認した。検証タブ・サーバ・ライブラリは終了・削除済み。
- `cli_compat_boundaries` 6 件と settings の既存 7 件が成功。新しい回帰はミラー有効 / 無効、旧ファイルあり / DB のみ、再編集・設定解除、生成本文の題名・著者名・本文、旧ファイル不変と取り出しファイルの後片付けを実 CLI で検証する。版数は未リリース 0.4.13 のまま。
- 両 issue 修正後の最終検証: `cargo test --workspace --all-targets --all-features` は 1,367 成功 / 参照 fixture 1 ignored、workspace 全 target / 全 feature の Clippy `-D warnings` と Worker wasm32 check が成功した。依存・lock・ライセンス表示の変更は無い。

## v0.4.13: clean の長時間走査を修正（issue #38）

- `clean` のストア走査は小説全体の再帰一覧を 512 件ずつ取得し、そのたびに SQLite / 実ファイルの全キーを集め直していた。`NativeStore::list_direct_files_sync` で raw / 本文直下だけを取得し、SQLite は object_key の索引範囲と直下判定を使う。正準 DB と実ファイルのみの孤児を併合し、履歴や挿絵を歩かない。
- Documents/WebNovel 配下の Rust TempDir に履歴 4,096 件を持つ SQLite ライブラリを作り、実 CLI で修正前は `clean -af` / 個別 clean が各 10 秒の検証期限を超えた。同じプローブで修正後は約 183ms / 162ms で正常終了し、孤児の削除と現行本文の保持を確認した。ユーザー報告の 1 時間待機そのものを計測した結果ではない。
- `commands::clean` 2 件と `cli_compat_boundaries` 5 件が成功。追加の回帰はミラー有効 / 無効、DB のみ・FS のみの孤児、513 件の履歴、凍結作品、dry-run の不変性を実 CLI と正準オブジェクトの全 snapshot 比較で確認する。版数は未リリース 0.4.13、CI の監視はユーザー指示で issue 対応後へ延期する。

## v0.4.13: Worker 設定編集の偽の競合を修正

- develop の配備 run `39035933191` は Worker のビルド・配備・S3 LIST が成功し、smoke の「stale expected value」検査の最初の保存だけ HTTP 409 (82/83 成功) で失敗していた。設定 GET は replica / isolate TTL cache、POST は primary / fresh という不一致が原因。
- 実 Rust Worker Wasm を 2 isolate + 共有ローカル D1 に載せ、reader のキャッシュを温めて writer から保存すると、新しい編集 GET が旧値を返して初回保存から 409 になることを再現した。`GET /api/global_setting` も既存 `metadata::settings_for_writes` で primary / fresh を使い、一覧・通常の表示設定のキャッシュは変更しない。scope 単位 CAS と本物の stale save の 409 は維持する。
- 同じ使い捨てプローブで修正後は他 isolate の勝者を取得・初回保存 200・古い画面の保存 409・勝者保持を確認。失敗した既存 HTTP 契約も実ローカル Worker に対して 1 件成功し、契約の勝者検査は no-op POST ではなく編集 GET の現在値で確認するよう更新。標準 `worker-build --release` (wasm-opt 込み) と Worker Wasm Clippy `-D warnings` が成功。
- ローカル Miniflare の workerd は compatibility date 2026-08-06 までのため、その日付で検証した。PushHub はこの metadata 専用 fixture に接続しておらず配信の検証には数えない。元ライブラリ・資格情報には触れず、Rust TempDir 所有の npm/runtime 領域・使い捨てソースは削除済み。版数は未リリース 0.4.13 のまま。

## v0.4.13: 機能ツアー

- 共通の `application::webui::feature_tour::FEATURE_TOURS` に、アルファポリスの小説・作者一覧対応とカクヨムネクスト対応を追加。無料分、レンタル・契約に応じたログイン Cookie の利用、既存本文の保護を説明し、通常の公開作品へのログインを要求しない。
- GitHub `/releases/latest` が v0.4.12 を指すことを確認し、ユーザー指定の v0.4.13 を維持。root / Worker manifest、各 lock、ライセンス表示は既に 0.4.13 のため変更しない。未公開版をさらに 0.4.14 へ進めない基準を AGENTS.md に保存した。
- 実 Native Web UI を Documents/WebNovel 配下の Rust TempDir で起動し、Firefox で両項目と v0.4.13 表示・横溢れなしを確認。旧ツアー既読状態から新項目だけ表示され、既読保存後の次回画面読み込みでは表示されない。全ツアー API には両項目が残る。検証タブ・サーバ・ライブラリ・使い捨てソースは終了・削除済み。実 CLI `version --more` も 0.4.13 (local-build) で正常終了した。
- `cargo check --workspace --all-targets --all-features`、Native 全 workspace / 全 target / 全 feature の 1,365 成功・参照 fixture 1 ignored、Worker Wasm check が成功。最新ツアーを 0.4.3 に固定した実装依存 assertion は削除し、新版への固定し直しは行わない。既読状態の後退防止・表示対象の版数境界の回帰は維持した。依存・lock・ライセンス表示の変更はない。

## v0.4.13: カクヨムネクストのログインと無料分取得

- `webnovel/kakuyomu.jp.yaml` を version 2.6 へ更新。`login_url` は `/auth/login`、`login_partial_pattern` は DSL の中間目次に出る `EmptyEpisode`。保存済みログインを既存の共通経路で試し、改善した資格情報を本文取得と次回更新に使う。サイト固有の Rust 分岐は追加しない。
- ユーザーの指定に従い、未ログイン・閲覧権なしでも警告付きで無料分を取得する。無料分から、Cookie 保存後の全話への追加取得も可能。通常の公開作品には保存 Cookie を送らない。
- 共通 downloader は部分一覧に保存済みの ID が欠ける場合、削除・ダイジェスト化・404 凍結とは判定せず `Canceled` で更新を見送る。force でも目次・本文を保持し、閲覧可能 0 話の部分一覧は空の小説として登録しない。ファイル出力名の変更もこの判定後に行う。警告は共通 `application::messages::download` から出す。
- 本文 URL が作品トップへ戻され、本文枠が無い場合は YAML / DSL の `fail` で止め、空本文をキャッシュ・保存しない。`extract` は既存例と同じく `let` で受け取る（条件式内の直接呼び出しは評価されない）。
- 指定作品 `2912051601247823656` は匿名の実ページで全 129 話のうち `Episode` 50 / `EmptyEpisode` 79。実 Native CLI で無料 50 話の本文ファイルと目次、レコードの `general_all_no=50`、Lite EPUB 65 エントリを確認し、再更新は変更なし。通常作品 `2912051601856437328` も 2 話の実取得・EPUB 生成が成功。制限された第 51 話は作品トップへ戻り、直接本文を取る経路でもエラーになることを確認した。
- 実 Edge と `narou_rs_login` がカクヨムのログイン画面を開くことを確認。ユーザーが NEXT 契約アカウントの Cookie を取得した後、実ライブラリ ID:24 (`2912051601247823656`) で匿名の無料 50 話から全 129 話へ取得範囲が増え、本文・EPUB・再更新の変更なしを確認した。資格情報と本文は実ライブラリへ保持する。Edge は Windows の `USERPROFILE` を架空の空ホームへ変えると DevTools 起動に失敗するため、ブラウザは実環境を継承して `--profile` のみ隔離する。
- `tests/kakuyomu_login.rs` の 6 件で、匿名・権利なしの無料分、資格情報の試行・セッション ID 再利用、無料分からの追加取得、権利失効後の保持（force 含む）、0 話拒否、公開作品への Cookie 非送信、制限画面の空本文化防止を確認。変更前の失敗を確認してから修正した。Native workspace / all-targets / all-features は 1,366 成功・参照 fixture 1 ignored、Native / Worker Wasm Clippy と Rustdoc は `-D warnings` で成功。
- 実サイト検証は Documents/WebNovel 配下の専用領域と Rust TempDir で行い、元ライブラリの ACL は変更しない。unit suite の `TEMP` / `TMP` は workspace `target` に向ける（通常の TEMP は sandbox 書込み不可、WebNovel 配下だと「未初期化」のテストが祖先の `.narou` を発見してしまう）。コマンド集計は 24 完了 / 4 部分 / 0 未実装を維持する。実 Cloudflare 配備・取得は今回の検証に含めない。

## v0.4.13: アルファポリスとログイン Cookie

- `webnovel/www.alphapolis.co.jp.yaml` (version 1.4) を追加・更新。小説トップ・各話 URL を作品トップへ正規化し、`app-cover-data` の配列順で章と閲覧可能な話を読む。未レンタル話は `login_partial_pattern` で検出し、無料公開・レンタル済み分だけを取得する。章名は最初に閲覧できる話へ付け、全話が閲覧不可の章は出力しない。題名・作者・あらすじ・タグ・日時・文字数・完結・R18 判定、本文の CSRF フォーム送信、作者一覧とページ送りは YAML / DSL に置き、サイト専用の Rust 分岐は追加しない。
- 汎用 DSL に `extract(/.../)` / `post_form(url, fields, headers?)` / `fail` を追加。フォームは同一 origin のみ、資格情報ヘッダ注入は拒否。URL・本文・ヘッダで要求を区別し、結果は 1 回の前処理だけ保持する（257 話の同一 POST 先でも混同・累積しない）。失敗時に途中の source を公開しない。
- `cookie_session: true` は FetchPolicy 内だけの Cookie を origin・Path・Secure・期限で制限する。保存ログインとは分離。`illust_from_raw: false` は解析済み本文・前後書きだけを走査し、空の reader ページにある UI 画像を取らない。実取得で `data:image` を URL として要求した問題を再現し、本文画像だけの実体・索引保存を回帰で確認した。
- 指定作品 `166360898/202065994` は実 Native CLI で全 36 話保存・更新なし・青空テキスト 32,556 文字・Lite EPUB 65 エントリとルビを確認。指定作者は 17 URL の集合を照合し、取得済み 1 件を除く 16 件を実 `author check --dry-run` で確認。別作者 `670425836` は 2 ページの 39 作品を列挙。短い作品 `260002762/818069876` も目次 3 話と本文を実取得した。
- `666146886/441062651` の未レンタル話を含む目次で全体が NotFound になることを匿名・保存 Cookie の両方で再現し、YAML で部分取得へ修正。実 CLI で無料公開 167 話・22 区分を保存し、ユーザーがレンタルした 6巻4話 (`1639957`) を通常の update で追加、合計 168 話・`requires_login=true` と session ID の保存を確認。既存 167 本文はハッシュ不変。青空テキストと Lite EPUB の 22 大見出し・168 話見出しを順序まで照合し、全ナビゲーション先と ZIP CRC が正常 (414 エントリ)。再更新は変更なし、目次と本文 169 ファイルのハッシュ不変。
- R18 指定作 `528033569/927510111` は新規の匿名 Cookie セッションで HTTP 200、目次 4 話・第 1 話本文 6,175 bytes を取得でき、ログイン必須ではなかった。既存 `over18=false` は取得を拒否してレコードを作らない。未レンタル話が残っても取得範囲を警告して閲覧可能分を保存し、保存済みの話が欠ける部分一覧では更新を見送る。電子書籍特典は未対応。
- `narou_rs_login` は親ドメイン・作者/トップ URL でもサイトを選択する。末尾 2 ラベルだけでは他の `*.co.jp` まで同じログインへ混入することを failing-before で確認し、`psl` による登録ドメイン判定へ置換。Cookie の収集・サイト名への畳み・親ホスト検索を修正した。新規依存 `psl` と probe lock、直接依存 57 件の表、GPL / 非 GPL notice を同期。
- 共通 downloader の「prefetched GET のログイン壁で fallback 前に return」「HTTP 200 の壁が残る資格情報を採用」「error_message が無い壁から題名なし・0 話で成功登録」を再現し修正。親ドメインの Cookie、期限切れ→有効な資格情報の選択、本文フォームへの適用、session ID 保存、次回先頭からの利用を回帰で確認。helper の保存後検証も共通 fetch / YAML 前処理へ統一した。
- 実 Edge のまっさらなプロファイルで `www.alphapolis.co.jp` 4 Cookie + 親ドメイン 1 Cookie を取得し、1 ログイン・2 ホストで暗号化保存、暗号化 version 3 export、実 CLI import/list、保存 Cookie 付き作品取得確認まで成功。Chrome はこの環境で DevTools ポートへ接続できず、Edge を明示指定した。追加検証ではユーザーの実アカウント Cookie でサイトの `login_status=true` とレンタル済み本文の取得を確認。購入・レンタル操作そのものはユーザーが行い、検証プログラムからは実行していない。
- 先行の使い捨て検証は Documents/WebNovel 配下の専用領域と Rust TempDir を使い、元ライブラリの ACL は維持した。専用領域のブラウザプロファイル・使い捨てソースは削除済み。今回ユーザーが保存したログイン Cookie と、実ライブラリの ID:25 の本文・EPUB は保持している。
- 今回の Native workspace / all-targets / all-features は分割実行で 1,367 成功（lib 1,135 + bin / integration / examples 232）・参照 fixture 1 ignored。全 target Clippy `-D warnings` と Worker Wasm check が成功。Alpha/NEXT の対象回帰は 14 成功。依存・ライセンスは今回変更なし。コマンド集計は 24 完了 / 4 部分 / 0 未実装を維持する。実 Cloudflare 上の取得・配備はこの結果に含めない。

## v0.4.13: DL 失敗時の ID 予約回収

- 新規取得前に進めた ID を失敗時に戻さない問題を、404 の後の成功が ID:1 になる failing-before 回帰で再現。`NovelRepository::allocate_id` を ID + 世代番号の `NovelIdReservation` に変更し、未登録のエラー・キャンセル・保留と、既存作品へ解決された不要な予約を `release_id` で返す。既存作品・過去の欠番は詰め直さない。
- Native Database / Memory repository は共通 allocator、SQLite / D1 adapter は migration 0013 と共通 SQL を使う。並行予約は一意、古い世代の解放は無効。upsert が予約を確定し、レコードが存在する ID はエラー後も再利用しない。登録済み ID を削除しても過去の予約から復活させない。
- 実 CLI の YAML / SQLite 両モードで、同一バッチの存在しない作品 → 公開 3 話の作品が ID:0 に登録され、失敗を含む終了コード 1 と、その後の既存 ID 更新を確認。検証は Documents/WebNovel 配下の Rust TempDir で隔離し、元ライブラリや資格情報は変更しない。
- 実 Miniflare / workerd のローカル D1 で production の予約 SQL と migration 0013 を実行し、16 並行予約の一意性・解放した 8 件だけの再利用・古い解放要求の拒否・登録済み/使用中 ID の保護・手動登録後の採番境界を確認。Worker handler からも同じ状態を読み出せた。実 Cloudflare 配備やリモート D1 での検証ではない。使い捨てソース・npm ツール領域・検証ライブラリは削除済み。
- Native workspace / all-targets / all-features は 1,366 成功・参照 fixture 1 ignored。新 migration の追加で壊れた「最新版は 12」という実装依存 assertion は削除し、v6 / v8 の実データ保存と失敗時の rollback 回帰は維持した。Native 全 target Clippy と Worker Wasm Clippy は `-D warnings`、Worker Wasm check も成功。依存・ライセンス・版数は変更せず、コマンド集計は 24 完了 / 4 部分 / 0 未実装を維持する。

## v0.4.13: Workers の本文を S3 基本にする

- `composition::build_object_stores` は S3 構成成功後に `content_store::initialize_s3_default` を通す。未設定なら旧本文・履歴の存在に応じて `migrating` / `s3` を primary の単一 SQL で保存し、新しい本文は S3 に置く。明示的な `d1` は非必須環境だけに残し、`NAROU_REQUIRE_S3=true` では同じ規則で進める。`migrating` / `s3` は戻さず、失敗を D1 保存で隠さない。Native は変更なし。
- 自動初期化は既存本文の一括コピー・削除を行わない。管理 API の `plan` / `status` は未設定状態を `d1` と報告する読み取り専用で維持する。旧本文の照合・切替・削除は既存の再開可能な移行処理を使う。
- 標準 release Wasm（wasm-opt 込み）、Python 74 件（本文 SQL 17 件を含む）、実 Worker の既存本文移行・更新失敗・履歴・EPUB・purge・再起動と HTTP 契約 104 件、作者追跡 6 シナリオが成功。追加で未設定の新規ライブラリ、旧本文あり、S3 必須の明示 d1、S3 設定不正、初回 PUT 失敗と復旧の 5 ケースを隔離 D1 + S3 HTTP fixture で実行した。
- develop 統合後、`tests/run.mjs --release` で更新した旧 D1 fixture の準備から実 Worker の起動・HTTP 契約 104 件・作者追跡 6 シナリオまで成功し、本文 SQL 17 件も再実行した。Worker の Wasm Clippy `-D warnings` も成功した。
- 本文移行の保守スクリプトは `copy_done` / `verify_done` / `purge_done` と `backend` を見て完了済み段階を飛ばす。検証途中の再実行でコピーへ戻ってカーソルを消す不具合を、実 Worker の 5 参照・1 件検証済みからの再開で再現した（修正前は検証済みが 6 件へ増える）。修正後は 5 件で完了し、旧 D1 本文削除後の EPUB・全 S3 保存 smoke・HTTP 契約 104 件も成功した。
- 自動初期化が先に `migrating` を保存した場合も、`copy` は予定件数が未記録なら旧本文の件数・圧縮前サイズを台帳へ記録する。旧本文 4 件と新規 S3 本文 1 件の実 Worker で予定件数が 0 になる失敗を再現し、修正後は予定 4 件・検証済み 5 件を再開・切替後も維持した。標準 release Wasm、Wasm Clippy `-D warnings`、全 S3 保存 smoke、HTTP 契約 104 件が成功した。
- 本番の本文・履歴 3810 件を全件照合して `s3` へ切替え、旧 D1 本文を 3810 件削除した。残存旧本文・回収待ちは 0。本文は圧縮前 85,618,942 B / S3 保存後 26,284,224 B、D1 実容量は 3,026,944 B。切替後の S3 LIST も成功。SQL export を SQLite に復元し、目次・設定・挿絵索引等 214 オブジェクト、61 作品の全レコード、移行状態以外の 32 状態行が移行前と一致することを確認した。
- 425 話の本番 EPUB を D1 保存時、併用時、全話 S3 参照時、旧 D1 本文削除後に実取得した。削除後も全 445 XHTML が移行前と byte 一致し、唯一の差は OPF の `dcterms:modified`。移行前後の復元済み DB・EPUB・結果は gitignore 済み `sample/worker-content-backups/` に保持する。保守接続と一時資格情報ディレクトリは終了・削除した。

## v0.4.13: R18 作者の全件列挙と Cookie 範囲

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