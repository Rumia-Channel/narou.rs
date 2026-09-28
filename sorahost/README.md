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

## 1. ビルド

組み込み EPUB エンジン (Lite) を有効にしてビルドする。外部 AozoraEpub3 (Java) は
コンテナに置かない。

```sh
cargo build --release --features lite
```

Linux 向けの実行ファイルと、同梱のサイト定義 (`webnovel/`) を用意する。
`narou init` は実行ファイルの隣の `webnovel/` から定義をコピーするため、
**`webnovel/` を同梱しないとサイト定義が空になる**。

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

## 7. 更新

1. 新しい `narou_rs` をビルドして配置する
2. Pterodactyl からサーバーを再起動する

`self-update` は使わない (実行ファイルの置き場が読み取り専用になりうるため)。
`webnovel/*.yaml` を差し替えたときも再起動で反映される。

## 8. 使えない機能

SORAHOST のコンテナで動かすため、次は動かない (Web UI 側では 501 相当):

- `browser` / `folder` (デスクトップ操作)
- `send` / `mail` (端末送信・SMTP。設定の読み書きはできる)
- 外部 AozoraEpub3 (`aozoraepub3dir` は設定しない。Lite を同梱して使う)
- `narou_rs_login` (ブラウザのある端末で実行し、`narou login import` で取り込む)

## 9. 確認

```sh
curl -I https://narou.example.com/            # basic 認証のチャレンジが返る
curl -u admin:<password> https://narou.example.com/api/novels/count
```

ブラウザで開き、ジョブの進捗 (WebSocket) が出ることを確認する。
Cloudflare はサーバー再起動時に WebSocket を切ることがあるが、Web UI は
再接続する。
