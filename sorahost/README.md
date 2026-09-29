# SORAHOST で narou.rs を動かす

SORAHOST (Pterodactyl) のコンテナで native 版 `narou_rs` を常駐させ、Web UI を
Cloudflare Tunnel 経由で公開する手順。**Workers は使わない** (無料枠の CPU・
接続数・ポート制約を受けない)。

```
Browser ──https──▶ Cloudflare (TLS / WAF / Tunnel)
                      │ cloudflared が張る外向き接続 (7844/TCP+UDP)
                      ▼
              SORAHOST コンテナ内
                cloudflared ──▶ http://127.0.0.1:8080 ──▶ narou_rs web
                                                  │
                                                  └─ 挿絵だけ S3 (Wasabi など) へ
```

- 受信ポートは不要。SORAHOST の割当ポート番号 (`nagoya.sorahost.net:50071` 等) には依存しない
- メタデータ・本文はライブラリ内の SQLite (`.narou/db.sqlite`)、挿絵だけ S3 に置く
- アプリはループバックにしかバインドしないので、外部から直接叩けない

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

配置先 (コンテナ内):

```
/home/container/narou/
  narou_rs            実行ファイル (chmod +x)
  webnovel/*.yaml     同梱のサイト定義
  start.sh            このリポジトリの sorahost/start.sh
  library/            .narou / 小説データ / webnovel (初回起動で作られる)
  bin/cloudflared     start.sh が取得 (手元で置いてもよい)
  .cloudflared-token  トンネルのトークン (chmod 600)
```

`/home/container` 配下は再デプロイで消えない。デプロイ用のディレクトリを
使い捨ての置き場にしないこと。

## 2. Cloudflare Tunnel

CI から配備する場合 (§9)、tunnel と DNS は `sorahost/ci/deploy_sorahost.py` が
Cloudflare API で作る。手動で作るときの手順は次のとおり。

1. Cloudflare ダッシュボード → **Networking → Tunnels → Create Tunnel**
   (コネクタは `cloudflared` を選ぶ)
2. 表示される**トークン**を控える (再表示できない。失くしたら rotate)
3. **Public hostname** を追加する
   - Hostname: `narou.example.com` (自分のドメイン)
   - Service: `http://127.0.0.1:8080` (`NAROU_RS_PORT` を変えたら合わせる)
4. DNS レコードはダッシュボードが自動作成する (CNAME → `<UUID>.cfargotunnel.com`)
5. **SSL/TLS モードを Off 以外にする** (Off だと WebSocket が通らない)
6. **Network → WebSockets を On** にする

トークンはコンテナ内の `/home/container/narou/.cloudflared-token` に置く。

```sh
printf '%s' '<TOKEN>' > /home/container/narou/.cloudflared-token
chmod 600 /home/container/narou/.cloudflared-token
```

## 3. 環境変数

Pterodactyl の Startup 変数、またはコンテナ内の環境変数として設定する
(ひな形は `sorahost/narou.env.example`)。

| 変数 | 用途 |
| --- | --- |
| `NAROU_WEB_PASSWORD` | Web UI の basic 認証パスワード (**必須**。公開されるため) |
| `NAROU_WEB_USER` | basic 認証のユーザ名 (既定は設定しない = narou 側の既定) |
| `NAROU_RS_ASSET_BACKEND` | `s3` で挿絵を S3 へ。未設定はローカル保存 |
| `S3_ENDPOINT` / `S3_BUCKET` / `S3_REGION` | 接続先 (Wasabi は `https://s3.<region>.wasabisys.com`) |
| `S3_PREFIX` | バケット内の接頭辞 (例 `narou/library`) |
| `S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY` | 資格情報 |
| `NAROU_RS_PORT` | ローカルの待受ポート (既定 8080) |
| `NAROU_RELAY` | `1` で Worker 用の取得リレーも同じコンテナで起動する (§10) |
| `SORAHOST_PROXY_KEY` | リレーの合言葉 (§10)。Worker secret の同名値と同じにする |
| `TUNNEL_TOKEN_FILE` | トークンのパス (既定 `$NAROU_RS_ROOT/.cloudflared-token`) |

`narou setting s3.endpoint=...` のように設定ファイル側へ書いてもよい
(環境変数があるときは環境変数が優先)。

## 4. 起動

Pterodactyl の起動コマンドに次を設定する。

```
bash /home/container/narou/start.sh
```

