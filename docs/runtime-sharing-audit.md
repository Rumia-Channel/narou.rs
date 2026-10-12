# native / Cloudflare Workers / SORAHOST 共通化監査 (2026-10)

対象: 「Workers 向け・SORAHOST 向けに積んだ機能のうち、実行環境に依存しないのに native と
共通化されていない経路」の洗い出し。初回監査はコード変更なし。2026-10-12 にシリーズ展開を共通化し、後続の実効性監査で登録・保存・設定・Web UI・I/O の優先修正を実施した。

- 初回監査時点の HEAD: `6434f27`〜`4ff29d3`（監査中に他作業で `worker_entry/ci/*` と
  `.github/workflows/platform.yml` が 2 コミット進んだ。本ファイルが引用する
  `src/**` と `worker_entry/src/**` は未変更）
- 方法: `src/**` / `worker_entry/**` / `sorahost/**` / `scripts/sorahost-proxy/**` を読解し、
  `grep` で「共有層の利用有無」を機械的に確認。主要な重複は本文を diff して一致を実測した。

---

## 2026-10-12 追補: ランタイム実効性監査の優先修正

シリーズ展開後の追加監査で確認した作者登録、Worker/Web UI の無効設定・未反映処理、
SQLite の保存境界、不要な往復と並行 I/O を修正した。下の旧監査表は調査時点の記録であり、
本節と重なる項目は本節の実装を優先する。単なる名前・定数の統合と、意味のある動作変更を区別する。

| 優先修正 | 実装した境界・挙動 |
|---|---|
| 作者・ユーザー URL | `AuthorListing` と `DownloadTargetExpander` を CLI / Worker / CLI 作者追跡で共用。小説・シリーズ判定を優先し、単発登録と継続追跡を分離。実効 YAML、複数ログイン、ページ送り、シリーズ所属作品の除外を同じ規則で処理する |
| SQLite の誤削除・不完全復帰 | TOC のストア上の存在で登録を維持。バックアップと YAML 復帰に本文・個別設定・ユーザー定義・挿絵を含め、全書き出し成功後だけマーカーを切替。削除は残った FS コピーを先に消し、旧ファイルから復活させない |
| canonical 保存先 | 変換前に設定を取り出す。設定焼き込み、挿絵索引、差分復元、clean / illust / inspect、起動時話数補完をストアへ接続。壊れた TOC・挿絵索引は削除を進めない。同期 converter のストア I/O は Tokio 内でも安全に実行する |
| 一時取り出し | 同一作品の利用者がスコープを共有し、最後の利用者だけが自分たちの作成ファイルを消す。プロセス間は作品単位のロック。ミラー有効時の復元は残す。既存ユーザーファイルを上書きせず、不完全ファイルを atomic publish しない |
| Web 設定の nil / dirty / CAS | 三状態で未設定を保持。変更項目と表示時の期待値だけを送信。保存先ごとの CAS、HTTP 409、未送信・未知キーの保持、replace の競合検査。未変更保存は POST も書き込みも行わない |
| Worker 能力と表示 | 共通 capability payload で非対応操作を制限。native 専用 S3 秘密値入力を隠し、無効設定を API でも拒否。concurrency はコンソール分離、keep-txt は既定 false、retry は最大 3。D1 固定を正しく説明し、取得失敗を YAML と推定しない |
| EPUB | 設定生成と extra assets の消費を両 builder で共有。ストリームでも横書き、挿絵の除外、CSS / DMincho、行高、自動表紙を出力に反映。候補は本文順で探し、保持は 1 枚、書き出しで再利用。非 EPUB 端末と native-only inspect を拒否 |
| 履歴・通知・更新 | PushHub の保持履歴を実際に取得・消去。再接続でも消去済み行を復活させない。設定保存の config reload と台帳の待機・実行数による table reload timing。進捗・通知は抑止しない |
| Worker / UI の往復削減 | enqueue / Queue 送信の batch 化、freeze snapshot、必要 ID の投影、選択レコードのみの取得、pending queue の keyset paging。設定・別入力の独立読取と login payload の一度読み、最小 backup status、debounce / single-flight の更新合流 |
| SQLite と bounded I/O | import / sections / versions / CRC verify / prefix delete を transaction / join / batch 化。本文・再開は 4 件の先読み窓で順序と最初の失敗を維持。小さい既知の S3 オブジェクトは最大 4 件・見積り 8 MiB、大きい／未知サイズは単独。画像 stat も上限付き |

### 制約を残す理由

- 同一サイトへの無制限取得、Worker の CPU 変換、同じ SQLite connection への並行 query は増やさない。
- EPUB の全画像先読みはしない。本文の窓は追加 I/O の上限であり、変換全体の RAM の厳密な上限ではない。
  S3 の 8 MiB も一覧サイズによる in-flight 見積りであり、プロセス全体の使用量を保証する値ではない。
- 複数保存先を跨ぐ設定要求は全期待値を事前検査し、各保存先を CAS する。
  YAML の global ファイル・local ストア・replace 全体を 1 transaction にしたという意味ではない。
- 全件更新の複合ソートと UI の client-side 絞り込みに必要な全レコード取得は残す。
  ID だけ必要な箇所と選択処理・待機キューの全量読取を削減した。
- Worker の永続的作者追跡 CRUD / scheduler は追加しない。今回接続したのは既存の登録欄からの単発展開。
- 速度改善率は未計測。改善は往復・投影・batch・先読みの構造と動作の確認であり、benchmark の結果ではない。

### 実行確認

- native の全テストと real-binary DB-only 回帰、Worker wasm build、ローカル Worker の
  HTTP / Queue / WebSocket / D1 CAS、JS の消費者回帰を実行した。
- 実ネットワークで `mypage.syosetu.com/2842627/` と `ncode.syosetu.com/s3795b/` を作品ジョブへ展開。
  取り消し前に 2 話の 1 作品が DL と自動変換まで完了した。全作者作品の DL 完走を意味しない。
- 隔離した native CLI で DB-only convert（stored INI / replace の反映）、backup ZIP、
  設定焼き込み、clean、完全な YAML 復帰と編集済みサイト定義の復元を確認した。
- Firefox の実画面で未設定表示、未変更保存の POST なし、Worker 非対応操作、D1 表示を確認。
  Worker の実ストリーム EPUB は横書き / 縦書き、挿絵の有無、本文順、CSS のリンク、
  DMincho 326,420 bytes、行高 2.1em、表紙 viewport 120×90 を ZIP の実内容で確認した。
- テスト用データは Rust `TempDir` が所有する隔離領域。実ユーザープロファイル・資格情報・
  規定の実ライブラリは触らない。Windows の既定 TEMP の OS5 と MSVC の失敗は、
  リポジトリ内のテスト scratch と LLVM compiler/linker の環境指定で回避した。製品設定は変更しない。
- 本番 Worker / S3 への直接配備・実機 send、外部 supervisor の下での再起動子プロセスの生存は
  本検証の完了項目に含めない。ローカル配備と API の切替前後・再起動要否を確認した。

## 2026-10-12 再監査・シリーズ展開の共通化

