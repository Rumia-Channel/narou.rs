# SORAHOST で narou.rs を動かす

SORAHOST (PteWorker の node モード) のコンテナで native 版 `narou_rs` を常駐させる
手順。**Workers は使わない**。専用サーバー 1 台で完結し、公開は PteWorker が行う (前段に
Cloudflare のコネクタと Access を置く構成も選べる §2)。CI から配備するのに必要な値は
**`SORAHOST_ENDPOINT` と `SORAHOST_TOKEN` の 2 つだけ** (§9)。

```
Browser ──https──▶ PteWorker (公開 URL / TLS)
                      │ ループバックへ転送
                      ▼
              SORAHOST コンテナ内
                narou_rs web (127.0.0.1:$PORT)
                      │
                      └─ 挿絵だけ S3 (Wasabi など) へ
```

コンテナ (プロジェクトルート = ボリューム) の中身:

```
sorahost.json   配備の定義 (リポジトリの sorahost/sorahost.json)
start.sh        起動スクリプト (リポジトリの sorahost/start.sh)
app/            実行ファイル + webnovel/  (配備で入れ替わる)
library/        narou のデータ (.narou / 小説データ / webnovel)。配備に含めない
```

- アプリは **PteWorker から渡される `PORT`** にループバックで束縛する
  (外部へ直接公開しない。受信ポートの設定は不要)
- メタデータ・本文はライブラリ内の SQLite (`.narou/db.sqlite`)、挿絵だけ S3 に置く
- 公開 URL は PteWorker が発行する (`sorahost-cli deploy` の出力 `url`、または
  コンソールの `url` コマンド)

## 1. ビルド (Linux バイナリを用意する)

Linux x86_64 の実行ファイルが必要。手元に Linux が無くてもよい。

### A. GitHub Actions で作る (推奨)

Actions の **Linux build (SORAHOST)** を `workflow_dispatch` で実行し、完了後に
artifact `narou_rs-linux-x86_64` をダウンロードして展開する。中身:

```
narou_rs-<版>-linux-x86_64-gpl/
  narou_rs / narou_rs_backup / narou_rs_login
  webnovel/   同梱のサイト定義
  preset/     外部 AozoraEpub3 を使う場合の雛形 (組み込みエンジンでは不要)
  Third-Party-License.md
```

ビルドは GitHub ホストの runner (ubuntu-22.04 = glibc 2.35) で行うので、
**self-hosted runner は使わない**(公開リポジトリの fork PR を手元で走らせないため)。
`lto = true` のリンクは数 GB のメモリを使うので、VPS 上でのビルドも勧めない。

置ける環境は glibc 2.35 以降 (Debian 12 / Ubuntu 22.04 以降)。それより古い
コンテナなら、一致するイメージ内でビルドするか musl ターゲットに切り替える。

### B. 手元の Linux / WSL で作る

```sh
cargo build --release --features lite
```

`target/release/` の `narou_rs`(+ `narou_rs_backup` / `narou_rs_login`)を、
リポジトリの `webnovel/*.yaml` と一緒に配置する。`narou init` は実行ファイルの
隣の `webnovel/` から定義をコピーするため、**`webnovel/` を同梱しないと
サイト定義が空になる**。

配置 (コンテナ内。プロジェクトルート = ボリュームの直下):

```
sorahost.json            リポジトリの sorahost/sorahost.json
start.sh                 リポジトリの sorahost/start.sh
app/narou_rs             実行ファイル (chmod +x)
app/narou_rs_backup      バックアップ用ヘルパー (Web UI のバックアップが使う)
app/webnovel/*.yaml      同梱のサイト定義 (初回 init が library/ へコピーする)
library/                 初回起動で作られる (.narou / 小説データ / webnovel)
```

`library/` は配備に含めないので、配備を繰り返しても作品データは残る (はず。
初回は 2 回配備して残ることを確認する)。

## 2. 公開

2 通り。どちらでもアプリは `127.0.0.1:$PORT` にだけ束縛する。