初回起動でライブラリを作り、`storage-backend` を `sqlite` に切り替え、
`server-bind=127.0.0.1` / `server-reverse-proxy.enable=true` を設定する。

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
| cloudflared | 約 16 MB (Cloudflare 公式の systemd 例) |
| SQLite | ディスク backed (ページキャッシュは既定で 2MB 程度) |

`start.sh` は `concurrency=true` を設定する (DL/update と convert/send を別レーンで並行)。
常駐 + 2 ジョブ + トンネル + OS のピークは次のとおり:

| メモリ割当 | 通常 (2 ジョブが普通サイズ) | スパイク含む最悪ケース |
| --- | --- | --- |
| 256 MB | 約 200 MB (余裕 50〜60 MB) | 超過し得る (うごイラ + 大きい EPUB が重なるとき) |
| **384 MB** | 約 200 MB (余裕 180 MB) | 約 300〜320 MB で収まる見込み |

スパイク込みでも安全側に倒すなら **384 MB** を推奨。256 MB でも通常運用は収まるが、
画像の多い作品を同時に 2 本処理するときだけ余裕がなくなる。

- 小説単位の排他 (`.narou/lock.yaml`) があるので、同じ小説が両レーンで同時に走らない
- スパイクが大きいのは うごイラ (APNG) の組み立て (実例 19 フレームで約 56MB) と
  挿絵入りの大きい EPUB (S3 は 1 オブジェクトを最大 64MB まで一括で読み書きする)
- OOM で落ちる場合は `narou setting concurrency=false` にするか、メモリ割当を増やす

## 8. 更新

1. 新しい `narou_rs` をビルドして配置する
2. Pterodactyl からサーバーを再起動する

`self-update` は使わない (実行ファイルの置き場が読み取り専用になりうるため)。
`webnovel/*.yaml` を差し替えたときも再起動で反映される。

## 9. CI からの自動配備

`develop` へ push する (または Actions の **Deploy SORAHOST** を手で回す) と、
ビルドから配備まで自動で流れる。**動くのは Repository variable の
`SORAHOST` が `T` のときだけ** (未設定・`F` なら何もせず終わる)。

`SORAHOST` が `T` の間は **Cloudflare Workers 側の CI も止まる** (`.github/workflows/platform.yml`
の wasm / worker / worker-contract / relay-deploy / worker-deploy-develop /
worker-deploy-production が skip される)。配備先を Worker に戻すときは `SORAHOST` を
`F` にするか消す (native / native-gpl / license のテストは配備先に依存しないので常に走る)。

流れ:

1. `.github/workflows/build-linux.yml` が Linux バイナリを作る (GitHub ホストの runner)
2. Cloudflare API で tunnel を用意する (無ければ作成・あれば再利用)
   - 公開ホスト名 → `http://127.0.0.1:8080` の ingress
   - `<ホスト名>` の CNAME を `<tunnel-id>.cfargotunnel.com` に向ける (proxied)
3. SFTP でバンドルを転送する (`narou_rs` 3 種 + `webnovel/` + `preset/` + `start.sh`)
4. tunnel トークンを `.cloudflared-token` へ書く (値が変わったときだけ)
5. パネルの API が設定されていればサーバーを再起動する
6. `SORAHOST_SMOKE_URL` があれば応答を確認する

### Environment `SORAHOST` に置く値

| 種別 | 名前 | 用途 |
| --- | --- | --- |
| var | `SORAHOST_HOST` | SFTP ホスト (パネルのアドレス) |
| var | `SORAHOST_USER` | SFTP ユーザ |
| var | `SORAHOST_TUNNEL_HOSTNAME` | 公開ホスト名 (例 `narou.example.com`) |
| secret | `SORAHOST_PASSWORD` | SFTP パスワード |
| secret | `SORAHOST_SSH_KEY` | 秘密鍵 (パスワードの代わり。任意) |
| var | `SORAHOST_TUNNEL_NAME` | tunnel 名 (任意。既定 `narou-sorahost`) |
| var | `SORAHOST_SFTP_PORT` | SFTP ポート (任意。既定 2022) |
| var | `SORAHOST_REMOTE_DIR` | 配置先 (任意。既定 `/narou` = コンテナの `~/narou`) |
| var | `SORAHOST_SERVICE_PORT` | コンテナ内の待受ポート (任意。既定 8080) |
| var | `SORAHOST_PANEL_URL` | パネル URL (任意。再起動に使う) |
| var | `SORAHOST_SERVER_ID` | サーバー ID (任意。再起動に使う) |
| secret | `SORAHOST_CLIENT_API_KEY` | パネルのクライアント API キー (任意) |
| var | `SORAHOST_SMOKE_URL` | 配備後の確認 URL (任意) |