取得・変換・作者追跡、設定・資格情報・保存、Web API・キュー、EPUB・SQL、リレー・配備の
5 領域を現行ソースで再調査した。以下が今回の判定であり、後続の初回監査と異なる場合はこちらを優先する。
シリーズ以外はソース比較による候補の確認で、障害の実機再現や修正まで済ませたという意味ではない。

### 今回実装した範囲

- `src/application/download_targets.rs` の `DownloadTargetExpander` が、実効 `SiteSetting`、
  `HttpClient`、`RateLimiter` を借りてシリーズ URL を展開する。サイト固有分岐は追加せず、
  `series_url` / `series_item_url`、ヘッダ・文字コード・取得間隔の既存規則を共有する。
- CLI の引数指定と対話入力、native Web の CLI 子プロセス、Worker の `/api/download`・
  `/api/download_force`・`/api/jobs`、Worker の保存済みジョブ・旧 envelope の実行を同じ処理へ接続した。
- Worker は全入力の展開成功後に作品ごとの台帳行を作る。ページ順・重複除去・元オプションの
  引き継ぎを維持し、取得失敗や空一覧を単体小説へフォールバックさせない。
- なろうのシリーズは「各小説を別レコードとして追加」、Pixiv の小説・漫画シリーズは
  従来どおり「シリーズ自体を 1 レコードとして追加」。後者を前者の展開へ巻き込まない。

### 残る共通化候補（今回未修正）

| 優先 | 対象・確認した重複／差 | 主な箇所 | 推奨する既存の受け皿 |
|---|---|---|---|
| 高 | 普通の native HTTP tier と relay が `Set-Cookie` を保持しない。更新ループ自体も native / Worker の二重実装 | `src/native/http.rs:165-209,422-505,928-937`、`worker_entry/src/http.rs:106-150,227-253`、`src/platform/relay.rs:201-263` | `platform::http` の応答契約、`platform::cookie_store`。relay の JS 実装も同時変更 |
| 高 | 真偽値の読み取りが `Bool` 限定／`true,false` 限定／`yes,on,1` 対応に分裂。Worker 内でも debug 設定の解釈が違う | `src/db/settings.rs:110-118`、`src/compat.rs:298-317`、`worker_entry/src/composition.rs:243-252`、`consumer.rs:498-508`、`webui/ui_prefs.rs:445-461` | `setting_core` に読み取り規則を置き、未設定と false を区別する |
| 高 | URL / ID / ncode / タイトルの解決が 5〜6 系統。URL fallback が異なり、Web の `SiteDefinitionProvider` は空実装 | `src/commands/mod.rs:46-87`、`update.rs:494-537`、`src/web/jobs.rs:398-439`、`worker_entry/src/webui/{download,job_actions,read_views}.rs` | `application::library` と既存 `SiteDefinitionProvider`。差を解決モードとして明示 |
| 高 | native のランタイムサイト定義が `effective()`、Worker が `effective_runtime()` を使用 | `src/native/site_definitions.rs:159-165`、`worker_entry/src/bundled_sites.rs:79-99` | `application::site_definitions` の version gate。古いユーザー定義への影響を確認して切替 |
| 高 | タグ色の保存で native は挿入順を保持、Worker は `HashMap` へ落とす。次の既定色の選択が順序に依存 | `src/tag_colors.rs:35-66`、`worker_entry/src/d1_repository.rs:608-611`、`src/application/tag_colors.rs:91-105` | `TagColors` の順序保持 codec。色の正規化も同所へ |
| 高 | Worker のタグ・凍結・削除・notepad・設定更新で native 相当の push 通知が欠ける | `src/web/{batch,novels,tags,misc,global_settings}.rs`、`worker_entry/src/webui/{tag_actions,row_actions,novels,read_views}.rs`、`global_settings.rs` | `application::push_events` の更新種別→イベント列。送信だけ各 runtime に残す |
| 高 | スケジューラの DST ギャップ、初回 catch-up、last-run 解釈が異なる。native が共有サービスを使わない | `src/web/scheduler.rs:223-296`、`src/application/scheduler.rs:180-189,236-260`、`worker_entry/src/scheduler.rs:201-209` | `SchedulerService`。narou.rb の外部仕様を確認し、保存形式だけ adapter に残す |
| 高 | EPUB オプションが 3 経路。Worker は横書き設定があっても `vertical: true`、source 経路では表紙・扉ページ設定の反映が異なる | `src/converter/device.rs:826-840`、`src/web/novels.rs:682-710`、`worker_entry/src/lib.rs:688-705`、`src/epub_lite.rs:439-557,586,681-701` | `epub_lite` の共通組立。設定と資産供給を分離する |
| 中 | 個別設定の JSON→INI パースと検証が約 170 行の写し | `src/web/novel_settings.rs:153-326`、`worker_entry/src/webui/settings.rs:221-392` | `application::novel_settings` に patch パーサを移す |
| 中 | ログイン一覧 payload、rename / order / clear の規則が CLI・native Web・Worker に分散 | `src/commands/login.rs:186-260`、`src/web/login.rs:114-320`、`worker_entry/src/{login,webui/login_actions}.rs` | `login` / `platform::cookie_store`。CLI の 1 始まり変換だけ外側へ |
| 中 | Cookie ストアの旧形式畳み・暗号化 map 操作・merge / replace / remove / clear が二重実装 | `src/native/cookie_store.rs:67-232`、`worker_entry/src/d1_cookie_store.rs:204-380` | `platform::cookie_store` の純粋な map codec。鍵取得と I/O は adapter |
| 中 | WHERE / 検索語 / sort / keyset の SQL 組立と状態 SQL が二重実装 | `src/native/sqlite/query.rs:32-170`、`worker_entry/src/d1_repository.rs:884-1005`、`src/native/sqlite/sql/status_*` | portable な `db` の SQL builder + `NovelBind`。ドライバの bind / transaction は別 |
| 中 | 更新確認後の `modified` 除去・日時・`end` タグ、自動 404 凍結が CLI と共有サービスで重複 | `src/commands/update.rs:701-763`、`src/compat.rs:529-560`、`src/application/novel_actions.rs:475-563` | 既存 `NovelActionService` を CLI でも消費する |
| 中 | 更新後の変換要否と `convert_failure` 操作が重複 | `src/commands/{download,update}.rs`、`worker_entry/src/{executor,convert}.rs` | `application::convert` の判定・状態変更。保存しない構成の差は引数化 |
| 中 | 正常結果の文言は共有済みだが、例外・中断・debug 詳細の dispatch は重複 | `src/commands/download.rs:158-176`、`update.rs:1829-1855`、`worker_entry/src/executor.rs` | `application::jobs` / `messages` のエラー行生成 |
| 中 | `tag:`・`^tag:`・bare tag 展開、update 入力正規化が重複 | `src/commands/download.rs:275-338`、`update.rs:418-471`、`src/web/jobs.rs:831-862`、`worker_entry/src/webui/job_actions.rs:322-383,616-647` | `application::webui`。repository 検索は port で渡す |
| 中 | DTO、上限、検索語結合、表示列→sort、タグ検証が複数箇所に残る | `src/web/state.rs:13-154`、`src/web/novels.rs:24-33,86-96`、`worker_entry/src/webui/{download,job_actions,list,novels,row_actions}.rs` | `web_payloads` / `webui`。値が同じだけの別用途の上限は統合しない |
| 中 | キュー種別・表示ラベルが独立実装で、Worker は元コマンドではなく ledger kind を表示 | `src/queue.rs:38-48`、`src/web/jobs.rs:134-237`、`worker_entry/src/webui/queue.rs:213-285` | `JobKind` と `messages::jobs` / `webui` |
| 中 | notepad の object ID・競合応答、taginfo / diff / author_comments の表示組立が重複 | `src/web/{misc,jobs,novels}.rs`、`worker_entry/src/webui/{read_views,novels}.rs` | `novel_content` / `webui`。CAS と HTTP 応答の送信は adapter |
| 中 | feature tour のテーブルと 3 要素 version 比較がコピーされ、既存の共有比較を使わない | `src/web/feature_tour.rs:20-142,281-337`、`worker_entry/src/webui/ui_prefs.rs:198-320,462-520` | 共通のツアーデータと `application::version_compare` |
| 中 | `convert.keep-txt` の env / YAML 解釈が 3 実装 | `src/converter/mod.rs:169-182`、`src/application/convert.rs:247-277`、`worker_entry/src/convert.rs:211-243` | `setting_core` / `application::convert`。native=true / Worker=false の既定は保持 |
| 中 | HTTP ヘッダ名・値の安全述語が複数系統、socket の redirect は各 hop の構文検証を通らない | `src/downloader/http_policy.rs:110-117`、`security.rs:13-15`、`src/platform/{relay,http1}.rs`、`worker_entry/src/http.rs:157-207` | 無条件コンパイルの `platform` のヘッダ規則と `url_policy` |
| 中 | 同じ relay protocol の Node curl 版と Worker 版で follow が異なる | `scripts/sorahost-proxy/server.mjs:75-108`、`worker/worker.mjs:74-87` | `platform::relay` の契約と共通 redirect 方針。`-L` 追加だけではヘッダ解析が壊れる |
| 中 | レート制限の idle reset 基準・閾値・pending 条件が異なる | `src/downloader/rate_limit.rs:120-150`、`worker_entry/src/site_rate_limiter.rs:53-67` | `platform::rate_limiter` の時系列状態機械。時計と permit 保存だけ別 |
| 中 | 在庫キー、import / export の管理対象名が別テーブル。Cookie 鍵長など公開済み定数も再宣言 | `src/native/sqlite/{state,export_yaml}.rs`、`src/db/inventory.rs`、`worker_entry/src/{d1_cookie_store,composition}.rs` | 用途を保った共通定数・管理対象表。import と rollback export の欠落を防ぐ |
| 低 | MIME の WebP 対応と 64 桁 hash 判定が複数実装 | `src/native/object_store.rs:252-265`、`src/platform/object_store.rs:84-96`、`s3_request.rs:72-78`、`src/illustration_store.rs:704`、`src/commands/illust.rs:238-241` | `platform::object_store` / 挿絵キーの共通述語 |
| 要判断 | 凍結判定は Ruby 互換の inventory-only と Web の store-or-tag が違う。状態文字列も複数実装 | `src/compat.rs:492-496`、`src/commands/manage.rs:506-513`、`src/application/library.rs:472-476`、`src/platform/repository.rs:328-355`、`src/db/{sort,database}.rs` | モードを明示して共有する。単に OR 判定へ統一すると narou.rb 互換を変える |
| 要判断 | native EPUB は全量 `Vec<u8>`、Worker は共有 stream writer。`ChunkSink` は `Rc/RefCell` で `!Send` | `src/web/novels.rs:708-710`、`src/epub_lite.rs:35,734-775`、`worker_entry/src/lib.rs:727-819` | entry writer は再利用可。Axum 用の Send sink / channel は別途必要 |
| 要判断 | portable とされるなろう API helper が native gate に閉じ、batch helper に実利用がない | `src/downloader/mod.rs:3-4,2997-3000`、`src/downloader/narou_api.rs:110-235`、`src/commands/update.rs` | 実際に使う API parse / classify を共有するか、未使用 helper を除去する。Worker 機能追加は別判断 |
| 低 | 配備用 curl 等の pin・manifest と develop / production env が重複 | `.github/workflows/{platform,deploy-sorahost}.yml`、`sorahost/start.sh:77-78`、`scripts/sorahost-proxy/{deploy.ps1,sorahost.json}` | 配備資産の単一表・描画器。GitHub secrets の job 別指定は保持する |