### A. 前段に Cloudflare のコネクタ + Access を置く (推奨)

TLS と本人確認 (Zero Trust) を前段に置く。直の IP:ポート宛は narou の Host 許可リストで
弾かれるので、公開経路はトンネルだけになる。

1. GitHub の Environment `SORAHOST` に値を置く (§3.2)
2. `develop` へ push すると `deploy-sorahost.yml` が `sorahost/ci/provision_connector.py` を実行し、
   コネクタ / 向き先 (ingress) / DNS /(設定していれば) Access を冪等に作る
3. Cloudflare の **Networks → Tunnels → 該当のコネクタ → Add a replica** でトークンを確認し、
   PteWorker の `.env` に `NAROU_CONNECTOR_TOKEN=<トークン>` と `NAROU_PUBLIC_HOST=<公開ホスト名>` を置いて再起動
4. `https://<公開ホスト名>/` が Access のログインを要求し、`http://<IP>:<PORT>/` が **400** になれば成功

- `NAROU_CONNECTOR_TOKEN` は CI では扱わない (公開リポジトリのログに出さないため)。ダッシュボードで
  確認してサーバー直下の `.env` に置く
- 向き先 (ingress) は `127.0.0.1:<SORAHOST_SERVICE_PORT>` (既定 18080 = PteWorker の `PORT`)。
  `NAROU_RS_PORT` を変えたら合わせる
- **トークンの権限 (UI の探し方)**: My Profile → API Tokens → Create Token → Custom token。
  - アカウント全体のポリシーに次を足して **Edit** (どれでも可。Cloudflare の API リファレンスが
    3 つ併記している): **`Cloudflare One Connectors`** / **`Cloudflare One Connector: cloudflared`** /
    **`Cloudflare Tunnel`**(説明が "Grants access to create and delete Cloudflare Tunnels")。
    似た名前の **`Argo Tunnel (Legacy)`** は旧版なので選ばない
  - **DNS はゾーン スコープ**なので、ポリシーをもう 1 行足して「ゾーン」→ 対象ドメイン →
    `DNS: Edit` と `Zone: Read`(ゾーン ID の解決に必要)
  - Access を使うなら同じアカウント全体のポリシーに **`Access: Apps and Policies: Edit`**
  - ダッシュボードの表記は Read/Edit、API リファレンスは Read/Write (同じもの)
- Zero Trust が未有効のアカウントでは Access の作成が 403 で止まる。ダッシュボードで有効化するか、
  `SORAHOST_ACCESS_EMAIL` / `SORAHOST_ACCESS_DOMAIN` を外して Access なしで進める
  (その場合は TLS だけが付く)
- 前段に Access を置く構成では **basic 認証は自動で切る** (二重になるだけのため)。
  残したいときだけ `NAROU_WEB_PASSWORD` を `.env` に置く (併用も可)

### B. PteWorker の公開 URL をそのまま使う

追加設定は不要 (`start.sh` は reverse proxy モードで動く)。ただし**平文 HTTP** なので
basic 認証のパスワードが回線上を素で流れる。強いランダム値にした上で、早めに A へ移ること。

## 3. 環境変数

置き場は 2 つだけ。**アプリの値はサーバー上の `.env`**、**CI の値は GitHub** に置く。
ここに挙げた名前がコードの読む全部で、綴りが違うと無視される。

### 3.0 アプリの値をどこに書くか

PteWorker の `sorahost.json` には環境変数を渡す項目が無く、Pterodactyl の
**Startup 変数**は名前が egg 側で決まっているため、こちらで用意した名前
(`NAROU_*`) を足せないことが多い。そこで `start.sh` は起動時に
**ボリューム直下の `.env`** を読み込む:

```
<ボリューム直下>          = アプリのログに出る /home/container
  .env                    ← これを置く (下記)
  narou-library/          作品データ
  bin/                    コネクタのバイナリ
  .sorahost/releases/...  配備ごとの実行ファイル
```

