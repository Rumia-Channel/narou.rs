# 自宅回線リレー（Cloudflare 保護サイト向け）

> **Cloud Run / SORAHOST へのリレー配備は 2026-10-21 に非推奨となり、Node 版リレーは削除しました。**
> リレーは Rust 版 `narou_relay` を自宅回線（このページ）または VPS
> (`relay.md` / `docs/vps-relay.md`) で運用してください。以下は経緯と比較の記録を含みます。

データセンター回線のリレーは、Cloudflare の managed challenge 配下にあるサイト（ハーメルン等）では **HTTP 403 (`cf-mitigated: challenge`)** のままになります。同じリレーを自宅回線で動かすと取得できます。このページはその手順です。

- 対象: `webnovel/syosetu.org.yaml`（ハーメルン）など、データセンター IP を拒否するサイト
- リレーは Rust バイナリ `narou_relay`（ワークスペース crate `relay/`）。Node.js は不要
- Worker 側の設定は `NAROU_RELAY_ENDPOINT` / `NAROU_RELAY_KEY` の 2 つだけ

## 1. なぜ自宅回線か（実測）

2026-10-20 に、同じクライアントで回線だけを変えてハーメルン (`https://syosetu.org/`) を取得した結果です。

| 実行場所 | curl (HTTP/2) | wget | Node fetch (HTTP/1.1) | Node HTTP/2 | 対照: ncode.syosetu.com |
| --- | --- | --- | --- | --- | --- |
| GitHub Actions（Azure のデータセンター） | 403 challenge | 403 challenge | 403 challenge | 403 challenge | 302（正常） |
| 自宅回線 | 200 | 200（ブラウザ UA。既定 UA は 403） | 403 challenge | 200 | 302（正常） |

- データセンター回線では **クライアントを変えても 403**（Cloudflare の判定は出口 IP が主因）
- 自宅回線ではクライアントによって結果が変わる（Node の HTTP/1.1 は 403、HTTP/2 と OS の curl は 200）
- したがって「Cloud Run の curl を wget に差し替える」などのクライアント変更では解決しない
- SORAHOST 上の Native でも取得できなかった（2026-10-20 利用者確認）。Cloud Run / SORAHOST の両方で、クライアントや UA を変えても 403 のまま
- 上の「Node fetch / Node HTTP/2」は当時リレーが Node 版で、Node の HTTP クライアントも取得経路の候補だった頃の実測。現在の Rust 版リレーの取得経路は curl / wget の 2 つだけ

補足: Pixiv はデータセンター経由でも取得できることを確認している（第三者プロキシ経由の取得例）。Cloud Run / SORAHOST のリレーは削除済みだが、データセンター回線でも取得できるサイトがあるため、VPS リレーはそうした用途にも使える。ハーメルンだけは自宅回線が必要。

### 1.1 実際に送られていたリクエスト（2026-10-20 捕捉）

リレー（Worker のヘッダを curl / wget が送る。VPS リレーは `NAROU_RELAY_FETCH=wget,curl`、自宅リレーは既定の `curl,wget`）:

```text
Host
Accept-Encoding: deflate, gzip        ← curl --compressed が追加（curl 経路は HTTP/2）
Cookie: over18=off
Accept / Accept-Language / Sec-Fetch-* / Upgrade-Insecure-Requests（サイト定義どおり）
User-Agent: ... Firefox/157 or 158     ← Worker が追加（ua_generator。native と共通）
```

wget 経路では `Accept-Encoding: identity` / `Connection: Keep-Alive` / `Proxy-Connection: Keep-Alive` が付き、HTTP/1.1 になる（curl 経路とはヘッダが異なる）。HTTP/1.1 ではヘッダ名の綴りもそのまま送られるため、リレーはサイト定義の `Sec-Fetch-*` 等の大文字小文字を保つ（小文字化すると Cloudflare が 403 にする。2026-10-21 修正）。

ネイティブ（Linux の wget 段）:

```text
Host
User-Agent: ... Firefox/157.0 or 158.0   ← ua_generator
Accept（サイト定義の値）/ Accept-Encoding: gzip / Connection: keep-alive
Accept-Language / Sec-Fetch-* / Upgrade-Insecure-Requests / Cookie: over18=off
```

同じ回線（自宅）で UA とクライアントを変えた結果:

| 組み合わせ | 自宅回線 |
| --- | --- |
| wget + ブラウザ UA（Firefox 131/158、Chrome 155）+ サイトヘッダ | 200 |
| wget 既定 UA（`Wget/1.25.0`） | 403 challenge |
| wget UA のみ（ヘッダ無し） | 403/200 混在 |
| curl（Schannel） | 200 |