### 初回監査の訂正・意図的な差

- SELECT / UPSERT とレコード codec は現在 `src/db/novel_codec.rs` を両 DB driver が使う。
  初回 §2.3 の全面的な未共有という読み方は誤り。残る候補は WHERE / term / sort と状態 SQL 等。
- 初回 A-8 の「踏み台が redirect を辿る」は Node curl 版では成立しない。
  `server.mjs` には `-L` がなく、`--max-redirs` だけでは follow しない。Worker relay 版は follow する。
- SORAHOST の `.env` 読み込み、cwd によるライブラリ指定、loopback 前提の起動認証、
  `/dev/null` による非対話、native の health / relay / S3 probe 未採用は、
  対応する要求がない限り共通化のためだけに機能追加しない。
- D1 / SQLite / YAML の I/O、transaction / batch、OS 子プロセス、Worker budget / lease / DO / TTL、
  raw 非保存・keep-txt の既定差、HTTP 応答上限、native 限定 illustration dedup は意図的な runtime 差として残す。
- source と既存ドキュメントにもずれがある。現行 `sorahost/start.sh` と配備 workflow には
  cloudflared / connector の処理が残り、native Web に 501 が実装されているという README の記述は確認できない。
  本作業はそれらを撤去・実装する変更ではない。

### 今回の検証

- `cargo test -p narou_rs --test application_services`: 6 件成功。順序・重複・HTML entity、
  YAML 差し替え、force 引き継ぎ、update 非展開、取得失敗・空一覧を確認。
- `worker-build`: Worker の wasm release build 成功。
- `cargo check -p narou_worker --target wasm32-unknown-unknown`: 成功。
  Windows のビルド検証は VS の開発環境とインストール済み `clang-cl` / LLVM を指定した。
  MSVC の C build が `D8050` で止まるローカル環境を回避するためで、依存や本番設定は変更していない。
- 実ページ `https://ncode.syosetu.com/s3795b/` は `n7826bd` / `n9364bq` の 2 URL へ展開された。
  CLI の対話入力も受理し、各作品の DL 開始まで確認。検証用ライブラリで本文保存時に
  `IO error: アクセスが拒否されました (os error 5)` となったため、CLI の DL 完走は未検証。
- ローカル Worker の Web UI から同 URL を入力し、HTTP 200 / `success:true` と個別 2 ジョブを確認。
  `/api/jobs` と `/api/download_force` も同じ 2 URL を登録し、force 等のオプションを保持した。