`.env` の作り方 (どちらでもよい):

- パネルの**ファイルマネージャ**で直下に `.env` を作って編集する
- SFTP (`sorahost-cli` の接続情報と同じホスト) で置く

```sh
# 例 (/home/container/.env)
NAROU_WEB_PASSWORD=長いランダム文字列
NAROU_WEB_USER=admin
NAROU_CONNECTOR_TOKEN=eyJ...            # ダッシュボードの Add a replica の値
NAROU_PUBLIC_HOST=narou.example.com     # 公開ホスト名
```

- 形式は `KEY=VALUE` の行、`#` で始まる行は無視。値は前後の空白も含めてそのまま使われる
- **既に環境にある値の方が優先**(プラットフォームの `PORT` や Startup 変数は上書きされない)
- パスを変えたいときは `NAROU_RS_ENV_FILE` で指定する
- 変更後は再起動(パネルの `restart`、または次の配備)で反映される

### 3.1 サーバー側の `.env` に書く値 (アプリの動作)

| 変数 | 何のため | 設定する値 | 設定ファイルとの優先 |
| --- | --- | --- | --- |
| `NAROU_WEB_PASSWORD` | Web UI の basic 認証。素の公開 URL では必須。コネクタを使うときは不要 (自動で切る) | 長いランダム文字列 | start.sh が `server-basic-auth.password` に書く |
| `NAROU_WEB_USER` | basic 認証のユーザ名 | 例 `admin` | 同 `server-basic-auth.user` (**未設定なら `admin` を補う**) |
| `NAROU_ALLOW_NO_PASSWORD` | `1` で「パスワード無しでも起動する」 | コネクタなしで認証なしにするときだけ `1` (コネクタ使用時は自動で切るので不要) | — |
| `NAROU_RS_ASSET_BACKEND` | `s3` で挿絵だけ S3 へ | `s3` (未設定 = ローカル保存) | **設定 `s3.asset-backend` が優先** |
| `S3_ENDPOINT` | S3 互換の接続先 | Wasabi: `https://s3.ap-northeast-1.wasabisys.com` | 設定 `s3.endpoint` が優先 |
| `S3_BUCKET` | バケット | 例 `mybucket` | 設定 `s3.bucket` が優先 |
| `S3_REGION` | リージョン | 例 `ap-northeast-1` | 設定 `s3.region` が優先 |
| `S3_PREFIX` | バケット内の接頭辞 | 例 `narou/library` | 設定 `s3.prefix` が優先 |
| `S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY` | 資格情報 | ストレージのキー | 設定 `s3.access-key-id` / `s3.secret-access-key` が優先 |
| `NAROU_RS_LOGIN_KEY` | ログイン Cookie の暗号鍵 (base64) | `openssl rand -base64 24` | `.narou/login.key` より優先 |
| `NAROU_RS_MIRROR_FILES` | `0` で `小説データ/` を作らない | 通常は未設定 (start.sh が `sqlite.mirror-files=false` を入れる) | **環境変数が設定より優先** |
| `NAROU_RS_SECTION_CACHE` | `0` で話ごとの変換キャッシュを作らない | 通常は未設定 (start.sh が `convert.section-cache=false` を入れる) | **環境変数が優先** |
| `NAROU_RS_KEEP_TXT` | `0` で変換 txt を残さない | 通常は未設定 (start.sh が `convert.keep-txt=false`) | **環境変数が優先** |
| `NAROU_RS_LEGACY_YAML` | `1` で SQLite をやめ YAML 管理に戻す | 通常は未設定 | 環境変数のみ |
| `NAROU_RS_EPUB_ENGINE` | EPUB エンジン | `lite` / `external` / `auto` (未設定 = auto) | **環境変数が `convert.epub-engine` より優先** |
| `NAROU_RS_APP` / `NAROU_RS_LIBRARY` | `app/` とライブラリの場所 | 通常は未設定 (配備パスから自動) | start.sh |
| `NAROU_RS_PORT` | 待受ポート | 通常は未設定 (プラットフォームの `PORT` を使う) | start.sh (リレー同居時のみ 8080) |
| `NAROU_RELAY` / `SORAHOST_PROXY_KEY` | 同じサーバーで取得リレーも動かすときだけ (§10) | `1` / 合言葉 | start.sh |
| `NAROU_CONNECTOR_TOKEN` | コネクタ (cloudflared) の接続トークン。置くと起動し、basic 認証を切る (`NAROU_WEB_PASSWORD` 併記で併用) | ダッシュボードの Add a replica の値 | start.sh |
| `NAROU_PUBLIC_HOST` | 公開ホスト名。置くとその Host 以外を弾く | 例 `narou.example.com` | start.sh |
| `NAROU_CONNECTOR_TOKEN_FILE` | トークンをファイルで渡す場合のパス | 既定 `$VOLUME_DIR/.connector-token` | start.sh |