ネイティブと同じヘッダ・UA をデータセンター（GitHub Actions / Azure）から投げた結果:

| 組み合わせ | データセンター |
| --- | --- |
| wget + Firefox 131/158 + ネイティブヘッダ | 403 challenge |
| curl（HTTP/2）+ Firefox 158 + ネイティブヘッダ | 403 challenge |
| Node HTTP/1.1 / HTTP/2 + Firefox 158 | 403 challenge |
| 対照: ncode.syosetu.com | 302 |

→ リクエスト内容をネイティブと同一にしても、データセンター回線では 403。**出口 IP が支配的**で、UA やクライアントの修正では解決しない。

## 2. 準備

自宅の Windows PC に以下を用意します。

- Rust ツールチェーン（`cargo --version` で確認。無ければ https://rustup.rs/ から）
- このリポジトリのチェックアウト（ワークスペースの `relay/` crate を含むブランチ）
- `cloudflared`（`cloudflared --version` で確認。無ければ https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/ から導入）
  - 無い場合でも `-NoTunnel` でローカル起動はできる

Node.js は不要です。初回起動時にバイナリが無ければ `home-relay.ps1` が
`cargo build --release -p narou_relay` を実行します（2 回目以降は `target/release/narou_relay.exe` を再利用。`-Rebuild` で再ビルド）。

## 3. 設定ファイルを作る

`scripts/relay/.env.example` を `scripts/relay/.env` にコピーし、値を設定します。`.env` は秘密情報を含むため git 管理外です。
`home-relay.ps1` は `NAROU_RELAY_ENV_FILE` でこの `.env` をリレーに渡します。

```powershell
Copy-Item scripts/relay/.env.example scripts/relay/.env
```

Windows では `.env` のアクセス権を現在のユーザーだけに制限します。

```powershell
icacls scripts\relay\.env /inheritance:r /grant:r "$($env:USERNAME):F"
```

```ini
# 32 バイト以上のランダム値:
#   openssl rand -base64 32
#   PowerShell 7: [Convert]::ToBase64String([System.Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
# (Get-Random / System.Random は暗号学的乱数ではないため鍵に使わない)
NAROU_RELAY_KEY=ここに生成したキー
# ハーメルン。R18 側 (h.syosetu.org) も必要なら含める
NAROU_RELAY_ALLOWED_HOSTS=syosetu.org,www.syosetu.org,h.syosetu.org
PORT=3000
```

- `NAROU_RELAY_ALLOWED_HOSTS` はカンマ区切りの完全一致。`*` や URL は不可（起動エラー）
- 未設定・空の許可リストでは、認証が正しくても取得を拒否します
- キーは Worker 側の `NAROU_RELAY_KEY` と完全に同じにする
- 環境変数 `NAROU_RELAY_KEY` が設定されている場合は `.env` より優先されます。ローテーション直後は起動ログの `を読み込みました (...)` に `NAROU_RELAY_KEY` が含まれるか（＝`.env` の値が使われたか）を確認してください
- 取得順は既定で `curl,wget`。wget が無ければ起動時に取得順から外れて `/health` の `fetch` から消える

## 4. 起動する（クイックトンネル）

```powershell
pwsh -File scripts/relay/home-relay.ps1
```

スクリプトは次を行います。

1. リレーのバイナリを用意（`-BinaryPath` → `target/release/narou_relay.exe` → 無ければ `cargo build --release -p narou_relay`）
2. `.env` を検証して `NAROU_RELAY_ENV_FILE` に設定し、`narou_relay` を起動
3. `cloudflared tunnel --url http://127.0.0.1:3000` でクイックトンネルを開始
4. 公開 URL（`https://xxxx.trycloudflare.com`）を表示し、`/health` の疎通を確認

表示例:

```
ローカル確認: http://127.0.0.1:3000/health
公開 URL:     https://xxxx.trycloudflare.com
公開 URL の疎通: OK (https://xxxx.trycloudflare.com/health)
```

起動ログの例:

```
narou relay: C:\...\scripts\relay\.env を読み込みました (NAROU_RELAY_KEY, NAROU_RELAY_ALLOWED_HOSTS)
narou relay listening on 127.0.0.1:3000 (fetch: curl:path, wget:path)
```

