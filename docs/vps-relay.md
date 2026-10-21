# VPS / Linux でのリレー運用（Rust バイナリ）

このリポジトリの現行デプロイ（`relay.rumia-ch.uk` / `narou-relay-vps`）向けの実運用手順は [`relay.md`](../relay.md) にまとめています。以下は一般的な手順です。

Cloudflare は同じ IP でも**クライアント指紋**で結果を変えることがあります。実測では、格安 VPS の共有 IPv4 NAT 出口から

- curl（HTTP/2）→ **403 `cf-mitigated: challenge`**
- wget + ブラウザ UA + サイト定義ヘッダ → **200 OK**

という組み合わせがありました。このページは、そのようなホストでリレーを **wget 経由**で動かす手順です。ホスト固有の分岐は増やさず、`NAROU_RELAY_FETCH` で取得クライアントを選びます。

リレーはワークスペース crate `relay/` のバイナリ `narou_relay`（Rust）です。**Node.js は不要**です。

## 1. 前提

- Linux（Debian / Ubuntu を想定、x86_64 / aarch64 はビルドした環境に合わせる）
- ビルドする場合は Rust ツールチェーン（`cargo`）。実行だけなら Linux バイナリをコピーしてもよい
- 実行時に **GNU wget かつ/または curl**、および `ca-certificates`
- このリポジトリ（少なくとも `relay/` と `scripts/relay/verify_auth.py`）。ビルドはリポジトリルートで行う
- 公開 IPv4 が無い VPS でも、[cloudflared](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) のアウトバウンド接続で公開できます

```sh
sudo apt-get update && sudo apt-get install -y git wget curl ca-certificates build-essential

# ビルドする場合のみ (rustup)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
rustup default stable
cargo --version

wget --version
curl --version
```

- ビルドだけなら Rust ツールチェーンで足りますが、リンカが必要なため `build-essential` を入れます
- curl / wget の両方が使えない場合、リレーは起動時に終了します（同梱 curl や curl.se からの CA ダウンロードはありません）

## 2. 配置

### 2.1 VPS 上でビルドする

```sh
git clone https://github.com/Rumia-Channel/narou.rs.git ~/narou.rs
cd ~/narou.rs
cargo build --release -p narou_relay
# => target/release/narou_relay
```

`main` に Rust リレーの crate が無い間は `develop` を使ってください（`test -f relay/Cargo.toml` で確認できる）。

### 2.2 手元でビルドした Linux バイナリをコピーする

VPS に Rust を入れたくない場合は、**同じ CPU アーキテクチャ・同じか古い glibc** でビルドしたバイナリをコピーします。
新しい glibc でビルドしたバイナリは古い VPS では動きません（`./narou_relay: /lib/x86_64-linux-gnu/libc.so.6: version 'GLIBC_2.xx' not found`）。

```sh
# 手元 (Linux / WSL) でビルドしてから
scp target/release/narou_relay user@vps:~/relay/narou_relay
ssh user@vps 'chmod +x ~/relay/narou_relay && ldd --version | head -1'
```

実行に必要なのはバイナリと `.env` だけです。運用中は `verify_auth.py` や更新のためリポジトリも置いておくと便利です。

## 3. 設定

`~/relay/.env`（パーミッション 600）:

```ini
NAROU_RELAY_KEY=ここに生成したキー
NAROU_RELAY_ALLOWED_HOSTS=syosetu.org,www.syosetu.org,h.syosetu.org
PORT=3000
NAROU_RELAY_HOST=127.0.0.1
NAROU_RELAY_FETCH=wget,curl
```

```sh
openssl rand -base64 32   # NAROU_RELAY_KEY の生成例
chmod 600 ~/relay/.env
```

環境変数:

| 変数 | 内容 |
| --- | --- |
| `NAROU_RELAY_KEY` | **必須**。`X-Proxy-Token` と照合する合言葉。未設定・空では起動しない |
| `NAROU_RELAY_ALLOWED_HOSTS` | 許可ホスト（カンマ区切りの完全一致）。未設定・空では取得をすべて拒否 |
| `PORT` / `SERVER_PORT` | 待受ポート。既定 3000。両方あるときは `PORT` が優先 |
| `NAROU_RELAY_HOST` | 待受アドレス。未指定なら `K_SERVICE` が無ければ `127.0.0.1`、あれば `0.0.0.0` |
| `NAROU_RELAY_FETCH` | 取得順（`curl` / `wget` のカンマ区切り）。既定 `curl,wget`。未知の名前は起動エラー |
| `NAROU_RELAY_CURL` / `NAROU_RELAY_WGET` | 実行ファイルのパス。未指定は PATH の `curl` / `wget` |
| `NAROU_RELAY_CACERT` | 任意。curl / wget に渡す CA バンドル（PEM）。起動時に検証する |
| `NAROU_RELAY_ENV_FILE` | 読み込む `.env` の明示指定 |