- `PORT` はプラットフォームが渡す値で、**こちらから設定しない** (narou はこれに束縛する)
- `s3` を選んで値を 1 つでも欠かすと**起動に失敗する** (黙ってローカル保存へ落ちない)
- ログインが要るサイトを使うなら `NAROU_RS_LOGIN_KEY` を決めておく (鍵を変えると保存済み Cookie は読めなくなる)
- basic 認証は **資格情報が空だと無効** (素通し) になる実装。コネクタなしの素の公開では
  `NAROU_WEB_PASSWORD` を必ず設定すること (`NAROU_WEB_USER` は未設定なら `admin` が入る)。
  未設定のまま起動すると `start.sh` がその場でランダムなパスワードを作って設定し、
  コンソールに表示する (誰も入れない状態で公開だけは避ける)。起動自体は成功するので配備は
  成功する (止めると PteWorker がデプロイ失敗 422 と見なし、前のリリースを配り続けるため)。
  コネクタを使うときは basic 認証を自動で切るので、パスワードは要らない。
  コネクタなしで認証なしにしたいときだけ `NAROU_ALLOW_NO_PASSWORD=1`

### 3.2 GitHub 側に置く値 (CI)

| 種類 | 名前 | 値 |
| --- | --- | --- |
| Repository variable | `SORAHOST` | `T` で SORAHOST 配備が有効になり、Worker 側の CI が止まる (`F` / 未設定で逆) |
| Environment `SORAHOST` secret | `SORAHOST_ENDPOINT` | PteWorker コンソールの「エンドポイント」 |
| Environment `SORAHOST` secret | `SORAHOST_TOKEN` | 同「デプロイトークン」(`token rotate` で再発行) |
| Environment `SORAHOST` secret | `SORAHOST_SMOKE_URL` | 任意。配備後の確認先 (未設定なら配備結果の `url`) |
| Environment `SORAHOST` secret | `CLOUDFLARE_API_TOKEN` | 任意 (§2-A を使うとき)。権限は §2-A の箇条書きを参照 |
| Environment `SORAHOST` secret | `CLOUDFLARE_ACCOUNT_ID` | アカウント ID (識別子だが外に見せたくないので secret) |
| Environment `SORAHOST` secret | `SORAHOST_PUBLIC_HOSTNAME` | 公開ホスト名 (例 `narou.example.com`) |
| Environment `SORAHOST` secret | `SORAHOST_SERVICE_PORT` | コネクタの向き先 = コンテナ内の待受ポート (既定 18080 = PteWorker の `PORT`) |
| Environment `SORAHOST` secret | `SORAHOST_CONNECTOR_NAME` | コネクタ名 (既定 `narou-sorahost`) |
| Environment `SORAHOST` secret | `SORAHOST_ACCESS_EMAIL` | Access で許可するメール (カンマ区切り) |
| Environment `SORAHOST` secret | `SORAHOST_ACCESS_DOMAIN` | 許可するメールドメイン |
| Environment `SORAHOST` secret | `SORAHOST_ACCESS_SESSION` | Access のセッション有効期限 (既定 24h) |

