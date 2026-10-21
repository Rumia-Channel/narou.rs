# VPS リレー設定手順（relay.rumia-ch.uk）

> **SORAHOST / Cloud Run (GCP) へのリレー配備は非推奨 (2026-10-21) で、Node 版リレーは削除しました。**
> リレーは Rust 版 `narou_relay` をこの VPS 手順または自宅回線 (`docs/home-relay.md`) で運用してください。

Cloudflare 保護サイト（ハーメルン等）を Workers から取得するための、VPS 上の取得リレーの設定手順です。
リレーはワークスペース crate `relay/` のバイナリ `narou_relay`（Rust）です。現在の構成と値は次のとおり。

| 項目 | 値 |
| --- | --- |
| 公開 URL | `https://relay.rumia-ch.uk` |
| Cloudflare Tunnel | `narou-relay-vps`（ID `ec95cf93-f801-49a2-9a2a-019385e1d0ea`） |
| リレー本体 | `~/narou.rs/target/release/narou_relay`（`cargo build --release -p narou_relay`、`NAROU_RELAY_FETCH=wget,curl`） |
| 待受 | `127.0.0.1:3000` |
| Worker 側 | GitHub Environment `CloudFlare` の Secret `RELAY_DOMAIN=relay.rumia-ch.uk`（`NAROU_RELAY_KEY` は VPS の `.env` と一致） |

```text
Worker ──HTTPS──> relay.rumia-ch.uk (Cloudflare) ──tunnel──> cloudflared ──> 127.0.0.1:3000 (narou_relay) ──wget──> 対象サイト
```

## 0. 今の状態の確認

```sh
# VPS 内
curl -sS http://127.0.0.1:3000/health        # 応答が無ければリレー未起動
# => {"ok":true,"fetch":"wget,curl"}
# 外部（自宅 PC など）
curl -sS https://relay.rumia-ch.uk/health    # 502 は「リレー未起動」か「DNS 未ルーティング」
```

502 のままだと Worker からは使えません。以下を順に実施してください。

## 1. 依存パッケージ

リレーのビルドに Rust ツールチェーン、実行に **システムの curl かつ/または wget** と CA 証明書を使います。
**Node.js は不要**です（Node 版リレーは 2026-10 に削除しました）。

```sh
sudo apt-get update
sudo apt-get install -y git wget curl ca-certificates build-essential

# Rust ツールチェーン (rustup)。ログインシェルに反映する
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
rustup default stable
cargo --version
wget --version
curl --version
command -v cloudflared
```

- ビルドだけなら Rust ツールチェーンで足りますが、リンカが必要なため `build-essential` を入れます
- 実行時に curl / wget の両方が使えない場合は、リレーは起動時に fail fast で終了します（同梱 curl はありません）
- `cloudflared` は未導入なら https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/ から導入する

## 2. リポジトリ

Rust リレーの crate (`relay/Cargo.toml`) が入ったブランチを使ってください。`main` への統合前は `develop` です。

```sh
cd ~/narou.rs 2>/dev/null || git clone https://github.com/Rumia-Channel/narou.rs.git ~/narou.rs
cd ~/narou.rs
git fetch origin
git pull

# relay/Cargo.toml が無い場合は develop へ切り替える (main はリリースまで追従が遅れる)
if [ ! -f relay/Cargo.toml ]; then
  git checkout develop 2>/dev/null || git checkout -b develop origin/develop
  git pull
fi
test -f relay/Cargo.toml && echo "Rust relay crate: OK"
git log --oneline -3
```

## 3. リレーの設定（.env）

`scripts/relay/.env` は systemd の `WorkingDirectory` から `./.env` として読まれます
（明示するなら `NAROU_RELAY_ENV_FILE`）。環境変数が既にある場合はそちらが優先されます。

```sh
cd ~/narou.rs/scripts/relay
cat > .env <<'EOF'
NAROU_RELAY_KEY=ここにキー
NAROU_RELAY_ALLOWED_HOSTS=syosetu.org,www.syosetu.org,h.syosetu.org
PORT=3000
NAROU_RELAY_HOST=127.0.0.1
NAROU_RELAY_FETCH=wget,curl
EOF
chmod 600 .env
```

- `NAROU_RELAY_KEY` は **GitHub の `NAROU_RELAY_KEY` と同じ値**にする（GitHub の Secret 値は読み出せないため、手元に無ければ作り直す）
- キーを作り直す場合:

```sh
# VPS
openssl rand -base64 32
# 自宅 PC（gh 認証済み）
gh secret set NAROU_RELAY_KEY --body '<上のキー>'
# その後 Worker を再配備（下記 8）
```

