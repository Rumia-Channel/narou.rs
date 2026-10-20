# 自宅回線リレー（Cloudflare 保護サイト向け）

Cloud Run / SORAHOST のリレーはデータセンター回線から取得するため、Cloudflare の managed challenge 配下にあるサイト（ハーメルン等）では **HTTP 403 (`cf-mitigated: challenge`)** のままになります。同じリレーを自宅回線で動かすと取得できます。このページはその手順です。

- 対象: `webnovel/syosetu.org.yaml`（ハーメルン）など、データセンター IP を拒否するサイト
- 既存のリレー実装（`scripts/relay/`）をそのまま使う。ホストに依存する分岐は追加しない
- Worker 側の設定は `NAROU_RELAY_ENDPOINT` / `NAROU_RELAY_KEY` の 2 つだけ（Cloud Run / SORAHOST と同じ）

## 1. なぜ自宅回線か（実測）

2026-10-20 に、同じクライアントで回線だけを変えてハーメルン (`https://syosetu.org/`) を取得した結果です。

| 実行場所 | curl (HTTP/2) | wget | Node fetch (HTTP/1.1) | Node HTTP/2 | 対照: ncode.syosetu.com |
| --- | --- | --- | --- | --- | --- |
| GitHub Actions（Azure のデータセンター） | 403 challenge | 403 challenge | 403 challenge | 403 challenge | 302（正常） |
| 自宅回線 | 200 | 未検証 | 403 challenge | 200 | 302（正常） |

- データセンター回線では **クライアントを変えても 403**（Cloudflare の判定は出口 IP が主因）
- 自宅回線ではクライアントによって結果が変わる（Node の HTTP/1.1 は 403、HTTP/2 と OS の curl は 200）
- したがって「Cloud Run の curl を wget に差し替える」などのクライアント変更では解決しない

補足: Pixiv はデータセンター経由でも取得できることを確認している（第三者プロキシ経由の取得例）。Cloud Run / SORAHOST のリレーは Pixiv 等の用途で引き続き利用できる。ハーメルンだけは自宅回線が必要。

## 2. 準備

自宅の Windows PC に以下を用意します。

- Node.js 24 以上（`node --version` で確認）
- このリポジトリのチェックアウト
- `cloudflared`（`cloudflared --version` で確認。無ければ https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/ から導入）
  - 無い場合でも `-NoTunnel` でローカル起動はできる

## 3. 設定ファイルを作る

`scripts/relay/.env.example` を `scripts/relay/.env` にコピーし、値を設定します。`.env` は秘密情報を含むため git 管理外です。

```powershell
Copy-Item scripts/relay/.env.example scripts/relay/.env
```

```ini
# 32 バイト以上のランダム値:
#   node -e "console.log(require('node:crypto').randomBytes(32).toString('base64url'))"
NAROU_RELAY_KEY=ここに生成したキー
# ハーメルン。R18 側 (h.syosetu.org) も必要なら含める
NAROU_RELAY_ALLOWED_HOSTS=syosetu.org,www.syosetu.org,h.syosetu.org
PORT=3000
```

- `NAROU_RELAY_ALLOWED_HOSTS` はカンマ区切りの完全一致。`*` や URL は不可
- 未設定・空の許可リストでは、認証が正しくても取得を拒否します
- キーは Worker 側の `NAROU_RELAY_KEY` と完全に同じにする

## 4. 起動する（クイックトンネル）

```powershell
pwsh -File scripts/relay/home-relay.ps1
```

スクリプトは次を行います。

1. `scripts/relay/server.mjs` を起動（`.env` を読み込む）
2. `cloudflared tunnel --url http://127.0.0.1:3000` でクイックトンネルを開始
3. 公開 URL（`https://xxxx.trycloudflare.com`）を表示し、`/health` の疎通を確認

表示例:

```
ローカル確認: http://127.0.0.1:3000/health
公開 URL:     https://xxxx.trycloudflare.com
公開 URL の疎通: OK (https://xxxx.trycloudflare.com/health)
```

- ポートを変える: `pwsh -File scripts/relay/home-relay.ps1 -Port 3100`
- トンネル無しでローカルだけ: `-NoTunnel`
- 終了は `Ctrl+C`。子プロセスも停止する

起動ログに `narou relay listening on 127.0.0.1:3000 (curl: path)` と出れば、OS の curl を使う状態です。Windows の curl は自宅回線のハーメルン取得に成功した実績があります。

## 5. Worker に設定する

GitHub リポジトリの Secrets を更新します（本リポジトリの CI を使う場合）。

| Secret | 値 |
| --- | --- |
| `NAROU_RELAY_ENDPOINT` | 表示された公開 URL（`https://xxxx.trycloudflare.com`） |
| `NAROU_RELAY_KEY` | `.env` の `NAROU_RELAY_KEY` と同じ値 |

