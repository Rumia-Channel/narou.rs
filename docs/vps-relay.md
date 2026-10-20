# VPS / Linux でのリレー運用（wget 経路）

Cloudflare は同じ IP でも**クライアント指紋**で結果を変えることがあります。実測では、格安 VPS の共有 IPv4 NAT 出口から

- curl（HTTP/2）→ **403 `cf-mitigated: challenge`**
- wget + ブラウザ UA + サイト定義ヘッダ → **200 OK**

という組み合わせがありました。このページは、そのようなホストでリレーを **wget 経由**で動かす手順です。ホスト固有の分岐は増やさず、`NAROU_RELAY_FETCH` で取得クライアントを選びます。

## 1. 前提

- Linux（Debian / Ubuntu を想定）
- Node.js 24 以上
- GNU wget（`wget --version` で確認）と `ca-certificates`
- このリポジトリの `scripts/relay/`（`server.mjs` / `transport.mjs` / `env.mjs` / `security.mjs` / `worker/` / `package.json`）
- 公開 IPv4 が無い VPS でも、[cloudflared](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) のアウトバウンド接続で公開できます

```sh
sudo apt-get update && sudo apt-get install -y wget ca-certificates
node --version   # v24 以上
wget --version
```

## 2. 配置

`scripts/relay/` をまるごと VPS に置きます（ビルド不要）。

```sh
scp -r scripts/relay user@vps:~/relay
```

## 3. 設定

`~/relay/.env`（パーミッション 600）:

```ini
NAROU_RELAY_KEY=ここに生成したキー
NAROU_RELAY_ALLOWED_HOSTS=syosetu.org,www.syosetu.org,h.syosetu.org
PORT=3000
NAROU_RELAY_HOST=127.0.0.1
NAROU_RELAY_FETCH=wget,node
```

- `NAROU_RELAY_FETCH` の既定は `curl,node`。wget を先頭にすると wget で取得する
- wget が見つからない場合は起動時に取得順から外れ、ログに理由が出る
- `NAROU_RELAY_KEY` は Worker 側の同名 Secret と完全に一致させる（32 バイト以上のランダム値）
- 設定は `scripts/relay/env.mjs` が `.env` から読む。値はログへ出ない

## 4. 起動と確認

```sh
cd ~/relay
node server.mjs
# => narou relay listening on 127.0.0.1:3000 (fetch: wget:path, node)
```

別端末から:

```sh
curl -sS http://127.0.0.1:3000/health
# => {"ok":true,"fetch":"wget,node"}
```

生成したキーで `/proxy` が使えることを確認するには `scripts/relay/verify_auth.py`（`NAROU_RELAY_ENDPOINT=http://127.0.0.1:3000`、`NAROU_RELAY_KEY=...`）を使います。

> wget 経路は、接続先の DNS を検証したうえで **127.0.0.1 の使い捨て CONNECT プロキシ**が検証済み IP へ接続し、TLS の SNI・証明書検証は wget が元のホスト名に対して行います。curl の `--resolve` と同じ「接続先固定」を維持するための仕組みです。HTTP（非 TLS）の対象は curl / Node に任せます。

## 5. 常駐（systemd）

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
ExecStart=/usr/bin/node /home/narou/relay/server.mjs
Restart=always
RestartSec=5
Environment=NODE_ENV=production

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now narou-relay
sudo journalctl -u narou-relay -n 20
```

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

Web UI でハーメルンの作品を取得・更新して確認します。リレーが使われたかは、リレーのログに `narou relay wget: ...`（失敗時）が出ないこと、レスポンスの `via`（`wget`）で判断できます。

## 8. トラブルシューティング

| 現象 | 確認 |
| --- | --- |
| `/health` の `fetch` が `curl,node` | `NAROU_RELAY_FETCH` が未設定。`.env` を直して再起動 |
| 起動ログに wget が無い | `wget --version` を確認。PATH に無ければ `apt-get install wget` |
| `via=node-http` になる | `NAROU_RELAY_FETCH` に `wget` が入っているか、`wget` が実行可能か |
| 403 challenge が続く | その IP では wget でも拒否。別の回線（自宅回線など）が必要 |
| TLS エラー（exit 5） | `ca-certificates` を入れる。VPS の時計ずれも確認 |
| 接続できない | `ss -ltnp` で 127.0.0.1:3000、cloudflared のログ、DNS/TLS を確認 |

## 9. セキュリティ

- `/proxy` は `X-Proxy-Token`（`NAROU_RELAY_KEY`）必須。`/health` だけが認証不要
- `.env` は 600。キーをログ・チャット・スクリーンショットへ出さない
- リレーは許可ホストのみ・DNS 公開 IP 検証・接続先固定・TLS 検証を行う。wget 経路もこの方針を維持する
- 3000 番を直接インターネットへ公開せず、トンネルまたはリバースプロキシ + TLS を使う
- VPS の共有 IPv4 NAT は他利用者と共有される。リレー用途をホストの規約で禁止していないか確認する