- `NAROU_RELAY_FETCH` は取得クライアントの順序。既定は `curl,wget`。この VPS では Cloudflare の challenge を避けるため `wget,curl` を先頭にする
  - 値は `curl` / `wget` のカンマ区切りのみ。未知の名前は起動エラー
  - `wget` が無い場合は起動時に取得順から自動で外れる（`/health` の `fetch` と起動ログで確認できる）
- 許可ホストを増やすときは `NAROU_RELAY_ALLOWED_HOSTS` にカンマ区切りで追加（例: `www.pixiv.net`）。`*` や URL は不可
- `.env` の値がログへ出ることはない

## 4. ビルドしてローカル確認

```sh
cd ~/narou.rs
cargo build --release -p narou_relay
# => target/release/narou_relay

cd scripts/relay
../../target/release/narou_relay
# => narou relay listening on 127.0.0.1:3000 (fetch: wget:path, curl:path)
```

`(fetch: ...)` の値は解決済みの取得順と、`NAROU_RELAY_CURL` / `NAROU_RELAY_WGET` の指定元
（`env` = 環境変数、`path` = PATH から発見）です。

別ターミナル:

```sh
curl -sS http://127.0.0.1:3000/health
# => {"ok":true,"fetch":"wget,curl"}
```

`fetch` が `curl,wget` になる場合は `NAROU_RELAY_FETCH` の設定漏れです。`curl` だけになる場合は
wget が未導入か実行できません（`sudo apt-get install -y wget`）。

起動ログが `narou relay startup failed: ...` で終了する場合は §11 を参照してください。

## 5. DNS をトンネルへ向ける

```sh
cloudflared tunnel route dns --overwrite-dns narou-relay-vps relay.rumia-ch.uk
```

## 6. 外部から確認

```sh
curl -sS https://relay.rumia-ch.uk/health
# => {"ok":true,"fetch":"wget,curl"}
```

- 502 のまま: リレー（`narou_relay`）が止まっている / cloudflared が止まっている / `route dns` 未実施
- 403 の JSON `{"error":"forbidden"}` が見える場合はリレーに届いています（キー不一致は Worker 側で発生）
- `/health` は認証不要、`/proxy` は `X-Proxy-Token` 必須

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
ExecStart=/home/ubuntu/narou.rs/target/release/narou_relay
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

- `WorkingDirectory` を `scripts/relay` にすると、そこにある `.env` がそのまま読まれる
- バイナリは `cargo build --release -p narou_relay` の出力（`target/release/narou_relay`）を直接指す。ビルド後に再起動する
- 手動で起動した `narou_relay` は停止してから行う（`Ctrl+C`）

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
  - `narou relay wget: ...` / `narou relay curl: ...` が出る場合は取得失敗（理由が `tls-verification` / `not-installed` / `failed` など）
  - `narou relay upstream status 403` は上流サイトの拒否（リレー自身の認証拒否とは別）
  - `narou relay proxy: authentication failed (X-Proxy-Token mismatch)` はキー不一致
  - 何も出ず 403 なら上流サイトが拒否
- Worker 側ログ:
  - `relay transport returned 403, keeping fetch 403` → 上流サイトの拒否
  - `relay transport failed ... relay request rejected (relay HTTP 403): forbidden` → キー不一致
  - `malformed relay response` / 接続失敗 → Worker からリレーに届いていない（この場合は Cloudflare フロントの名前付きトンネルではなく Tailscale Funnel / ngrok を検討）

## 10. 更新

```sh
cd ~/narou.rs && git pull && cargo build --release -p narou_relay && sudo systemctl restart narou-relay
```

ビルドが失敗した場合は、古いバイナリのまま `restart` しない（更新前に `cargo build` の成否を確認する）。

## 11. トラブルシューティング