- 旧状態を模した保存済みシリーズジョブを cron で再投入し、親 `succeeded`、
  WebSocket の `queue_complete`、正しい 2 子ジョブ、共通の展開通知を確認した。
- `[有効 ID, 404 のシリーズ URL]` の登録は `success:false` / `results:[]` となり、
  待機数は 0→0。展開エラーより前の対象も登録されない。
- Worker の検証ジョブは登録・fan-out を観測した後に取り消した。シリーズ全作品の DL 完走・EPUB 比較、
  および監査候補の挙動差の再現試験は今回の検証結果に含めない。

## 0. 結論

**共通化できるのにできていない経路は広範囲に存在する。** 大きく 3 種類ある。

| 種類 | 件数の目安 | 代表 |
|---|---|---|
| **A. 実害が出ている / 出うる** | 9 | native の Set-Cookie 書き戻しが構造的に無効、サイト定義 version gate の非対称、スケジューラ DST 判定差、設定値 coercion 差 |
| **B. 同一ロジックの二重実装（片側修正漏れ）** | 30+ | feature tour テーブル 122 行の完全コピー、INI 値パース約 175 行、Cookie ストアのオーケストレーション、NovelRepository の SQL 組立、ターゲット解決 5〜6 実装 |
| **C. 片側にしかない汎用機能** | 10+ | リレー段（core にあるのに Worker 専用）、`SchedulerService`（native 未使用）、`push_events`（native のジョブループ未使用）、`EpubStreamWriter`（Worker のみ）、SORAHOST の起動設定・capability 表明 |

設計方針（`docs/platform-abstraction.md`）と `src/application/**` の共有層は既に十分な
受け皿になっている。問題は「受け皿があるのに、そこへ寄せずに環境側へ書いた」経路が
後から積み上がったこと。**新しい共有層を作る必要はほとんどなく、既存の関数を呼ぶ置換が大半**。

---

## 1. A: 実害が出ている / 出うる経路

### A-1. native の Set-Cookie 書き戻しが構造的に動かない ★最重要

- 書き戻しロジックは native にもある: `src/native/http.rs:160-209`（`persist_set_cookie`）を
  `NativeHttpClient::send`（`:928-939`）が毎回呼ぶ。
- しかし native のサイト取得 tier は **Content-Type しか返さない**:
  - curl tier: `:434-441` が `Content-Type:` だけ拾い、`:474-478` で
    `headers: content_type_header(content_type)` を返す
  - reqwest tier: `:490` / `:501` も `content_type_header(...)` のみ
  - subprocess tier: `:636` / `:676` は `headers: Vec::new()`
- したがって `:172` の `filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))` は常に空。
- Worker 側は fetch 経路で全ヘッダを返す（`worker_entry/src/http.rs:304`）ので書き戻しが動く。
- 影響: **ログイン必須サイトのセッションが native では更新されない**（Worker では更新される）。
  `AGENTS.md` の「応答の `Set-Cookie` は…`src/native/http.rs` が書き戻してセッションを維持する」
  という記述と実装が一致していない。
- 受け皿: 「site fetch でも保持するヘッダ集合」を `src/platform/http.rs` 側の規則にし、
  native の 3 tier がそれに従う。`HttpRequest::trusted_endpoint` の doc
  （`src/platform/http.rs:57-65`）が既に「site fetch は部分集合」と定義しているので、
  その部分集合に `Set-Cookie` を足すのが最小修正。
- テストが無い: リポジトリ全体で set-cookie を検証するテストは
  `worker_entry/src/lib.rs:1698`（Worker の認証 Cookie）のみ。

### A-2. サイト定義の version gate が native のダウンローダ経路だけ掛からない

- 共有層は 2 つを意図的に分けている: `effective()`（gate 無し＝管理/表示用、`/api/sites` と一致）と
  `effective_runtime()`（version gate 付き＝fetch policy / ダウンローダ用）。
  doc は「fetch policy に渡す実効定義は `effective_runtime()` を使う」と明記:
  `src/application/site_definitions.rs:262-290`。
- Worker は従う: `worker_entry/src/bundled_sites.rs:79,97` が `effective_runtime()`。
- native の**ランタイム**スナップショットは `effective()`（gate 無し）を使っている:
  `src/native/site_definitions.rs:159`。`/api/sites` の管理表示が `effective()` なのは設計どおりだが、
  ここはダウンローダへ渡す実効定義なので共有層の指定に反する。
  ここで確定したスナップショットを downloader/CLI が読む（`src/downloader/mod.rs:888`、
  `src/commands/mod.rs:58`、`src/commands/update.rs:329,1148`）。
- native 内でも不整合: `SiteSetting::load_all()`（`src/downloader/site_setting/mod.rs:417-436` →
  `loader.rs:34-48`）は `merge_user_definition_yaml`（`mod.rs:388-414`）で gate を掛けており、
  `src/platform/cookie_store.rs:282` や `src/commands/author.rs:57` はこちらを使う。
- 影響: 同梱版より `version` が低いユーザー `webnovel/*.yaml` が、native の DL では有効 /
  Worker では無視、という外部観測差。
- 修正は 1 行（`effective()` → `effective_runtime()`）。ただし既存 native ライブラリで
  「古い version のユーザー定義が効かなくなる」挙動変化になる点は要判断。

### A-3. 自動更新スケジューラが native と Worker で別判定（DST・catch-up・last-run）

native は共有 `SchedulerService` を**一切使っていない**（`src/web/scheduler.rs:12` の import に無い）。
`Schedule::parse` だけ共有（`:170-172`）。Worker は共有層を使う（`worker_entry/src/scheduler.rs:201-209`）。

| 論点 | native | 共有層（Worker が使用） |
|---|---|---|
| DST ギャップ | 存在しない時刻を **+1 分ずつ繰り上げ**（`src/web/scheduler.rs:239-250`、テスト名 `uses_rounded_dst_gap`） | その候補を**捨てる**（`src/application/scheduler.rs:232-237`） |
| last-run 未記録時 | catch-up しない（`:259-262`） | 当日時刻を過ぎていれば実行（`:183`） |
| last-run 保存 | local 設定 `update.auto-schedule.last-run` に epoch 秒（`:25,276-294`） | D1 checkpoint に RFC3339（`src/application/jobs.rs:1027,1173-1184`） |
| 重複防止 | queue 走査（`:468-503`） | 世代 + プランナリース CAS（`src/application/jobs.rs:1106-1155`） |

影響: `update.auto-schedule=0230` + `timezone=America/New_York` の DST 当日で
native=03:00 / Worker=翌日 02:30。初回起動直後の catch-up も逆。
`src/application/mod.rs:119` で `scheduler` は両 root で構築済み（`src/web/mod.rs:125`）なのに
native 側の消費者が存在しない。

### A-4. 設定値の coercion が Worker だけ別規則（`yes` / `on` / `1` が偽になる）

- Worker: `worker_entry/src/composition.rs:243-275`
  `setting_bool` は `"true"` / `"false"` のみ受理（`:246-250`）。