§3.1 の値は CI には置かない (サーバー直下の `.env` に置く)。

### 3.3 鍵・資格情報の仕様

| 値 | 仕様 (コードが受け付ける形) | 作り方 |
| --- | --- | --- |
| `NAROU_WEB_PASSWORD` | 任意の文字列。**空だと basic 認証が無効になる**実装なので空にしない | 未設定なら `start.sh` が 24 文字の英数を自動生成。自分の値にするなら `.env` に置く |
| `NAROU_WEB_USER` | 任意の文字列 | 未設定なら `admin` を補う |
| `NAROU_RS_LOGIN_KEY` | **base64** (標準・パディングあり)。復号後 **16 バイト以上**が必要。32 バイトはそのまま、16〜31 バイトは SHA-256 で 32 バイトへ伸長。前後の空白は無視、壊れた base64 はエラー | `openssl rand -base64 24` (または `-base64 32`) |
| `S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY` | 空でない文字列 (SigV4 の鍵。長さの規定なし) | ストレージ側で発行 |
| `S3_ENDPOINT` | `scheme://host` (必要なら `:port`)。path-style で扱う | Wasabi: `https://s3.<region>.wasabisys.com` |
| `S3_REGION` | 空でない文字列 (署名に必須) | 例 `ap-northeast-1` |
| `SORAHOST_ENDPOINT` | URL。パス込みでよく、`<endpoint>/deploy` へ送る | PteWorker コンソールの「エンドポイント」 |
| `SORAHOST_TOKEN` | 不透明な文字列。`Authorization: Bearer` で送る | 同「デプロイトークン」(`token rotate` で再発行) |
| `SORAHOST_PROXY_KEY` | 空でない文字列。完全一致で照合 (定数時間比較ではない) | `openssl rand -hex 32` など |

内部形式 (実装が決めているもの。手で作らない):

- 保存するログイン Cookie は `enc:v1:<base64 nonce>:<base64 暗号文>`。XChaCha20-Poly1305、nonce 24 バイト、
  AAD は `narou.rs/login-cookie` でサイト (ホスト) を束ねるため、別サイトへ流用できない。
  旧形式の平文は読める (次回保存で暗号化)
- `narou login export` の書き出しは version 3 (version 1 / 2 も読める)。
  `--passphrase` 指定時は Argon2id (19 MiB / t=2 / p=1、salt 16 バイト) → XChaCha20-Poly1305
  (AAD `narou.rs/login-export`)
- 資格情報の ID は UUIDv4 形 (小文字 hex)
- 鍵を変えると保存済み Cookie は復号できない (取り込み直す)
- `NAROU_ADMIN_TOKEN` は Worker 用 (SORAHOST=T の間は未使用)。定数時間比較で、未設定なら 500
  `authentication_not_configured`

Worker 側の CI は別系統の値を使う (Environment `Cloudflare`): `CLOUDFLARE_API_TOKEN` /
`CLOUDFLARE_ACCOUNT_ID` / `SORAHOST_PROXY_ENDPOINT` / `SORAHOST_PROXY_KEY` /
`SORAHOST_PROXY_TOKEN` / `NAROU_S3_*`。**native の `S3_*` と Worker の `NAROU_S3_*` は
別物**なので混同しないこと (`SORAHOST=T` の間は使われない)。

## 4. 起動

起動コマンドは `sorahost.json` が持つ (`"start": "bash start.sh"`)。Pterodactyl の
画面で起動コマンドを設定する必要はない (`sorahost-cli deploy` が反映する)。

初回起動でライブラリを作り、`storage-backend` を `sqlite` に切り替え、
`server-bind=127.0.0.1` / `server-port=$PORT` / `server-reverse-proxy.enable=true`
を設定する。

配備で消えない場所 (実測で確認した挙動):