- リポジトリ Secret に置く（本リポジトリの `CloudFlare` Environment にはリレー用 Secret は無い）
- 更新後、Worker を再配備する（`develop` へ push するか、Actions の `Platform portability` を `target=develop` で実行）
- Worker 側は `NAROU_RELAY_ENDPOINT` と `NAROU_RELAY_KEY` が両方揃ったときだけリレーを使う

## 6. 動作確認

1. ブラウザで `https://xxxx.trycloudflare.com/health` を開き、`{"ok":true}` を確認
2. Web UI でハーメルンの作品を取得・更新する
3. 失敗する場合、リレー自身のログ（`home-relay.ps1` を動かしたウィンドウ）と Worker のログを確認
   - `relay transport returned 403, keeping fetch 403` → 上流サイトがリレーの取得を拒否（自宅回線でも発生し得る。サイト定義のヘッダ・Cookie を確認）
   - `relay transport failed ... relay request rejected (relay HTTP 403): forbidden` → `NAROU_RELAY_KEY` の不一致
   - `malformed relay response` → Worker からリレーに届いていない（URL・トンネル・TLS を確認）

## 7. 常設化

クイックトンネルは**起動のたびに URL が変わる**ため、常用するなら名前付きトンネルか別サービスを使います。

### 7.1 Cloudflare の名前付きトンネル（同じ Cloudflare アカウントのドメイン）

```powershell
cloudflared tunnel login
cloudflared tunnel create narou-relay
cloudflared tunnel route dns narou-relay relay.example.com
```

`%USERPROFILE%\.cloudflared\config.yml`:

```yaml
tunnel: narou-relay
credentials-file: C:\Users\<user>\.cloudflared\<tunnel-id>.json
ingress:
  - hostname: relay.example.com
    service: http://127.0.0.1:3000
  - service: http_status:404
```

起動は 2 つに分けます。

```powershell
# 1) リレー本体（トンネルは張らない）
pwsh -File scripts/relay/home-relay.ps1 -NoTunnel
# 2) 別ウィンドウで名前付きトンネル
cloudflared tunnel run narou-relay
```

`NAROU_RELAY_ENDPOINT` は `https://relay.example.com` にします。

### 7.2 Cloudflare 以外のトンネル（Worker からの到達確認用）

Worker はリレーへ生ソケット (`connect()`) で接続します。Cloudflare のドメイン（`trycloudflare.com` / 自分のドメイン）へ Worker から接続できるかは実機で未検証です。Worker 側のログが「リレーに接続できない」形で失敗する場合は、Cloudflare 以外のトンネルを使ってください。

- Tailscale Funnel: `tailscale funnel 3000` → `https://<machine>.<tailnet>.ts.net`
- ngrok: `ngrok http 3000` → `https://xxxx.ngrok-free.app`

いずれも URL はサービス側の仕様に従います（無料枠の URL は固定されない場合があります）。

### 7.3 Windows の自動起動

タスクスケジューラで「ログオン時」に次を実行します。

```
pwsh -NoProfile -File <リポジトリ>\scripts\relay\home-relay.ps1
```

- 名前付きトンネルを使う場合は、リレー本体（`-NoTunnel`）と `cloudflared tunnel run` を別タスクに分ける
- 「ユーザーがログオンしていなくても実行する」を選ぶ場合は資格情報の保存が必要
- PC のスリープでトンネルが切れるため、スリープ設定も確認する

## 8. セキュリティと注意

- `/proxy` は `X-Proxy-Token`（`NAROU_RELAY_KEY`）が一致しないと 403。`/health` だけが認証不要
- リレーは許可ホストのみ取得し、DNS を検証して接続先を固定し、TLS 証明書を検証する。この安全策は他のホストと同じ
- `.env` は git 管理外。キーをログ・チャット・スクリーンショットへ出さない。漏れたら新しい値に作り直し、Worker とリレーの両方を更新する
- 自宅 PC が停止・スリープしている間は Worker から取得できない（既存の fetch 403 にフォールバックするだけで、データは壊れない）
- リバースプロキシ等で 3000 番ポートを直接インターネットへ公開しない。トンネルを使う

## 9. 結果の扱い（Worker のフォールバック）

Worker は対象サイトの取得が 403 になったときだけ、生ソケット → リレーの順に試します。リレーが落ちている・設定が無い場合は元の 403 のままです。自宅リレーを設定したからといって、通常のサイト取得経路は変わりません。

- Cloud Run / SORAHOST のリレー設定を消す必要はない。ただしハーメルンは自宅リレーが必要
- 併用したい場合は、両方が使うホストを同じ `NAROU_RELAY_ALLOWED_HOSTS` に置く