Cloudflare の値も **この環境に置く** (`secrets.CLOUDFLARE_API_TOKEN` と
`vars.CLOUDFLARE_ACCOUNT_ID`。環境 `Cloudflare` と同じものでよい)。
API トークンの権限は **Account: Cloudflare Tunnel Edit** と **Zone: DNS Edit**。

> なぜ環境を分けないのか: job が使える Environment は 1 つだけで、かつこの
> リポジトリは public のため Actions の job 出力が誰でも読める。tunnel トークンを
> job 間で渡す設計にすると秘密が公開されるので、1 つの job の中で完結させている。

再起動の 3 値 (`SORAHOST_PANEL_URL` / `SORAHOST_SERVER_ID` /
`SORAHOST_CLIENT_API_KEY`) を入れない場合は、配備後にパネルから手で再起動する。

> Worker 側の配備 (`.github/workflows/platform.yml`) はこの変数の影響を受けない。
> SORAHOST へ一本化するときは、同ワークフローの配備ジョブに
> `if: vars.SORAHOST != 'T'` を足すか、ワークフロー自体を無効化する。
>
> `SORAHOST` 環境に**同名の `SORAHOST` 変数を置かないこと** (Repository variable
> の判定と混ざる)。

## 10. Worker の取得リレーと同じサーバーで動かす

Worker の踏み台 (`scripts/sorahost-proxy/`) と同じ SORAHOST で narou も動かせる。
PteWorker は **1 プロジェクト = 起動コマンド 1 つ** なので、起動コマンドを narou の
`start.sh` に寄せ、リレーはそこから起動する。

1. パネルの `.env` に `NAROU_RELAY=1` を足す (`SORAHOST_PROXY_KEY` は既にある値のまま。
   narou 側の `NAROU_*` / `S3_*` と同居してよい)。
2. リレーの起動コマンドを差し替える。CI から配備している場合は Repository variable
   `SORAHOST_RELAY_START` = `bash narou/start.sh` を入れる (未設定なら従来どおり
   `node server.mjs` で、リレーだけが動く)。
3. narou を先に配備してからリレーを配備する (`narou/start.sh` が無いと起動しない)。
4. 再起動し、ログに `取得リレーを起動しました` と `narou_rs` の起動が出ることを確認する。

- ポート: リレーはプラットフォームの `PORT` (127.0.0.1)、narou は `NAROU_RS_PORT`
  (既定 8080)。`start.sh` はプラットフォームの `PORT` を上書きしない。
- リレーの合言葉は Worker secret の `SORAHOST_PROXY_KEY` と**同じ値**にする。ずれると
  Worker からは 403 になり、取得できないサイトが黙って増える (Worker は元の取得結果へ
  戻るだけなので表面化しない)。`relay-deploy` ジョブが配備後に
  `scripts/sorahost-proxy/verify_auth.py` で一致を確認する。手元で確かめるなら:

  ```sh
  SORAHOST_PROXY_ENDPOINT=https://... SORAHOST_PROXY_KEY=... \
    python3 scripts/sorahost-proxy/verify_auth.py
  ```

- 別サーバーのままにする場合は何もしなくてよい (`NAROU_RELAY` 未設定なら `start.sh` は
  リレーを起動しない)。
- 注意: リレーの配備 (`sorahost-cli deploy`) はプロジェクトの起動コマンドを書き換える。
  共有するときは `SORAHOST_RELAY_START` を常に設定しておくこと。
- 注意: リレーの配備が `narou/` を消さないことは未確認 (`include` の 3 ファイルだけを
  送る作りなので残る想定)。初回は配備後に `narou/narou_rs` が残っているかを見る。

## 11. 使えない機能

SORAHOST のコンテナで動かすため、次は動かない (Web UI 側では 501 相当):

- `browser` / `folder` (デスクトップ操作)
- `send` / `mail` (端末送信・SMTP。設定の読み書きはできる)
- 外部 AozoraEpub3 (`aozoraepub3dir` は設定しない。Lite を同梱して使う)
- `narou_rs_login` (ブラウザのある端末で実行し、`narou login import` で取り込む)

## 12. 確認

```sh
curl -I https://narou.example.com/            # basic 認証のチャレンジが返る
curl -u admin:<password> https://narou.example.com/api/novels/count
```

ブラウザで開き、ジョブの進捗 (WebSocket) が出ることを確認する。
Cloudflare はサーバー再起動時に WebSocket を切ることがあるが、Web UI は
再接続する。