- **ライブラリは release ディレクトリの外**に置く。PteWorker は配備ごとに
  `/home/container/.sorahost/releases/<日時>-<hash>/` を作り直すため、その中に
  ライブラリを置くと配備のたびに作品が消える。`start.sh` は配備パスから
  ボリューム直下を割り出して `$VOLUME_DIR/narou-library` を使う
  (上書きは `NAROU_RS_LIBRARY`)
- **`server-ws-port=0` を設定する**。narou.rb は `server-port + 1` も WebSocket に
  使うが、PteWorker はその番号を自分のルータ (workerd, 例 18081) に使うため
  `Address already in use` でプラットフォーム側が落ちる。`0` にすると併設リスナーを
  作らず、本体ポートの `/ws` で受ける (Web UI の接続先は元から `/ws` のため機能は落ちない)

## 5. 既存ライブラリの移行

手元のライブラリ (`.narou` + `小説データ`) を SORAHOST へ持って行き、挿絵だけを
S3 へ逃がす場合:

```sh
narou setting s3.asset-backend=s3          # 以降 this ライブラリは S3 を使う
narou illust s3-push                       # 件数と容量を数えるだけ (dry-run)
narou illust s3-push -f                    # 実際に S3 へ写す
narou illust s3-verify                     # バイト単位で突き合わせる
```

`narou setting s3.asset-backend=local` に戻せば、移行前のローカル保存へ即座に
戻せる (S3 側は消さない)。

## 6. 容量

実測 (13 作品・画像主体の Pixiv 作品を含む) の内訳と、SORAHOST での考え方:

| 保存先 | 内容 | 実測 (13 作品) |
| --- | --- | --- |
| S3 | 挿絵のバイナリ | 36.8 MB (S3 へ移動) |
| `db.sqlite` | メタデータ + 本文 + 変換済みテキスト (brotli 圧縮) | 79 MB |
| `小説データ/` | 本文・raw HTML・変換済みテキスト・EPUB | 148 MB (挿絵を除く) |
| 変換キャッシュ | `.narou/section_convert_cache/` | 0 (start.sh が無効化) |

- 挿絵を S3 に置くと、ローカルのミラーと `db.sqlite` の両方から消える (この構成で -73 MB)
- `start.sh` は `convert.section-cache=false` を設定する。無効にすると変換のたびに
  全話を変換し直す代わりに、容量を使わない (有効時の実測で 1 作品あたり約 1.5 MB)
- `start.sh` は初回に `economy=nosave_diff,nosave_raw` / `convert.no-epub=true` /
  `convert.keep-txt=false` を入れる。止まるものと実測 (1 作品あたり):
  - 差分スナップショット (`nosave_diff`): 6.7 MB
  - raw HTML (`nosave_raw`): 19.4 MB。挿絵のローカライズは取得時にメモリ上の HTML で
    行うので、新規話の挿絵は従来どおり保存される
  - 保存 EPUB (`no-epub`): テキスト作品で 2 MB、画像主体で 40〜55 MB
  - txt (`keep-txt`): 5.2 MB。変換結果は SQLite に残り、Web UI の
    「EPUB をダウンロード」はそこから都度生成する
  - 実ファイルのミラー (`sqlite.mirror-files=false`): `小説データ/` を作らない。
    **保存先は `.narou/db.sqlite` だけ**になり、変換は必要な間だけ
    `toc.yaml` / `本文/*.yaml` / `setting.ini` / `replace.txt` を取り出して消す。
    この構成では `narou illust orphan|rebuild|fix-ext` と `narou clean` は
    (対象の実ファイルが無いので) 実質何もしない。`narou backup` と
    `narou db export-yaml --in-place` によるロールバックは従来どおり動く
- 挿絵の削除を伴う `narou illust orphan -f` は、raw が無い構成ではキャッシュを唯一の
  参照源にする (キャッシュは保存時に必ず書かれるが、これを失っていると孤児と判定される)