- `NAROU_RELAY_KEY` は Worker 側の同名 Secret と完全に一致させる（32 バイト以上のランダム値を推奨）
- `NAROU_RELAY_FETCH` の既定は `curl,wget`。wget を先頭にすると wget で取得する
- wget が見つからない場合は起動時に取得順から外れ、`/health` の `fetch` から消える（理由が stderr に出る）
- `.env` の読み込み順は `NAROU_RELAY_ENV_FILE` → 実行時 CWD の `./.env` → バイナリのディレクトリの `.env` → `$HOME/.env`。最初に見つかった 1 つだけを読み、既に設定済みの環境変数は上書きしない
- 読み込んだ `.env` は `narou relay: <path> を読み込みました (...)` と表示される。値そのものはログへ出ない
- `NAROU_RELAY_ALLOWED_HOSTS` は `*` や URL・ポート付きの値を拒否する（起動エラー）
- `NAROU_RELAY_CACERT`（任意）に更新済みの CA バンドルを指定すると、curl / wget がそのバンドルで TLS を検証する。
  リレーは起動時にバンドルを検査し、古いバンドル（2026-10-20 の eMudhra 中間 CA 侵害で失効したルートを含む）では
  `narou relay startup failed: CA バンドルを検証できません: ...` で起動しない。Linux なら
  `NAROU_RELAY_CACERT=/etc/ssl/certs/ca-certificates.crt` を推奨

## 4. 起動と確認

```sh
cd ~/relay
./narou_relay
# => narou relay listening on 127.0.0.1:3000 (fetch: wget:path, curl:path)
```

`(fetch: ...)` は解決済みの取得順と実行ファイルの由来です（`env` = 環境変数で明示、`path` = PATH から発見）。

別端末から:

```sh
curl -sS http://127.0.0.1:3000/health
# => {"ok":true,"fetch":"wget,curl"}
```

生成したキーで `/proxy` が使えることを確認するには `scripts/relay/verify_auth.py`（`NAROU_RELAY_ENDPOINT=http://127.0.0.1:3000`、`NAROU_RELAY_KEY=...`）を使います。

```sh
NAROU_RELAY_ENDPOINT=https://relay.example.com NAROU_RELAY_KEY='<.env と同じキー>' \
  python3 scripts/relay/verify_auth.py
```

> wget 経路は HTTPS の対象だけを扱います。接続先の DNS を検証したうえで **127.0.0.1 の使い捨て CONNECT プロキシ**が検証済み IP へ接続し、TLS の SNI・証明書検証は wget が元のホスト名に対して行います。curl の `--resolve` と同じ「接続先固定」を維持するための仕組みです。HTTP（非 TLS）の対象は curl 経路が扱います。

## 5. 常駐（systemd）

バイナリを `~/relay/narou_relay` に置き、`.env` を `~/relay/.env` に置いた例です。
リポジトリを丸ごと置いて `target/release/narou_relay` を直接指定しても構いません。

`/etc/systemd/system/narou-relay.service`:

```ini
[Unit]
Description=narou relay
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=narou
WorkingDirectory=/home/narou/relay
ExecStart=/home/narou/relay/narou_relay
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now narou-relay
sudo journalctl -u narou-relay -n 20
```

- `WorkingDirectory` に `.env` を置くとそのまま読まれる（別の場所なら `NAROU_RELAY_ENV_FILE` で指定）
- バイナリを更新したら `sudo systemctl restart narou-relay`

## 6. 公開（入口）

VPS に公開 IPv4 が無い場合の例です。**常用するなら 6.2 以降の非 Cloudflare トンネルを推奨**します。

### 6.1 cloudflared クイックトンネル（お試し）

```sh
cloudflared tunnel --no-autoupdate --url http://127.0.0.1:3000
# => https://xxxx.trycloudflare.com
```

- 起動のたびに URL が変わります。変更したら GitHub Secrets の `NAROU_RELAY_ENDPOINT` を更新して Worker を再配備
- Worker はリレーへ**生ソケット (`connect()`) で接続**します。Cloudflare フロントの URL に Worker から到達できるかは**実機で未検証**です。Worker のログが接続失敗になる場合は 6.2 へ

### 6.2 Cloudflare 以外のトンネル（推奨）

- Tailscale Funnel: `tailscale funnel 3000` → `https://<machine>.<tailnet>.ts.net`（無料・URL 固定）
- ngrok: `ngrok http 3000` → `https://xxxx.ngrok-free.app`

いずれも VPS からのアウトバウンド接続だけで公開でき、公開 IPv4 は不要です。

### 6.3 名前付き Cloudflare Tunnel（自分のドメイン）