- native: `src/compat.rs:309-318` は `true|yes|on|1` を真、Number は非 0 を真。
  `src/db/settings.rs:110-121` は `false|no|off|0` も許容。
- 影響: `update.strong: yes` / `guard-spoiler: 1` / `download.use-subdirectory: on` は
  native では有効、Worker では未設定扱い。**D1 に既に `yes` で入っている値が統合後に
  意味を持つ**ため、native 側の規則へ寄せるのが互換的。
- 数値の解釈にも差: Worker の `setting_f64`（`:260`）は `trim()` するが、native の
  `src/downloader/rate_limit.rs:177` / `src/commands/update.rs:1067` はしない。
  `download.interval: " 0.5 "` で挙動が分かれる。
- 二重化の原因: `src/db/settings.rs` は `src/db/mod.rs:7-8` で native 限定。
  置き場は `setting_core`（両ビルドで使われている）。

### A-5. `webui.debug-mode` の解釈が 3 箇所で異なり、Worker の detail が黙って出ない

- native: `src/compat.rs:309-318`（trim も lowercase もしない）
- 共有: `src/application/debug.rs:61-77`（`truthy()` は trim + lowercase）
- Worker: `worker_entry/src/consumer.rs:498-508` は `.as_bool()` ＝ **YAML の真偽値のみ**。
  設定が文字列 `"true"` や `1` だと false。
- 影響: Worker の `queue_failed.data.detail` が設定どおりに有効化されない（デバッグ不能）。

### A-6. Worker の UI 操作が `table.reload` / `tag.updateCanvas` を送らない（コメントの前提が誤り）

- `worker_entry/src/webui/tag_actions.rs:16-18` と `row_actions.rs:22-25` は
  「Worker に broadcast 経路が無い」と書くが、事実に反する。`PushHubClient` +
  `push_events::table_reload()` は Worker 内にあり、`webui/job_actions.rs:922-929`、
  `queue_actions.rs:90`、`push_hub.rs:113-114` で実使用中。
- 影響: Worker でタグ編集・凍結・削除をしたとき、他タブ/他クライアントの一覧と
  タグキャンバスが更新されない（native は更新される）。
- 共有層の追加は不要。既存クライアントを呼ぶだけ。

### A-7. リレー経路だけ Set-Cookie を落とす

- リレーの応答はプロトコル上 `{status, contentType, location, via, body}` しか運ばず、
  `worker_entry/src/http.rs:242-248` が `contentType` / `location` だけを組み立てる
  （`src/platform/relay.rs:200-213`）。
- socket 経路は全ヘッダを保持する（`src/platform/http1.rs:192`）ので、
  **リレーに落ちたサイトだけ**ログイン Cookie 更新が起きない。
- 修正は `platform/relay.rs` / `scripts/sorahost-proxy/server.mjs` /
  `scripts/sorahost-proxy/worker/worker.mjs` の 3 実装同時変更。

### A-8. Worker の socket / relay 経路にリダイレクト先の SSRF 再検証が無い

- native は毎ホップ検証する: `src/native/http.rs:993-1000`、`:727`、
  `src/downloader/http_policy.rs:233`。
- Worker の socket ホップは再検証なし（`worker_entry/src/http.rs:182-197`。
  `SocketTarget::parse` は scheme/host しか見ない: `src/platform/http1.rs:42-50`）。
- リレーは踏み台が辿る。踏み台側の検査は `if (!/^https?:\/\//.test(target))`
  （`scripts/sorahost-proxy/server.mjs:174`）と `--max-redirs 5`（`:84`）のみ。
- Workers の `connect()` は private 宛で失敗するため実害は限定的だが、リレー経由は
  踏み台ネットワークからの到達になる。

### A-9. レート制限ペーシングの idle reset 条件が両実装で違う

- native: `src/downloader/rate_limit.rs:127-150`（基準 `last_download`、閾値は
  **スコープ外の** `self.max_steps_wait_time`、条件に `no_pending_slot` を含む）
- Worker DO: `worker_entry/src/site_rate_limiter.rs:65-67`（基準 `next_allowed_at_ms`、
  閾値は**スコープ値**）
- 加えて native は `Instant`（monotonic）、DO は `js_sys::Date::now()`（wall clock）。
- `min_interval: 30` 級のサイトでリセット窓が 5s vs 30s に分かれる。

---

## 2. B: 同一ロジックの二重実装（片側修正漏れ）

コード内コメントが「写し」「揃えること」と自認しているものが多い。**diff で完全一致を実測**
したものは ★ 付き。

### 2.1 Web UI / API 層

| 対象 | native | worker | 備考 |
|---|---|---|---|
| feature tour | `src/web/feature_tour.rs:20-142`, `:281-337` | `webui/ui_prefs.rs:198-320`, `:462-519` | ★ テーブル 122 行が完全一致。`ui_prefs.rs:196-197` に「native 側に項目を足したらこちらも揃えること」 |
| INI 値パース | `src/web/novel_settings.rs:153-326` | `webui/settings.rs:221-392` | ★ 差はインデントのみ（14 行）。日本語エラー文まで一致 |
| 一覧のソート列対応 / 検索語結合 | `src/web/novels.rs:86-96`, `:24-33` | `webui/list.rs:91-101`, `:165-176` | match が完全一致 |
| taginfo HTML | `src/web/jobs.rs:2182-2257` | `webui/read_views.rs:908-969` | 集計・HTML・キー一致 |
| author_comments | `src/web/novels.rs:291-382` | `webui/novels.rs:70-161` | ratio 計算式まで一致 |
| diff 一覧 HTML / diff_list / diff_clean | `src/web/jobs.rs:612-672,1404-1442,1556-1607` | `webui/read_views.rs:451-524,530-568,574-622` | 文言一致 |
| notepad 楽観ロック | `src/web/misc.rs:334-395` | `webui/read_views.rs:780-877` | object_id/競合応答が一致（CAS の実装差は意図的） |
| キュー表示文言 | `src/web/jobs.rs:134-156,194-200,959` | `webui/queue.rs:231-285,374` | ★ `describe_update_targets` 等が完全一致 |
| get_pending_tasks JSON | `src/web/jobs.rs:1965-2006` | `webui/queue.rs:405-428` | キー一致 |
| ジョブ投入検証 | `src/web/jobs.rs:48-57,67-74,268-293,831-862` | `webui/download.rs:182-191`, `webui/job_actions.rs:214-246,616-647,973-980` | ★ `normalize_update_targets` は逐語一致 |
| `expand_tag_targets` | `src/commands/update.rs:425-492` | `webui/job_actions.rs:327-384` | worker 側が「写し」と自認 |
| edit_tag / tag_change_color / tag_list | `src/web/tags.rs:186-259`, `misc.rs:199-276` | `webui/tag_actions.rs:162-270`, `webui/queue.rs:102-158` | 生 HTML 文字列まで一致 |
| freeze/unfreeze/remove 応答文 | `src/web/novels.rs:204-289` | `webui/novels.rs:167-245` | `"(file deletion incomplete)"` 等 |
| キュー操作文言 | `src/web/jobs.rs:1906-2034` | `webui/queue_actions.rs:112-244` | 「キャンセルしました」等 |
| ログイン API | `src/web/login.rs:173-190,269-321` | `webui/login_actions.rs:220-237`, `worker_entry/src/login.rs:16-75` | reorder 検証・status payload が一致 |
| `version_is_newer` | `src/web/misc.rs:96-104` | `webui/read_views.rs:1022-1030` | 共有 `version_compare` を呼ぶ 5 行 |
| `TRANSPARENT_GIF` | `src/web/jobs.rs:30-33` | `worker_entry/src/lib.rs:1406-1409` | バイト列が完全一致 |
| Content-Disposition サニタイズ | `src/web/novels.rs:493-505` | `worker_entry/src/lib.rs:870-881` | worker 側が「同じ規則」と自認 |
| リクエスト DTO 16 種 | `src/web/state.rs:13-154`, `web/update.rs:25-33`, `web/login.rs:21-55` | `webui/*.rs`, `lib.rs` の各所 | `ListParams` だけ `application::web_payloads` に移設済み。`BatchIdsBody` は worker 内 3 箇所に再宣言 |
| アセット版数付与 | `src/web/frontend.rs:10-108`（要求時 Rust） | `worker_entry/build_assets.mjs:41-85,113`（ビルド時 Node） | 同一アルゴリズムの 2 言語実装 |