- **古い出力ファイルは残る**: 出力名の設定 (`convert.filename-to-ncode` や
  `[作者名]` 接頭辞) を変えると、同じ本文の `novel.txt` / `.epub` が別名で増える。
  手元のライブラリをそのまま持ち込むと重複を引き継ぐので、気になる場合は削除する
  (実測: なろう長編 1 作品で txt 3 重複 14.9 MB + epub 2 重複 4.4 MB)
- **EPUB を溜めないのが一番効く**: 画像主体の作品は 1 冊 40〜55 MB になる。Web UI の
  「EPUB をダウンロード」は保存済みテキストから都度生成するので、`narou convert` で
  EPUB を作らない設定 (`convert.no-epub=true`) にしておけばディスクを消費しない
- バックアップ (`narou backup`) は挿絵が S3 にあるためテキスト主体で小さくなる

## 7. メモリ

実測 (Windows の debug ビルド = 上限側の値。Linux の release は通常もっと小さい):

| 構成要素 | 実測 |
| --- | --- |
| `narou web` 常駐 (13 作品・API 1 回) | 31 MB |
| `narou convert` (391 話の長編) | ピーク 61 MB |
| `narou convert` 2 本同時 (concurrency 相当) | 合計ピーク 120 MB |
| SQLite | ディスク backed (ページキャッシュは既定で 2MB 程度) |

`start.sh` は `concurrency=true` を設定する (DL/update と convert/send を別レーンで並行)。
トンネルやリレーを同居させない構成 (既定) のピークは次のとおり:

| メモリ割当 | 通常 (2 ジョブが普通サイズ) | スパイク含む最悪ケース |
| --- | --- | --- |
| 256 MB | 約 180 MB (余裕 70 MB) | 超過し得る (うごイラ + 大きい EPUB が重なるとき) |
| **384 MB** | 約 180 MB (余裕 200 MB) | 約 300〜320 MB で収まる見込み |

スパイク込みでも安全側に倒すなら **384 MB** を推奨。256 MB でも通常運用は収まるが、
画像の多い作品を同時に 2 本処理するときだけ余裕がなくなる。

- 小説単位の排他 (`.narou/lock.yaml`) があるので、同じ小説が両レーンで同時に走らない
- スパイクが大きいのは うごイラ (APNG) の組み立て (実例 19 フレームで約 56MB) と
  挿絵入りの大きい EPUB (S3 は 1 オブジェクトを最大 64MB まで一括で読み書きする)
- OOM で落ちる場合は `narou setting concurrency=false` にするか、メモリ割当を増やす

## 8. 更新

`develop` に push する (§9 の CI が配備する) か、手元から:

```sh
cd <sorahost.json があるフォルダー>
SORAHOST_ENDPOINT=... SORAHOST_TOKEN=... npx sorahost-cli deploy --yes
```

配備すると PteWorker が起動し直す。`self-update` は使わない。
`app/webnovel/*.yaml` を差し替えたときも、配備 (か手動の `restart`) で反映される。

## 9. CI からの自動配備

`develop` へ push する (または Actions の **Deploy SORAHOST** を手で回す) と、
ビルドから配備まで自動で流れる。**動くのは Repository variable の
`SORAHOST` が `T` のときだけ** (未設定・`F` なら何もせず終わる)。

`SORAHOST` が `T` の間は **Cloudflare Workers 側の CI も止まる**
(`.github/workflows/platform.yml` の wasm / worker / worker-contract / relay-deploy /
worker-deploy-develop / worker-deploy-production が skip)。Worker に戻すときは
`SORAHOST` を `F` にするか消す (native / native-gpl / license は常に走る)。

流れ:

1. `.github/workflows/build-linux.yml` が Linux バイナリを作る (GitHub ホストの runner)
2. `deploy/` に `app/` (narou_rs / narou_rs_backup / webnovel) + `start.sh` +
   `sorahost.json` を集める
