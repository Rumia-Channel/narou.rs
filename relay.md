# VPS リレー設定手順（relay.rumia-ch.uk）

Cloudflare 保護サイト（ハーメルン等）を Workers から取得するための、VPS 上の取得リレーの設定手順です。
現在の構成と値は次のとおり。

| 項目 | 値 |
| --- | --- |
| 公開 URL | `https://relay.rumia-ch.uk` |
| Cloudflare Tunnel | `narou-relay-vps`（ID `ec95cf93-f801-49a2-9a2a-019385e1d0ea`） |
| リレー本体 | `~/narou.rs/scripts/relay/server.mjs`（Node.js 24、`NAROU_RELAY_FETCH=wget,node`） |
| 待受 | `127.0.0.1:3000` |
| Worker 側 | GitHub Environment `CloudFlare` の Secret `RELAY_DOMAIN=relay.rumia-ch.uk`（`NAROU_RELAY_KEY` は VPS の `.env` と一致） |

```text
Worker ──HTTPS──> relay.rumia-ch.uk (Cloudflare) ──tunnel──> cloudflared ──> 127.0.0.1:3000 (node server.mjs) ──wget──> 対象サイト
```

## 0. 今の状態の確認

```sh
# VPS 内
curl -sS http://127.0.0.1:3000/health        # 応答が無ければリレー未起動
# 外部（自宅 PC など）
curl -sS https://relay.rumia-ch.uk/health    # 502 は「リレー未起動」か「DNS 未ルーティング」
```

502 のままだと Worker からは使えません。以下を順に実施してください。

## 1. 依存パッケージ

```sh
sudo apt-get update
curl -fsSL https://deb.nodesource.com/setup_24.x | sudo -E bash -
sudo apt-get install -y nodejs wget ca-certificates git
node --version    # v24.x
wget --version
command -v cloudflared
```

## 2. リポジトリ

```sh
cd ~/narou.rs 2>/dev/null || git clone --depth 1 https://github.com/Rumia-Channel/narou.rs.git ~/narou.rs
cd ~/narou.rs && git pull
```

## 3. リレーの設定（.env）

```sh
cd ~/narou.rs/scripts/relay
cat > .env <<'EOF'
NAROU_RELAY_KEY=ここにキー
NAROU_RELAY_ALLOWED_HOSTS=syosetu.org,www.syosetu.org,h.syosetu.org
PORT=3000
NAROU_RELAY_HOST=127.0.0.1
NAROU_RELAY_FETCH=wget,node
EOF
chmod 600 .env
```

- `NAROU_RELAY_KEY` は **GitHub の `NAROU_RELAY_KEY` と同じ値**にする（GitHub の Secret 値は読み出せないため、手元に無ければ作り直す）
- キーを作り直す場合:

```sh
# VPS
node -e "console.log(require('node:crypto').randomBytes(32).toString('base64url'))"
# 自宅 PC（gh 認証済み）
gh secret set NAROU_RELAY_KEY --body '<上のキー>'
# その後 Worker を再配備（下記 8）
```

- 許可ホストを増やすときは `NAROU_RELAY_ALLOWED_HOSTS` にカンマ区切りで追加（例: `www.pixiv.net`）

## 4. リレーを起動してローカル確認

```sh
cd ~/narou.rs/scripts/relay
node server.mjs
# => narou relay listening on 127.0.0.1:3000 (fetch: wget:path, node)
```

別ターミナル:

```sh
curl -sS http://127.0.0.1:3000/health
# => {"ok":true,"fetch":"wget,node"}
```

`fetch` が `curl,node` になる場合は `NAROU_RELAY_FETCH` の設定漏れ、または wget が未導入です。

## 5. DNS をトンネルへ向ける

```sh
cloudflared tunnel route dns --overwrite-dns narou-relay-vps relay.rumia-ch.uk
```

## 6. 外部から確認

```sh
curl -sS https://relay.rumia-ch.uk/health
# => {"ok":true,"fetch":"wget,node"}
```

- 502 のまま: リレー（`node server.mjs`）が止まっている / cloudflared が止まっている / `route dns` 未実施
- 403 の JSON `{"error":"forbidden"}` が見える場合はリレーに届いています（キー不一致は Worker 側で発生）

## 7. 常駐化（systemd）

### 7.1 リレー

```sh
sudo tee /etc/systemd/system/narou-relay.service >/dev/null <<'EOF'
[Unit]
Description=narou relay
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=ubuntu
WorkingDirectory=/home/ubuntu/narou.rs/scripts/relay
ExecStart=/usr/bin/node /home/ubuntu/narou.rs/scripts/relay/server.mjs
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now narou-relay
curl -sS http://127.0.0.1:3000/health
journalctl -u narou-relay -n 20 --no-pager
```