### 2.2 ターゲット解決（5〜6 実装）

順序（alias → 数値 id → URL → ncode → タイトル→ncode）は同一だが、**URL 分岐が違う**。

| 実装 | toc_url フォールバック |
|---|---|
| `src/commands/mod.rs:46-86`（CLI 共通） | あり（`unwrap_or_else(\|\| setting.toc_url())`） |
| `src/commands/download.rs:458-470` | あり |
| `src/commands/update.rs:494-529` | **無し**（`toc_url_with_url_captures(target)?`） |
| `worker_entry/src/webui/job_actions.rs:268-319` | あり |
| `worker_entry/src/webui/job_actions.rs:389-434` | 無し |
| `worker_entry/src/webui/read_views.rs:261-303` | `site_definitions.resolve_toc_url` 経由 + `target` 文字列フォールバック |

`src/web/jobs.rs:398-439`（`resolve_existing_id_for_target_with_library`）も同系統で、
native 側は `EmptySiteDefinitionProvider` を踏むため URL が常に解決不能。

### 2.3 保存層

| 対象 | native | worker | 備考 |
|---|---|---|---|
| NovelRepository の WHERE / term / sort | `src/native/sqlite/query.rs:32,119,135,144` | `worker_entry/src/d1_repository.rs:884,961,980,989` | native 側冒頭コメントが「ported verbatim from worker_entry/src/d1_repository.rs」と自認 |
| query/scan_ids/allocate_id/apply_batch | `src/native/sqlite/repository.rs:99-116,248-283,297-310` | `d1_repository.rs:138-197,220-246` | ORDER BY / keyset / シーケンスが同型 |
| 状態検索・ソート SQL | `src/native/sqlite/sql/*.sql` | `d1_repository.rs:973,975` の文字列リテラル | ★ バイト一致を実測。sort 用は native の .sql と別リテラル |
| 状態テキスト規則 | `src/db/database.rs:564-576` / `src/db/sort.rs:68-80`（同一本文の 2 ファイル、`src/db/mod.rs:15-18` でビルド切替） | `src/platform/repository.rs:340-355` など計 5〜6 箇所 | 同一性を検査するテストが無い |
| Cookie ストアのオーケストレーション | `src/native/cookie_store.rs:67-240` | `worker_entry/src/d1_cookie_store.rs:204-378` | ★ `merge_groups` / `replace_groups` / `remove` / `clear_all` が逐語一致（async 差のみ） |
| Set-Cookie 書き戻しループ | `src/native/http.rs:165-209` | `worker_entry/src/http.rs:106-155` | 原子関数（`host_for_sent` / `apply_set_cookie`）は共有済みで、ループだけ 2 実装 |
| マイグレーション SQL | `src/native/sqlite/migrations/*.sql`（12） | `worker_entry/migrations/*.sql`（10） | ★ 8 ファイルがバイト一致。`tests/sqlite_migration_upgrade.rs:33-92` が対応表で固定。0005/0006 は**同番号で別意味** |
| `login_session` の NULL 表現 | `src/native/sqlite/record_map.rs:52-55`（`""`） | `d1_repository.rs:711`（NULL） | 相互移行経路が無いため現状は無害 |
| `extra_fields` 上限 | `src/db/novel_codec.rs:108` = 64 KiB | `d1_repository.rs:25` = 1.9 MB | 同一レコードで成否が分かれる |
| Content-Type 表 | `src/native/object_store.rs:260-273`（webp あり） | `src/platform/object_store.rs:84-96`（webp なし、D1/S3 が使用） | `.webp` 挿絵で MIME が違う |
| 挿絵論理キー | `src/platform/object_store.rs:366-369`（`sanitize_key_component` 経由） | `src/illustration_store.rs:177`（`join` 直） | sanitize が片側のみ |
| 挿絵 dedup | `src/native/s3.rs:78`（sqlite+s3 で有効） | `worker_entry/src/s3_store.rs:33`（固定 false） | 仕組み自体は `src/platform/s3_store.rs:44` で共有 |

### 2.4 変換 / EPUB / ジョブ

| 対象 | 場所 | 備考 |
|---|---|---|
| `convert.keep-txt` の解釈 | `src/converter/mod.rs:176-182` / `src/application/convert.rs:258-277` / `worker_entry/src/convert.rs:197-213` | **3 実装**。env パースは同一、既定値が native=true / Worker=false（設計意図）。env パースだけ共有可能 |
| EPUB 組立オプション | `src/web/novels.rs:682-699` / `worker_entry/src/lib.rs:634-652` / `src/converter/device.rs:826` | **3 実装**。Worker は `vertical: true` 固定（native は `!settings.enable_yokogaki`）、`cover_from_first_image` の判定も別、`assets_dir`/`extra_assets` 無し |
| EPUB の 409 条件・文言 | `src/web/novels.rs:666-673` | worker `lib.rs:518-523` は `"Convert failed: {error}"` |
| 自動変換の判定 | `src/commands/update.rs:1753-1789` + `src/commands/download.rs:608-612` | worker `executor.rs:495-579` に再実装（`--no-convert` / `--convert-only-new-arrival` / `convert_failure`） |
| `JobType` と `JobKind` | `src/queue.rs:38-48` / `src/application/jobs.rs:26-62` | 並列 enum + `src/web/jobs.rs:214-224` に 3 つ目の文字列表 |
| `history_replayable` | `src/web/push.rs:619-630` | worker `push_hub.rs:57-68` が逐語コピー |
| `push_events` の native 側未使用 | `src/application/push_events.rs:154-216` | native のジョブループ（`src/web/worker.rs:189-247,317-325`）と `src/progress.rs:269-336` が `serde_json::json!` で手組み。共有ビルダーを 1 つも呼んでいない |
| 定数の二重定義 | `0.7s` / `5s`: `src/downloader/rate_limit.rs:7-8` ↔ `src/platform/rate_limiter.rs:121-124`、`2.5s`: `src/commands/update.rs:39` ↔ `worker_entry/src/composition.rs:53`、`"__unknown__"`: `src/commands/update.rs:47` ↔ `worker_entry/src/executor.rs:354`、16 MiB: `worker_entry/src/d1_object_store.rs:30` / `worker_entry/src/lib.rs:573` ↔ 共有 `platform::SMALL_CAP`（`src/platform/s3_store.rs:27`、`src/platform/mod.rs:59` で export 済み） | |
| `INVENTORY_NAME = "login_cookie"` | `src/native/cookie_store.rs:24` ↔ `worker_entry/src/d1_cookie_store.rs:30` | 共有 `platform::cookie_store` に置ける |