```sh
cloudflared tunnel login
cloudflared tunnel create narou-relay
cloudflared tunnel route dns narou-relay relay.example.com
# 宛先をローカルのリレーにする (config.yml の ingress でもよい)
cloudflared tunnel run --url http://127.0.0.1:3000 narou-relay
```

常駐させる場合は `sudo cloudflared service install`（名前付きトンネルの場合）か systemd のユニットを使ってください。

`RELAY_DOMAIN=relay.example.com` を GitHub Environment `Cloudflare` の Secret に設定すると、Worker の `NAROU_RELAY_ENDPOINT` は自動で `https://relay.example.com` になります（`NAROU_RELAY_ENDPOINT` より優先）。名前付きトンネルは URL が固定されます（Worker からの到達性は 6.1 と同じ未検証事項）。

## 7. Worker 側

GitHub Secrets を更新して Worker を再配備します。

| Secret | 値 |
| --- | --- |
| `RELAY_DOMAIN` | リレーの公開ドメイン（例 `relay.example.com`）。設定時は `NAROU_RELAY_ENDPOINT` より優先 |
| `NAROU_RELAY_ENDPOINT` | 公開 URL（クイックトンネル等。`RELAY_DOMAIN` を使う場合は不要） |
| `NAROU_RELAY_KEY` | `.env` の `NAROU_RELAY_KEY` と同じ値 |

設定は本リポジトリの CI を使う場合のみ。`CloudFlare` 環境の Secret として登録します。

```sh
gh secret set RELAY_DOMAIN --env CloudFlare --body relay.example.com
gh secret set NAROU_RELAY_KEY --env CloudFlare --body '<.env と同じキー>'
```

独自に Worker を配備する場合は、その Worker に `NAROU_RELAY_ENDPOINT=https://relay.example.com` と `NAROU_RELAY_KEY` を直接設定します。

Web UI でハーメルンの作品を取得・更新して確認します。リレーが使われたかは、リレーのログに `narou relay wget: ...` / `narou relay curl: ...`（失敗時）が出ないこと、レスポンスの `via`（`wget` / `curl`）で判断できます。

## 8. トラブルシューティング

| 現象 | 確認 |
| --- | --- |
| `/health` の `fetch` が `curl,wget` | `NAROU_RELAY_FETCH` が未設定（既定順で動いている）。`.env` を直して再起動 |
| `/health` の `fetch` が `curl` だけ | wget が無いか実行できない。`wget --version`、`apt-get install -y wget` |
| 起動ログに wget が無い | `wget --version` を確認。PATH に無ければ `apt-get install wget` か `NAROU_RELAY_WGET` で指定 |
| `via=curl` になる（`wget,curl` 指定なのに） | `NAROU_RELAY_FETCH` に `wget` が入っているか、`wget` が実行可能か |
| 起動直後に終了する | `journalctl -u narou-relay -n 50 --no-pager`。`narou relay startup failed: NAROU_RELAY_KEY is required` / `invalid NAROU_RELAY_FETCH: ...` / `invalid host` / `CA バンドルを検証できません` など |
| 403 challenge が続く | その IP では wget でも拒否。別の回線（自宅回線など）が必要 |
| wget は直接 200 なのにリレー経由だけ 403 | リレーへ渡す JSON のヘッダ名が小文字化されていないか。HTTP/1.1 の wget は渡された綴りをそのまま送るため、`Sec-Fetch-*` 等が小文字だと Cloudflare が 403 にすることがある（2026-10-21 修正済み。Worker を更新する。リレー側の変更は不要） |
| TLS エラー（exit 5） | `ca-certificates` を入れる。VPS の時計ずれも確認 |
| 起動時に `CA バンドルを検証できません` | `NAROU_RELAY_CACERT` のバンドルが古い（2026-10-20 の eMudhra 中間 CA 侵害で失効したルートを含む）。`apt-get install --only-upgrade ca-certificates` で更新し、`NAROU_RELAY_CACERT=/etc/ssl/certs/ca-certificates.crt` を設定して再起動する |
| 接続できない | `ss -ltnp` で 127.0.0.1:3000、cloudflared のログ、DNS/TLS を確認 |
| `GLIBC_2.xx' not found` | バイナリをビルドした環境より VPS の glibc が古い。VPS 上でビルドするか、古い環境でビルドしたバイナリを使う |

## 9. セキュリティ

- `/proxy` は `X-Proxy-Token`（`NAROU_RELAY_KEY`）必須。`/health` だけが認証不要
- `.env` は 600。キーをログ・チャット・スクリーンショットへ出さない
- リレーは許可ホストのみ・DNS 公開 IP 検証・接続先固定・TLS 検証を行う。wget 経路もこの方針を維持する
- 3000 番を直接インターネットへ公開せず、トンネルまたはリバースプロキシ + TLS を使う
- VPS の共有 IPv4 NAT は他利用者と共有される。リレー用途をホストの規約で禁止していないか確認する