手動で起動した `node server.mjs` は停止してから行う（`Ctrl+C`）。

### 7.2 cloudflared（名前付きトンネル）

`~/.cloudflared/config.yml`:

```yaml
tunnel: narou-relay-vps
credentials-file: /home/ubuntu/.cloudflared/ec95cf93-f801-49a2-9a2a-019385e1d0ea.json
ingress:
  - hostname: relay.rumia-ch.uk
    service: http://127.0.0.1:3000
  - service: http_status:404
```

systemd ユニット（`/etc/systemd/system/narou-tunnel.service`）:

```ini
[Unit]
Description=narou relay tunnel
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=ubuntu
Environment=HOME=/home/ubuntu
ExecStart=/usr/bin/cloudflared --no-autoupdate --config /home/ubuntu/.cloudflared/config.yml tunnel run narou-relay-vps
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now narou-tunnel
sudo systemctl status narou-tunnel --no-pager
```

- 手動で試すだけなら `cloudflared tunnel run --url http://127.0.0.1:3000 narou-relay-vps` でも可（上記 service と同時に起動しない）
- パッケージ版 cloudflared を使う場合は `sudo cloudflared service install` でも常駐化できる

## 8. Worker 側（GitHub）

```sh
# リレードメイン（設定済み）
gh secret set RELAY_DOMAIN --env CloudFlare --body relay.rumia-ch.uk
# キーを変更した場合のみ
gh secret set NAROU_RELAY_KEY --body '<VPS の .env と同じ値>'

# develop Worker の再配備
gh workflow run platform.yml --ref develop -f target=develop
```

- `RELAY_DOMAIN` があると `NAROU_RELAY_ENDPOINT=https://relay.rumia-ch.uk` が自動で使われる（`NAROU_RELAY_ENDPOINT` より優先）
- production Worker は main からの配備。反映には main への統合（リリースまたは指示）が必要

## 9. 動作確認（Worker）

- Web UI でハーメルンの作品を取得・更新する
- リレーのログ: `journalctl -u narou-relay -f`
  - `narou relay wget: ...` が出る場合は取得失敗（理由が `tls-verification` / `not-installed` / `failed` など）
  - 何も出ず 403 なら上流サイトが拒否
- Worker 側ログ:
  - `relay transport returned 403, keeping fetch 403` → 上流サイトの拒否
  - `relay transport failed ... relay request rejected (relay HTTP 403): forbidden` → キー不一致
  - `malformed relay response` / 接続失敗 → Worker からリレーに届いていない（この場合は Cloudflare フロントの名前付きトンネルではなく Tailscale Funnel / ngrok を検討）

## 10. 更新

```sh
cd ~/narou.rs && git pull
sudo systemctl restart narou-relay
```

## 11. トラブルシューティング

| 現象 | 確認 |
| --- | --- |
| 外部 `/health` が 502 | `curl http://127.0.0.1:3000/health`、`systemctl status narou-relay`、`systemctl status narou-tunnel`、`cloudflared tunnel info narou-relay-vps` |
| `/health` が `fetch: curl,node` | `.env` の `NAROU_RELAY_FETCH=wget,node`、`wget --version` |
| 403 `{"error":"forbidden"}` | VPS のキーと GitHub の `NAROU_RELAY_KEY` が不一致 |
| TLS エラー | `ca-certificates`、VPS の時刻（`timedatectl`） |
| 上流が 403 challenge | その出口 IP が Cloudflare に拒否されている。wget でも通らない場合は別回線（自宅）が必要 |
| TLS 証明書の不一致 | `relay.rumia-ch.uk` は `*.rumia-ch.uk` の範囲内。それより深い名前（例 `relay.narou.rumia-ch.uk`）は証明書がないため TLS が失敗する |

## 12. 注意

- 3000 番ポートを直接インターネットへ公開しない（公開はトンネル経由のみ）
- `.env` は 600。キーをログ・チャット・コミットへ出さない
- 旧 `relay.narou.rumia-ch.uk` の DNS レコードは削除してよい
- VPS の共有 IPv4 NAT が Cloudflare に拒否される場合、wget でも 403 になる。その場合は自宅回線のリレー（`docs/home-relay.md`）を使う
- 関連資料: `docs/vps-relay.md`（一般手順）、`docs/cloud-run-relay.md`、`docs/sorahost-relay.md`、`docs/home-relay.md`