---

## 3. C: 片側にしかない汎用機能（もう片方が使えるのに使っていない）

| 機能 | 実装場所 | 使っていない側 | 備考 |
|---|---|---|---|
| 取得リレー段 | `src/platform/relay.rs`（core、無条件コンパイル）+ `worker_entry/src/http.rs:213-254` | native | `platform::relay` / `platform::http1` を import するのは `worker_entry/src/http.rs:13-14` のみ。`docs/platform-abstraction.md:531` も「native は未使用」と記載。native も SORAHOST で常駐するため、踏み台 IP から取りたい需要は成立しうる。既定 off の opt-in が妥当 |
| `RelayRequest::post_body` | `src/platform/relay.rs:194-197` | 全経路 | 呼び出し元はテストのみ |
| `SchedulerService` | `src/application/scheduler.rs` | native（A-3 参照） | 両 root で構築済み |
| `push_events` のビルダー群 | `src/application/push_events.rs` | native のジョブループ / `WebProgress` | 環境非依存の純関数 |
| `SiteDefinitionProvider` port | `src/application/events.rs:77-95` | **両方**（native `src/web/mod.rs:138` / worker `worker_entry/src/composition.rs:906` が `EmptySiteDefinitionProvider`） | 実体 `SiteDefinitions` は各 root が都度組み立てる。`docs/cloudflare-workers-migration-plan.md:270-271` が「撤去予定」と記載済み |
| `EpubStreamWriter` / `ChunkSink` | `src/epub_lite.rs:35,750-775` | native | Worker は 1 エントリずつ流す（`worker_entry/src/lib.rs:673-767`）。native は `src/web/novels.rs:708-710` で `Vec<u8>` に全量を溜めてから返す（大きい作品でメモリ不利） |
| なろう API 一括更新 | `src/downloader/narou_api.rs:111` / `src/downloader/mod.rs:2847`（`HttpClient` + `NovelRepository` だけに依存） | Worker | Worker の update は per-novel DL のみ。`SiteUpdateCapabilityProvider` も worker では未使用 |
| native の既定リクエストヘッダ | `src/native/http.rs:1113-1132`（Accept / Accept-Language / Accept-Encoding / Connection） | Worker | 同梱サイト定義で `headers:` を持つのは 2/7 件。Worker は UA しか送らない |
| `LibrarySortColumn` の表現力 | `src/application/library.rs:34-45` | **両方** | `SORT_COLUMN_KEYS` / `SORT_COLUMN_LABELS` は 14 列だが、列 8/11/12/13（tags/status/toc_url/new_arrivals_date）が両側とも Id に落ちる |
| site 定義の実効キャッシュ | native: `install_effective_site_settings` + `src/commands/web.rs:199-211` の 30 秒ポーリング | worker: `worker_entry/src/bundled_sites.rs:104-128` の 30 秒 TTL | **同じ目的の別機構**。native 側のコメントが「worker_entry::isolate_cache の TTL と同じ粒度」と相互参照。`worker_entry/src/webui/job_actions.rs:248-250` は「native の `effective_site_settings` 相当」と自認 |
| `FlushQueue` / permit バッチ / TTL キャッシュ | worker | native | 環境差として妥当（native は OS パイプと同期実行） |

### 3.1 SORAHOST（native 常駐）側の専用経路

| 論点 | 場所 | 内容 |
|---|---|---|
| 起動設定を毎回 shell から `narou setting` | `sorahost/start.sh:203-240` | `server-bind` / `server-ws-port=0` / `server-reverse-proxy.enable` / `server-add-accepted-hosts` / `convert.section-cache=false` を毎起動書き込む。`NAROU_RS_SECTION_CACHE`（`src/converter/mod.rs:191`）という env 経路が既にあるのに shell は setting を書いており、`sorahost/narou.env.example:42` と二重。`--port`（`start.sh:312`）と `server-port`（`:218`）も同じ値 |
| 「公開されるのに認証なし」ガードが shell にしかない | `sorahost/start.sh:171-200` | native の判定は bind アドレス基準（`src/commands/web.rs:555-557`）で 127.0.0.1 束縛では発火しない。一時パスワード生成を shell が実装し、native 仕様（user と password の両方が要る）をコメントで写経（`:196-197` ↔ `src/web/server_security.rs:105-110`） |
| 非対話モードが `< /dev/null` | `sorahost/start.sh:25-26,146,312` | native は `stdin().is_terminal()` のみ（23 箇所）。`--yes` / `NAROU_NONINTERACTIVE` が無い |
| ライブラリ位置・.env を CLI で指定できない | `sorahost/start.sh:34-41,43-68,70-71,139-142` | `NAROU_RS_LIBRARY` / `NAROU_RS_ENV_FILE` 等を Rust 側で読む箇所は **0 件**。`--root` / `--env-file` が無い |
| 「この環境では効かない機能」の表明が native に無い | worker: `worker_entry/src/webui/native_only.rs:27-107`（501 + 文言） / `worker_entry/src/global_settings.rs:135-206`（約 50 項目の無効設定リスト） | native には相当が無い（`src/web/` に 501 は 0 件）。`sorahost/README.md §11` の「Web UI 側では 501 相当」は**文書だけ**。`JobKind::is_worker_executable`（`src/application/jobs.rs:69-73`）と合わせ、同じ capability 知識が 4 箇所に分散 |
| `/health` が native に無い | リレー: `scripts/sorahost-proxy/server.mjs:134-136` / Worker: `/health/live` `/health/ready` | SORAHOST の smoke は `/` のステータスだけ（`.github/workflows/deploy-sorahost.yml:179`） |
| S3 必須ゲートが SORAHOST に無い | Worker: `wrangler.*.toml`, `platform.yml:245,420`, `ci/render_config.py:240` | native の `/api/storage/mode` は `mode` しか返さず `probe` 非対応（`src/web/storage.rs:21,29,34`） |
| ピン・マニフェストの二重管理 | `sorahost/start.sh:77-79` ↔ `.github/workflows/deploy-sorahost.yml:61-65` ↔ `scripts/sorahost-proxy/deploy.ps1:14` | cloudflared の tag/SHA、static curl の版、CA の入手元が 2〜3 箇所。`sorahost.json` も 3 箇所で別内容（`sorahost/README.md:250` の記載は実物と不一致） |
| develop / production の env ブロック | `.github/workflows/platform.yml:207-247` と `:388-421` | 20 行以上が同一コピー |
| start.sh を検証するテストが無い | `tests/test_sorahost_smoke.py:16-25` | workflow の smoke ステップだけを `bash -n` する。`start.sh` 本体・env 一覧と README/ひな形の一致は機械検証されていない |

