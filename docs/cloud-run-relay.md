# Cloud Run Buildpacks による外部 HTTP 取得リレー

## 1. 目的と対象

Cloudflare Workers が直接取得できない場合に限り、Google Cloud Run の出口から HTTP 取得する低頻度の補助サービスです。narou.rs 本体、D1、S3、Queues は Cloudflare 側に残します。

対象は `scripts/relay/` の通常の Node.js HTTP サーバーです。起動エントリーは `bootstrap.mjs`（`npm start`）で、`server.mjs` を起動する前に curl の有無を確認します。Dockerfile・GHCR・Functions Framework・関数ターゲットは使いません。Cloud Run の配備は Developer Connect と Cloud Build が行い、Cloudflare 本体の GitHub Actions 配備から独立させます。

**同じコードが SORAHOST (PteWorker) でも動きます。** ホストごとの差（`PORT` か `SERVER_PORT` か、待受アドレス、設定の渡し方、curl の有無）は `scripts/relay/env.mjs` と `scripts/relay/transport.mjs` が吸収するので、配備先を変えてもリレーの実装は分岐しません。PteWorker へは [`sorahost-relay.md`](sorahost-relay.md) の手順で同じ `scripts/relay/` を配備します。

Workers の既存順序は `fetch` → 403 の GET に限り socket → 成功しなければ設定済みの外部リレーです。外部経路の 2xx/3xx だけを採用し、失敗なら元の 403 を維持します。サイト側のアクセス制限・利用規約を尊重し、Challenge の自動操作は行いません。

## 2. Google Cloud の前提

利用者自身の Google Cloud アカウントと、課金を有効にしたプロジェクトを使ってください。共通のアカウント・認証キーを配る構成ではありません。

次の API と IAM を準備します。