3. `sorahost-cli deploy` で SORAHOST へ送る (`--json` の `url` が公開先)
4. 公開先を叩いて応答を確認する (basic 認証があるので 401/403 でも「立っている」)

### Environment `SORAHOST` に置く値

| 種別 | 名前 | 用途 |
| --- | --- | --- |
| secret | `SORAHOST_ENDPOINT` | PteWorker のコンソールに出るエンドポイント |
| secret | `SORAHOST_TOKEN` | デプロイトークン (`Authorization: Bearer`) |
| var | `SORAHOST_SMOKE_URL` | 任意。確認先 (未設定なら配備結果の `url` を使う) |

この 2 つ (と任意の 1 つ) だけ。SFTP も Cloudflare の資格情報も使わない。

> `SORAHOST` 環境に**同名の `SORAHOST` 変数を置かないこと** (Repository variable
> の判定と混ざる)。

アプリ側の設定 (`NAROU_WEB_PASSWORD` / `S3_*` / `NAROU_RS_ASSET_BACKEND` など) は
CI ではなく **PteWorker の `.env`** に置く (§3、ひな形は `sorahost/narou.env.example`)。

## 10. Worker の取得リレーと同じサーバーで動かす (任意)

既定は **別サーバー** (専用サーバー 1 台で narou だけを動かす)。どうしても同じ
サーバーへ同居させたい場合だけ、次の手順になる。PteWorker は
**1 プロジェクト = 起動コマンド 1 つ** なので、起動コマンドを narou の `start.sh`
に寄せて、リレーはそこから起動する。

1. サーバー直下の `.env` に `NAROU_RELAY=1` を足す (`SORAHOST_PROXY_KEY` は既にある値のまま)
2. リレー側の起動コマンドを `bash start.sh` に差し替える。リレーを CI から配備して
   いる場合は Repository variable `SORAHOST_RELAY_START` = `bash start.sh` を入れる
   (未設定なら従来どおり `node server.mjs` で、リレーだけが動く)
3. narou を先に配備してからリレーを配備する (`start.sh` が無いと起動しない)
4. ログに `取得リレーを起動しました` と `narou` の起動が出ることを確認する

- ポート: リレーがプラットフォームの `PORT` (127.0.0.1)、narou は `NAROU_RS_PORT`
  (既定 8080)。`start.sh` は同居時にプラットフォームの `PORT` を narou へ渡さない
- リレーの合言葉は Worker secret の `SORAHOST_PROXY_KEY` と**同じ値**にする。ずれると
  Worker からは 403 になり、取得できないサイトが黙って増える (Worker は元の取得結果へ
  戻るだけなので表面化しない)。`relay-deploy` ジョブが配備後に
  `scripts/sorahost-proxy/verify_auth.py` で一致を確認する。手元で確かめるなら:

  ```sh
  SORAHOST_PROXY_ENDPOINT=https://... SORAHOST_PROXY_KEY=... \
    python3 scripts/sorahost-proxy/verify_auth.py
  ```

- 注意: リレーの配備 (`sorahost-cli deploy`) はプロジェクトの起動コマンドを書き換える。
  同居させるときは常に `SORAHOST_RELAY_START` を設定しておくこと。

## 11. 使えない機能

SORAHOST のコンテナで動かすため、次は動かない (Web UI 側では 501 相当):

- `browser` / `folder` (デスクトップ操作)
- `send` / `mail` (端末送信・SMTP。設定の読み書きはできる)
- 外部 AozoraEpub3 (`aozoraepub3dir` は設定しない。Lite を同梱して使う)
- `narou_rs_login` (ブラウザのある端末で実行し、`narou login import` で取り込む)

## 12. 確認

```sh
curl -I https://<公開 URL>/                     # basic 認証のチャレンジが返る
curl -u admin:<password> https://<公開 URL>/api/novels/count
```

ブラウザで開き、ジョブの進捗 (WebSocket) が出ることを確認する。
配備直後は反映に数秒かかることがある (コンソールの `logs` で起動ログを確認できる)。