---

## 4. 環境差として妥当（切り分けの記録）

以下は**共通化しない**のが正しい。混同しないよう記録する。

- 実行基盤: Worker の budget / checkpoint（`worker_entry/src/budget.rs`）、queue ledger + lease
  （`worker_entry/src/ledger.rs`、`src/application/jobs.rs:1009-1185` の `SchedulerCheckpoint`）、
  Durable Object による per-site permit、`FlushQueue` による送信間引き、isolate TTL キャッシュ。
- 保存方針: Worker は raw 非保存・EPUB 非保存・本文は objects（YAML blob）。native は FS/SQLite
  ミラー。※ただし `AGENTS.md:354` の「セクション行と列へ展開」と実装は不一致で、
  `content_backend` はコードに存在しない（ドキュメントのみ）。
- 提供しない機能の 501 / blocked: send / mail / MOBI / 外部 AozoraEpub3 / folder / browser /
  self-update / `narou db` 保守 / バージョン履歴系。
- push 履歴容量（native 10000 / リプレイ 1000 行・1 MB ↔ worker 200 件）、
  `queue_status` の語彙差、`key_source` の語彙差、`__NAROU_RS_WEBUI_BUILD__` のキー差。
  ※リプレイ対象種別（`history_replayable`）と target_console/scope フィルタは環境非依存なので共有候補。
- `webui_config` の port / ws_port、feature tour のバージョン源、`storage_migration` の状態。

---

## 5. 共通化の受け皿（どこへ寄せるか）

新規モジュールはほぼ不要。既存の置き場で足りる。

| 寄せ先 | 対象 |
|---|---|
| `src/application/webui.rs`（既に `validate_web_target_value` / `html_escape` / `sort_ids_from_records` / `targets_to_strings` がある） | キュー表示文言、notepad object_id/競合応答、taginfo、diff HTML、ログイン payload、feature tour ヘルパ、上限定数 |
| `src/application/web_payloads.rs`（`ListParams` の前例） | 残り 16 種類のリクエスト DTO |
| `src/application/feature_tour.rs`（新設 or `webui`） | `FEATURE_TOURS` テーブル + バージョン比較 |
| `src/application/novel_settings.rs`（既存） | INI 値パース（`parse_*`）と未知設定名検証 |
| `src/application/novel_content.rs` | author_comments の組み立て |
| `src/application/convert.rs` / `src/epub_lite.rs` | `keep-txt` の env 解釈、`EpubBuildOptions` の決定、EPUB チャンクストリーム生成 |
| `src/application/jobs.rs` | 自動変換の要否判定、ジョブ検証、capability（`JobKind` と「効かない設定」の一本化） |
| `src/application/scheduler.rs` | DST 解決・catch-up・last-run 形式（native の `SchedulerService` 採用） |
| `src/application/site_definitions.rs` | `effective_runtime()` へ統一、マージ鍵（ファイル名 or `name`）の統一 |
| `src/platform/rate_limiter.rs`（`DownloadPacing` の前例） | ペーシング状態機械の純関数（native は count=1、DO は count=N） |
| `src/platform/cookie_store.rs`（原子関数は既に共有） | Set-Cookie 書き戻しループ、map 単位のロード/セーブ port、map 操作（merge/replace/remove/clear） |
| `src/platform/http.rs` / `src/platform/http1.rs` / `src/platform/relay.rs` | site fetch で保持するヘッダ集合、リダイレクト時の剥がすヘッダ集合、リダイレクト解決、transport 既定ヘッダ、ヘッダ上書き規則、relay の headers 運搬 |
| `src/platform/repository.rs` / `src/db/` | WHERE / term / sort / keyset の SQL 断片生成、状態テキスト規則 |
| `src/commands/`（native）と `worker_entry/src/webui/`（worker） | ターゲット解決を 1 本化（フォールバック有無を引数で明示） |
| CI | `platform.yml` の env ブロック、ピン一覧、`sorahost.json` の単一化 |

---

## 6. 検証方法（共通化前後で固定すべきテスト）

1. **native の Set-Cookie**（A-1）: ローカル HTTP サーバが `Set-Cookie` を返す状況で
   `persist_set_cookie` 後の保存値が更新されることを固定する。既存 `tests/login_fallback.rs` は
   `MockHttpClient` なので transport の実装差を検出しない。
2. **サイト定義 version gate**（A-2）: 同梱より低い `version` を持つユーザー定義を置いたとき、
   native の DL と Worker の DL が同じ定義を使うことを固定する。
3. **スケジューラ**（A-3）: `America/New_York` の DST 当日、last-run 未記録、
   last-run 形式の往復を native / 共有層の両方で表で固定する。
4. **設定 coercion**（A-4）: `yes` / `on` / `1` / `" 0.5 "` を D1 と local_setting の両方に
   入れて、同じ解釈になることを固定する。
5. **feature tour / DTO / INI パース**: 共有層へ移したあと、native の応答 JSON バイトと
   Worker の応答 JSON バイトを比較する回帰を 1 本置く（現在は「コピー元と一致」を保証する
   テストが無い）。
6. **SORAHOST**: `sorahost/start.sh` の env 一覧と `narou.env.example` / README §3.1 の表の
   一致を機械検証する（`tests/test_sorahost_smoke.py` は smoke ステップしか見ていない）。

---

## 7. 要確認（判断が必要な点）

1. A-2 の version gate を native に掛けると、既存ライブラリで「古い version のユーザー定義が
   効かなくなる」。許容するか。
2. A-3 の catch-up / DST はどちらが narou.rb 互換か（`sample/narou` の突き合わせは未実施）。
3. `toc_url()` フォールバックの有無（2.2）はどれが正か。`src/commands/update.rs:404-407` の
   コメントは「ここだけ無し」と読めるが、Ruby 版との一致は未確認。
4. Worker の本文保存を `objects` の YAML blob のままとするか、`AGENTS.md:354` の
   「セクション行と列」へ進めるか（`content_backend` は未実装）。
5. `worker_entry/src/s3_store.rs:33` の `illustration_dedup: false` は意図的な配備差か暫定か。
6. `LibrarySortColumn` に Tags/Status/TocUrl/NewArrivalsDate が無いことは既知か
   （API は 14 列のうち 10 列しか受けられない）。
7. native にリレー段を足す実需（native 自身がリレーのホストになりうるため、別踏み台向けの
   汎用機能としての価値判断）。
8. `sorahost/README.md:187`「s3 を選んで値を欠かすと起動に失敗する」は実装と不一致の疑い
   （`src/native/s3.rs:98` は遅延評価）。
9. SORAHOST コンテナに `/.dockerenv` があるか（無ければ self-update ボタンが出る:
   `src/version.rs:119-127,145-149`）。