- Cloud Run Admin、Cloud Build、Artifact Registry、Developer Connect、Secret Manager。ログ確認には Cloud Logging も使用します。
- 配備するユーザーとビルド用サービスアカウントに、[継続的デプロイの公式手順](https://docs.cloud.google.com/run/docs/continuous-deployment#required-roles)の権限を付けます。Developer Connect 接続にはビルド側の `roles/developerconnect.readTokenAccessor` も必要です。
- **実行用**には専用のサービスアカウントを作り、後述の認証用 Secret だけを読む権限を付けます。ビルド用と実行用のアカウントを区別し、実行用へプロジェクト全体の Editor/Owner を付けないでください。
- 組織のポリシーで公開 HTTP サービスが禁止されている場合は管理者による許可が必要です。本体は Google の ID トークンを発行しないため、IAM 認証必須のサービスには接続できません。

作成前に予算と通知を設定してください。予算通知自体は課金を強制停止しません。

## 3. GitHub リポジトリを接続する

1. Cloud Run の「サービスを作成」から「リポジトリから継続的にデプロイ」を選びます。「関数を作成」は選びません。
2. インテグレーションに **Developer Connect**、ソース管理に **GitHub** を選びます。
3. 初回は「新しいリポジトリをリンク」から Google Cloud Run の GitHub App を認可し、必要なリポジトリだけを選びます。
4. `Rumia-Channel/narou.rs` を選びます。接続権限のない一般利用者は、自分の GitHub に fork し、その fork の `develop` を接続してください。fork の更新取り込みも自分で管理します。

GitHub の認証は Developer Connect 側で行います。GCP のサービスアカウント鍵を GitHub Secrets に保存する必要はありません。

## 4. Developer Connect と自動配備

接続・ビルドのリージョンも可能ならサービスと揃えます。[Cloud Build 用 Developer Connect は大阪をサポート](https://docs.cloud.google.com/developer-connect/docs/locations)しています。接続作成後にそのリージョンは変更できません。

対象ブランチは `develop`、正規表現を入力する画面では `^develop$` にします。保存すると Cloud Build トリガーが作られ、その後の push からビルド・配備されます。初回保存時に直ちに実行される場合もあります。

**Google 側のトリガーは GitHub Actions の完了を自動的には待ちません。** `develop` へのマージ前に必要なテストを通すブランチ保護・PR 条件を設定するか、初回・変更検証中はトリガー承認/無効化を使ってください。`[skip ci]` だけで Google 側の配備も停止すると考えないでください。本変更ではリポジトリの保護設定や Cloudflare の develop/production 配備条件を勝手に変更しません。

## 5. Buildpacks のビルド文脈と起動

ビルド方式に **Google Cloud Buildpacks** を選び、ビルドコンテキストは **`/scripts/relay`**、Entrypoint と Function target は **両方空欄**にします。

このディレクトリの `package.json` が Node.js `24.x` と `npm start` → `node bootstrap.mjs` を指定します。npm の lock も置きますが、外部 npm 依存はありません。リポジトリ全体や `worker_entry` をビルド文脈にしないでください。

`bootstrap.mjs` は起動時に PATH の curl を確認し、無ければ **`scripts/relay/pins.json` にピン留めした SHA-256 で検証した静的 curl と CA バンドルを `/tmp` へダウンロード**してから `server.mjs` を起動します（初回・コールドスタートごとに約 6.5 MB）。用意できなかった場合はこれまでどおり Node 実装（`viaPinnedHttp`）で続行し、起動は止めません。`NAROU_RELAY_CURL` / `NAROU_RELAY_CACERT` が設定済み、または curl が PATH にある場合はダウンロードしません。コールドスタートを避けたい場合は最小インスタンス数を 1 にします（課金に注意）。

Google の Node.js ランタイムイメージには curl が入っていないことがあり、その場合はリレーが Node の TLS クライアントを使うため、Cloudflare の bot 判定で 403 になるサイトが出ます。`bootstrap.mjs` はこの差を埋めるためのものです。

`K_SERVICE` のある Cloud Run 上では `0.0.0.0:$PORT`、ローカルでは `127.0.0.1:${PORT:-3000}` に待ち受けます。コードは Cloud Run が渡した PORT を上書きしません。待受は `NAROU_RELAY_HOST`、ポートは `PORT` → `SERVER_PORT` の順で解決し、設定は `NAROU_RELAY_ENV_FILE` があればその `.env`、無ければ `<モジュールの隣>/.env` → `$HOME/.env` も読みます（環境変数が最優先）。同梱の curl がある場合は書き込み可能な場所へコピーして使い、`NAROU_RELAY_CURL` / `NAROU_RELAY_CACERT` で明示もできます。

localhost 接続を許可する fixture はリポジトリ直下の `tests/` に隔離し、このビルド文脈には含めません。本番用に私設 IP を許可する環境変数はありません。`.gcloudignore` は手元からのソース送信向けで、Developer Connect のビルド文脈設定の代わりにはなりません。

## 6. Cloud Run 管理画面の設定

| 項目 | 設定値 |
| --- | --- |
| Service name | `narou-relay` |
| Region | `asia-northeast2`（大阪） |
| Deployment | Continuous deployment from Git repository |
| Integration | Developer Connect |
| Repository | `Rumia-Channel/narou.rs`（一般利用者は自分の fork） |
| Branch | `develop`（正規表現なら `^develop$`） |
| Build type | Google Cloud Buildpacks |
| Build context directory | `/scripts/relay` |
| Entrypoint | 空欄 |
| Function target | 空欄 |
| Container port | `8080` |
| CPU | `1 vCPU` |
| Memory | `512 MiB` |
| Scaling | 自動、最小 `0` / 最大 `1` |
| Concurrency | `1` |
| Request timeout | `90 seconds` |
| Billing | request-based |
| Ingress | all |
| Authentication | Allow public access |
| Custom domain | なし |
| VPC connector | なし |

公開アクセスは Google IAM の入口の設定です。アプリケーション側の `/proxy` には引き続き認証が必要です。

リレー内部は全体 75 秒、curl と代替 HTTP(S) は各 30 秒で打ち切ります。クライアント切断時は実行中の子プロセス/HTTP 接続を中断し、代替経路を追加で開始しません。常駐ジョブ、定期ポーリング、ウォームアップ用アクセス、永続ファイルは使いません。

## 7. Secret Manager に認証キーを設定する

1. 自分専用の長いランダムなキーを作ります。例: `node -e "console.log(require('node:crypto').randomBytes(32).toString('base64url'))"`。表示した値はチャット・リポジトリ・ログへ貼らないでください。
2. 自分のプロジェクトの Secret Manager に `narou-relay-key` 等の名前で保存します。値に余分な改行や空白を付けないでください。
3. 実行用サービスアカウントに、この Secret に対する **Secret Manager Secret Accessor**（`roles/secretmanager.secretAccessor`）を付けます。
4. Cloud Run の「変数とシークレット」で環境変数 **`NAROU_RELAY_KEY`** にこの Secret を参照させ、版は数値で固定します。

[Google の Secret 設定手順](https://docs.cloud.google.com/run/docs/configuring/services/secrets)では、環境変数注入は特定バージョンへの固定を推奨しています。キーは起動時に読みます。新しい Secret の版を作るだけでは既存インスタンスの値は変わらないので、参照する版を変更して新しいリビジョンへ配備します。

キー未設定では非0で起動に失敗します。照合は固定長 digest の timing-safe 比較です。不正なキーで `/proxy` を呼ぶと403になります。テストで使う予測可能なキーを本番に流用しないでください。

## 8. 接続先ホストの許可リスト

Cloud Run の通常の環境変数に **`NAROU_RELAY_ALLOWED_HOSTS`** を設定します。たとえばハーメルンだけなら次のように、実際に使用するホストを明示します。

```text
syosetu.org,www.syosetu.org,h.syosetu.org
```

- カンマ区切りの完全一致です。`syosetu.org` から別サブドメインへの許可は推測しません。`*`・URL・ポートつきの値は受け付けません。
- 大文字小文字・IDN・末尾ドットを正規化します。
- 未設定/空の許可リストでは **認証が正しくても取得を拒否**します。`/health` の成功だけで取得可能とは判断しないでください。
- HTTP/HTTPS 以外と URL 内のユーザー情報を拒否します。IP を直接指定しても、loopback・private・link-local・メタデータ用・予約/移行用等の非公開アドレスへは接続しません。
- ホスト名は全 DNS 応答を検証し、一部に非公開 IP が混ざっていても拒否します。検査した IP を curl の `--resolve` と代替 HTTP(S) の `lookup` に固定し、DNS の再解決によるすり替えを防ぎます。
- curlrc・環境プロキシを通信の迂回路にしません。TLS 証明書は元の URL のホスト名で検証します。任意の Host ヘッダーによる接続先権限の変更も拒否します。

必要な別ホストが判明したときだけ追加してください。動作確認のため一時的に `example.com` 等を追加した場合は確認後に外します。

## 9. 配備後の URL を確認する

Cloud Run のサービス詳細に表示される `https://...run.app` のサービス URL を控えます。独自ドメインは不要です。

本体は接続先をオリジンへ正規化し、`/proxy` を付けます。パス付きの中継 URL や `cloudfunctions.net/関数名` は指定しません。ブラウザから `サービスURL/health` を開き、200の `{"ok":true}` を確認してください。

コンソールの「リビジョン」「ログ」で、ビルド成功と実行時の起動成功を区別して確認します。健康状態だけでは curl が使えることも対象サイトを取れることも証明しません。

## 10. Cloudflare Workers 側の設定

自分の Worker に次の Secret を設定します。

- **`NAROU_RELAY_ENDPOINT`**: 前節の HTTPS サービス URL。
- **`NAROU_RELAY_KEY`**: Secret Manager に保存したものと同じ認証キー。

本リポジトリの本体 CI を使う場合は GitHub Environment `Cloudflare` の同名 Secret に登録します。`worker_entry/ci/deploy_worker.py` は既存どおりこれらを Worker に渡すだけで、Google Cloud の配備・認証はしません。独自に Worker を配備する利用者はその Worker 自身へ設定してください。Google のサービスアカウント JSON は不要です。

両方が揃ったときだけリレーが有効です。切り替え先の認証と取得を検証するまで、既存サービスや Secret を削除しないでください。本体の再配備/更新により設定を反映し、サイト定義・Cookie・取得間隔は従来のダウンロード経路を使います。

## 11. 動作検証

### ローカルの回帰

Node.js24・npm・curl があるリポジトリのルートで実行します。テスト用の実 workerd は runner が起動・停止し、GCP やリモート Worker を作成しません。

```sh
npm ci --prefix scripts/relay
node tests/run-relay.mjs
```

`tests/relay.test.mjs` の Node/Worker 契約に加え、許可リスト・SSRF・TLS・タイムアウト・切断・機密情報非漏洩を確認します。既存CIの `worker-contract` も同じ runner を使います。

### 実際の公開 URL

接続先とキーを安全に環境変数へ設定し、`NAROU_RELAY_PROBE_URL` には利用が許可され、許可リストへ登録した検証用 URL を置きます。

```sh
python scripts/relay/verify_auth.py
```

確認対象は health200、誤キー403、正しいキーでのリレー200です。さらに **上流status・本文・viaを確認**してください。上流403でもリレー自身は正常に200の JSON を返すので、リレー200は本文取得成功と同義ではありません。

`via=curl` が curl 経路の証拠です。`via=node-http` は実行障害後の DNS 固定された代替経路です。curl が正常な HTTP403 を受信しただけでは代替へ進みません。curl 不在・実行不可・TLS検証失敗は、生のstderrや引数ではなく有限の分類で Cloud Logging に記録します。TLS検証失敗やサイズ超過を別経路で隠しません。

最後に自分の Worker から許可された実作品を取得し、リレー利用時にも目次・本文・Cookie更新・取得間隔が従来どおりか確認します。Cloud Run向けHTTPS socketはこの実経路で検証します。実失敗の根拠なしにWorkers fetchへ置換しません。

大阪リージョンの指定は、日本IPとしての判定を保証しません。必要なら一時的に許可したIP確認サービスで出口と国判定を調べます（例: `www.cloudflare.com/cdn-cgi/trace` の `ip` / `loc`）。その判定と対象サイト自身の判定も同じとは限りません。対象サイトからの応答を別に確認し、確認後は検証ホストの許可を削除してください。

### プロトコルと上限

`GET /health`、`POST /proxy`、互換用`GET /proxy`を維持します。POSTは`url`/`headers`を送り、応答は`status`・`contentType`・`location`・全`setCookie`・base64の`body`・任意`via`です。重複ヘッダー順序・UTF-8の既存表現・バイナリ・圧縮本文を保ち、リダイレクトは追いません。

| 対象 | 上限 |
| --- | --- |
| リクエスト JSON | 64 KiB（文字数ではなくバイト数） |
| 展開後の上流本文 | 16 MiB |
| 上流ヘッダー | 共通 protocol の 64 KiB |
| JSON メタデータ | 共通 protocol の 448 KiB |
| base64 本文込みの JSON | 本文の約4/3＋メタデータ。16 MiBそのものではない |

アプリはCookie・Authorization・取得URL・子プロセスの生診断をログに出しません。ただし互換用GETで機密値をURLへ載せると、Google等のアクセスログに残る可能性があります。通常の本体はPOSTを使います。ログ設定でも本文・ヘッダーを独自に収集しないでください。

## 12. コールドスタート

最小0なので初回や休止後は起動待ちが発生します。常時起動のための定期アクセスは追加しません。Cloud Run自身の90秒と本体socket側の接続/読み取り期限は別物です。長く待てば必ず取得できるという保証はありません。

低頻度の補助経路を想定しています。複数サイトが同時にリレーを要求すると、最大1・同時実行1の設定では待機や拒否が起き得ます。単に本体の同一サイト並列数を増やして解決しないでください。

## 13. 無料枠と課金リスク

[Cloud Run料金](https://cloud.google.com/run/pricing)の無料枠は請求先アカウント単位で集計され、リージョン・利用状況・課金方式に依存します。「低頻度なので必ず無料」とは判断しません。

CPU/メモリ/リクエストだけでなく、インターネットへの転送、Cloud Build、Artifact Registryの保存、Secret Manager、ログにも注意してください。base64化で転送量も増えます。日本リージョンの転送を北米の無料転送枠と混同しないでください。

最小0・最大1は費用やメモリの上振れを抑える設定で、請求額の上限保証ではありません。公開エンドポイントでは認証で拒否したリクエストでもアプリまで届くため処理費用が発生し得ます。予算通知・使用量監視・不要画像の整理を行い、利用しないサービスは停止してください。

## 14. アップデートとロールバック

`develop`（forkならそのブランチ）への更新をDeveloper Connectが検知し、Cloud Buildが新しいリビジョンを作成します。Cloud Buildの成功、Cloud RunのReady、公開URLのhealth/認証/本文/curl経路の順に確認します。GitHub側のチェックとGoogle側の履歴は別々に確認してください。

問題があればトリガーを一時無効化/承認待ちにしてから、Cloud Runの「リビジョン」→「トラフィックを管理」で検証済みリビジョンへ戻します。根本修正はコード上で行い、コミットの由来を残して再配備します。強制pushで履歴を改変しません。

ロールバック先でもSSRF/認証対策が有効なこと、参照Secret版が利用可能なことを確認します。未保護の旧リレーへ戻して公開しないでください。キーを更新したときはCloud RunとWorkerの値も一致させます。切り替え中のリレー障害は元の取得結果を保持するため、取得が成功するとは限りません。

## 15. 停止と削除

1. まず自分のWorkerからリレー設定を外すか、必要な代替経路へ切り替えます。
2. Cloud Buildトリガーを無効化し、pushで復活しないようにします。
3. 一時停止はCloud Runの「スケーリング」で手動インスタンス数を0にします。最小インスタンス0だけでは自動起動するので停止ではありません。詳細は[手動スケーリング](https://docs.cloud.google.com/run/docs/configuring/services/manual-scaling)を参照してください。
4. 再開時は自動スケーリングへ戻し、既定の最小/最大・認証・許可リストを再確認します。
5. 完全撤去はCloud Runのサービスを削除します。不要になった自分のトリガー、Developer Connect接続、Artifact Registry画像、Secretも個別に整理します。共有中の他サービスの資源を削除しないでください。

停止・削除でも実行中の処理や別途保管している画像/ログ等の費用が直ちに全て消えるとは限りません。管理画面で残存資源と請求を確認してください。

## 16. よくあるエラー

| 現象 | 確認箇所と対処 |
| --- | --- |
| ビルドがpackage.jsonを見つけない | コンテキストがリレー用ディレクトリか、developに必要なファイルがあるか確認 |
| 関数ターゲット/Frameworkのエラー | 通常のサービスとして作り、Entrypoint・Function targetを空にして再ビルド |
| PORTを待ち受けない | 起動ログ、Node24、Secretの参照権限、許可リストの構文、ポート競合を確認。失敗時は非0 |
| healthは200だがproxyが400 | 許可ホストの完全一致、全DNS応答の公開性、URL/headers/64KiB制限を確認 |
| リレー自身が403 | IAM公開設定とアプリ認証を区別。リレーのJSON403ならキーの一致を確認 |
| リレー200だが上流403 | 対象サイトの拒否。リレー認証成功でも取得成功ではない。制限や規約を確認 |
| 404 / 405 | パスとメソッドを確認。普通のページや関数URLではなくサービスURLを使う |
| 502 / timeout | 分類ログ、DNS/TLS、切断・本文上限・上流状態を確認。機密を含む診断を公開しない |
| `not-installed` / `not-executable` | 実リビジョンでcurlが使用できていない。ビルド/実行環境を確認し、ハーメルン取得成功とは扱わない |
| `tls-verification` | CA/証明書/ホスト名を確認。検証を無効化して回避しない |
| 本体が元の403を返す | リレー未設定、リレー障害、上流が2xx/3xx以外の場合を確認 |
| 前の版へ勝手に戻る/直らない | トリガーと配備元ブランチ、実際に配備されたcommit/リビジョン、Secret版を確認 |

## 17. Cloudflare 保護サイト（ハーメルン等）は取得できない

2026-10-20 の実測では、このリレー（データセンター回線）から `https://syosetu.org/`（ハーメルン）を取得すると、curl・wget・Node の HTTP/1.1／HTTP/2 のいずれでも **HTTP 403 (`cf-mitigated: challenge`)** でした。対照の `https://ncode.syosetu.com/` は正常（302）です。クライアントを変えても結果は変わらないため、Cloudflare は出口 IP を見て managed challenge を返しています。

ハーメルン等を Workers から取得するには **自宅回線で動かすリレー**が必要です。手順と実測値は [home-relay.md](home-relay.md) を参照してください。このリレーは Pixiv など、データセンター回線でも取得できるサイト向けに引き続き利用できます。

## 18. 取得クライアントの切り替え（NAROU_RELAY_FETCH）

既定は `curl,node`（curl で取得し、curl が起動できない場合だけ Node）。Cloudflare が curl を 403 にし wget は通すホスト（例: 共有 IPv4 NAT の格安 VPS）では、`NAROU_RELAY_FETCH=wget,node` で wget 経由にできます。

- 値はカンマ区切りの取得順。実行できない wget は起動時に外れ、起動ログと `/health` の `fetch` に実際の順序が出ます
- wget は curl の `--resolve` 相当が無いため、127.0.0.1 の使い捨て CONNECT プロキシで検証済み IP へ接続し、TLS の SNI・検証は wget が行います
- HTTP（非 TLS）の対象は wget では扱わず、次の経路へ進みます
- 403 などの HTTP 応答は「取得結果」として扱われ、別クライアントでの再試行はしません（1 リクエスト = 上流 1 回の契約を維持）
- リレーがデータセンター回線のままなら、wget に替えても Cloudflare の challenge は解けません。詳細と手順は [vps-relay.md](vps-relay.md) / [home-relay.md](home-relay.md)

## 検証済み範囲と未検証事項

ローカルのNode24/Windowsでは、実Node/ローカルworkerdを通す54件とRustリレー9件・HTTP/1.1 codec23件が成功し、Worker wasmチェックも成功しました。実際の`npm start`で8080の公開待受、health200、不正キー403、公開検証先example.comの上流200/本文/`via=curl`、メタデータ宛の拒否、鍵未設定/ポート競合時のexit1を確認しました。

その後、利用者から実サービスの起動成功が報告されています。ただし、以下の実ログ・取得経路・対象サイト応答をこちらで確認したものではありません。

次は**ローカル試験では証明できず、実環境で未検証**です。

- Google Cloud Build/Developer Connectの実ログと、GitHub pushからの自動再配備の一連の確認。
- Google上の実行リビジョンでのcurl起動、CA/TLS、出口IPが日本として扱われるか、対象サイトが取得を許可するか。
- 実WorkerからCloud RunのHTTPSサービスへの既存socket経路、実データのCookie/本文/画像の取得。
- 実プロジェクトのIAM、Secret注入、リビジョン切替、スケーリングと請求。

実サービスURLと利用者自身のプロジェクトで確認するまで、移行の全完了条件を満たしたとは扱いません。認証・SSRF対策未完成の版を本番データで公開運用しないでください。