`(fetch: ...)` の `curl:path` / `wget:path` は実行ファイルの由来（`env` = 環境変数で明示、`path` = PATH から発見）です。Windows の wget が無い場合は `(fetch: curl:path)` になります。

- ポートを変える: `pwsh -File scripts/relay/home-relay.ps1 -Port 3100`
- トンネル無しでローカルだけ: `-NoTunnel`
- バイナリを明示する: `-BinaryPath .\target\release\narou_relay.exe`
- 強制的に再ビルドする: `-Rebuild`
- 終了は `Ctrl+C`。子プロセスも停止する

Windows の OS の curl は Schannel を使い、自宅回線のハーメルン取得に成功した実績があります。

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

1. ブラウザで `https://xxxx.trycloudflare.com/health` を開き、`{"ok":true,"fetch":"curl,wget"}` を確認
2. Web UI でハーメルンの作品を取得・更新する
3. 失敗する場合、リレー自身のログ（`home-relay.ps1` を動かしたウィンドウ）と Worker のログを確認
   - `narou relay curl: ...` / `narou relay wget: ...` → 取得経路の失敗（理由が `tls-verification` / `not-installed` / `timed-out` / `failed` など）
   - `narou relay upstream status 403` → 上流サイトがリレーの取得を拒否
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

`NAROU_RELAY_ENDPOINT` は `https://relay.example.com` にします。本リポジトリの CI を使う場合は、GitHub Environment `CloudFlare` の Secret **`RELAY_DOMAIN`** に `relay.example.com` を設定すれば、`NAROU_RELAY_ENDPOINT` は自動で `https://relay.example.com` になります。

### 7.2 Cloudflare 以外のトンネル（常用はこちらを推奨）

Worker はリレーへ通常の fetch サブリクエスト（`NAROU_RELAY_ENDPOINT` のオリジンに `POST /proxy`）で接続します。Cloudflare のドメイン（`trycloudflare.com` / 自分のドメイン）へ Worker から接続できるかは実機で未検証です。**常用するなら、最初から Cloudflare 以外のトンネル**を使うのが安全です。

- Tailscale Funnel: `tailscale funnel 3000` → `https://<machine>.<tailnet>.ts.net`（無料、URL 固定）
- ngrok: `ngrok http 3000` → `https://xxxx.ngrok-free.app`（無料枠は URL が固定されない場合あり）

Cloudflare Tunnel を使う場合は、まずクイックトンネルで `Worker → リレー` が通るかだけを確認し、通らなければ上記へ切り替えてください。URL はサービス側の仕様に従います。

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
- TLS は実行環境の curl / wget に委ねる。Windows の curl（Schannel）と wget は既定で OS の証明書ストアを使う
  - 2026-10-20 の eMudhra 中間 CA 侵害では、失効したルートを含む CA バンドルでの起動を拒否する（`relay/src/ca.rs` の SHA-256 指紋デナイリスト）
  - `NAROU_RELAY_CACERT`（任意）を指定すると、curl の `--cacert` / wget の `--ca-certificate` に渡り、クライアント既定の CA ストアを置き換える。指定したバンドルは起動時に検証され、侵害ルートを含めば `narou relay startup failed: CA バンドルを検証できません: ...` で停止する。Windows を更新したうえで、必要な場合だけ指定する
- `.env` は git 管理外。キーをログ・チャット・スクリーンショットへ出さない。漏れたら新しい値に作り直し、Worker とリレーの両方を更新する。Windows では `.env` の ACL を現在のユーザーだけに制限する（§3）
- 環境変数 `NAROU_RELAY_KEY` が残っていると `.env` の新しい鍵より優先される。ローテーション後はクリーンなシェルから起動し、起動ログの適用キーを確認する
- 自宅 PC が停止・スリープしている間は Worker から取得できない（既存の fetch 403 にフォールバックするだけで、データは壊れない）
- リバースプロキシ等で 3000 番ポートを直接インターネットへ公開しない。トンネルを使う

## 9. 結果の扱い（Worker のフォールバック）

Worker は対象サイトの取得が 403 になったときだけ、生ソケット → リレーの順に試します。リレーが落ちている・設定が無い場合は元の 403 のままです。自宅リレーを設定したからといって、通常のサイト取得経路は変わりません。

- 旧 Cloud Run / SORAHOST のリレーと配備資産は削除済み。Worker が使うリレーは `NAROU_RELAY_ENDPOINT` の 1 つだけなので、自宅回線か VPS のどちらかを選ぶ
- VPS リレーで足りないハーメルン等の取得だけを自宅リレーへ切り替える運用が基本