| 現象 | 確認 |
| --- | --- |
| 外部 `/health` が 502 | `curl http://127.0.0.1:3000/health`、`systemctl status narou-relay`、`systemctl status narou-tunnel`、`cloudflared tunnel info narou-relay-vps` |
| `/health` の `fetch` が `curl,wget` | `.env` の `NAROU_RELAY_FETCH=wget,curl` が未設定（既定順で動いている） |
| `/health` の `fetch` が `curl` だけ | `wget --version` を確認し、`sudo apt-get install -y wget` |
| 起動ログの `(fetch: ...)` に wget が無い | wget が PATH に無いか実行不可。`NAROU_RELAY_WGET` でパスを明示することもできる |
| 起動直後に終了する | `journalctl -u narou-relay -n 50 --no-pager` の `narou relay startup failed: ...` を確認。`NAROU_RELAY_KEY` 未設定、`NAROU_RELAY_FETCH` の不正名、CA バンドル、curl/wget が両方使えない、のいずれか |
| 403 `{"error":"forbidden"}` | VPS のキーと GitHub の `NAROU_RELAY_KEY` が不一致 |
| TLS エラー | `ca-certificates`、VPS の時刻（`timedatectl`） |
| 上流が 403 challenge | その出口 IP が Cloudflare に拒否されている。wget でも通らない場合は別回線（自宅）が必要 |
| 起動時に `CA バンドルを検証できません` | `NAROU_RELAY_CACERT` のバンドルが古い（2026-10-20 の eMudhra 中間 CA 侵害で失効したルートを含む）。`ca-certificates` を更新して再起動する（下の「CA バンドルの侵害対応」参照） |
| VPS の wget は直接 200 なのにリレー経由だけ 403 challenge | リレー呼び出し JSON のヘッダ名が小文字化されていないか。HTTP/1.1 の wget は渡された綴りをそのまま送るため、`Sec-Fetch-*` 等が小文字だと Cloudflare がブラウザ相当と見なさない。2026-10-21 に `platform::relay` を修正済み。Worker を更新して再配備する（リレー側の変更は不要） |
| TLS 証明書の不一致 | `relay.rumia-ch.uk` は `*.rumia-ch.uk` の範囲内。それより深い名前（例 `relay.narou.rumia-ch.uk`）は証明書がないため TLS が失敗する |

### CA バンドルの侵害対応（2026-10、eMudhra 中間 CA）

2026-10-20 に eMudhra の 8 つの中間 CA の秘密鍵が復元可能だったとして失効しました。侵害された
中間 CA は `AAA Certificate Services`（Sectigo）と `emSign ECC Root CA - G3` などへ連鎖するため、
失効情報を扱わないクライアントは任意ホストへの証明書偽造（MITM）を受け入れてしまいます。

このリポジトリの対応:

- Rust リレーは TLS 検証を curl / wget に委ね、`NAROU_RELAY_CACERT` を指定した場合は
  そのバンドルが検証の根拠になる。`relay/src/ca.rs` の SHA-256 指紋デナイリストで既知の
  侵害ルートを検出し、起動時に fail closed で停止する。
  `NAROU_RELAY_CACERT` は curl の `--cacert` / wget の `--ca-certificate` に渡り、
  クライアント既定の CA ストアを置き換える（指定しない場合は各クライアントの既定）。
- 起動時に `NAROU_RELAY_CACERT` のバンドルを検証する。古いバンドルのままでは起動しない。
- Rust 依存の `rustls-webpki` は修正版 0.103.16（失効した中間 CA の SPKI を拒否）へ、`rustls` は
  0.23.46 へ更新（現構成の native TLS は native-tls / OS ストアなので未使用だが監査対象。
  `webpki-roots` / `webpki-root-certs` は依存に無い）。

VPS の手順:

```sh
# 1) OS の CA を更新（例: Ubuntu 20260816ubuntu0.24.04.1 / Debian 20260816+nmu1 以降）
sudo apt-get update && sudo apt-get install --only-upgrade ca-certificates
# 2) システムバンドルに侵害ルートが残っていないことを確認（0 なら OK）
grep -c -E 'AAA Certificate Services|emSign ECC Root CA - G3' /etc/ssl/certs/ca-certificates.crt || true
# 3) リレーを更新し、システムバンドルを明示して再起動
cd ~/narou.rs && git pull && cargo build --release -p narou_relay
printf '\nNAROU_RELAY_CACERT=/etc/ssl/certs/ca-certificates.crt\n' >> scripts/relay/.env
sudo systemctl restart narou-relay
```

`NAROU_RELAY_CACERT` を設定すると curl / wget の両方がそのバンドルで検証します。侵害ルートが
残っていれば `narou relay startup failed: CA バンドルを検証できません: ...` で停止します。

- Native アプリの TLS は OS の証明書ストア（Windows Schannel / Linux OpenSSL / macOS）を使うため、
  OS の更新（Windows Update、`ca-certificates` パッケージ等）が必要です。

## 12. 注意

- 3000 番ポートを直接インターネットへ公開しない（公開はトンネル経由のみ）
- `.env` は 600。キーをログ・チャット・コミットへ出さない
- 旧 `relay.narou.rumia-ch.uk` の DNS レコードは削除してよい
- VPS の共有 IPv4 NAT が Cloudflare に拒否される場合、wget でも 403 になる。その場合は自宅回線のリレー（`docs/home-relay.md`）を使う
- 旧 Cloud Run / SORAHOST 向けの配備スクリプト（`bootstrap.mjs` / `deploy-sorahost.mjs` / `relay.js` / `worker/`）と
  `docs/cloud-run-relay.md` / `docs/sorahost-relay.md` は削除済み。Node.js の導入も不要
- 関連資料: `docs/vps-relay.md`（一般手順）、`docs/home-relay.md`（自宅回線）
