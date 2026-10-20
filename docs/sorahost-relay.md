# SORAHOST (PteWorker) による外部 HTTP 取得リレー

## 1. 目的と対象

Cloudflare Workers が直接取得できないサイトを、SORAHOST の出口 IP から取得するためのリレーです。
**Cloud Run 版と同じ `scripts/relay/` のコードをそのまま配備します。** ホストごとの差は次の 2 点だけで、
`scripts/relay/env.mjs` と `scripts/relay/transport.mjs` が吸収します。

| 差 | Cloud Run | SORAHOST (PteWorker) |
| --- | --- | --- |
| 待受ポート | `PORT`（Cloud Run が注入） | `SERVER_PORT`（実機報告。`PORT` があればそちら） |
| 待受アドレス | `K_SERVICE` があるので `0.0.0.0` | ループバック（`127.0.0.1`）が要件 |
| 設定の渡し方 | 環境変数（Secret Manager など） | `sorahost.json` に env を書く項目が無いので `/home/container/.env` |
| curl | イメージの curl、無ければ Node 実装へフォールバック | コンテナに curl が無いので**同梱した静的 curl** を使う |

リレーは**単体で 1 サーバー**に置きます。narou 本体と同居させる構成（`sorahost/README.md` §10）は
残していますが、メモリの余裕が無いため既定は別サーバーです。

## 2. プランと前提

| 項目 | 値 |
| --- | --- |
| プラン | **Micro（メモリ 256 MB / CPU 150% / ディスク 5 GB）** を推奨（スワップはプラン既定） |
| 実行環境 | Node.js |
| リージョン | 東京（名古屋は 2026-11-12 に閉鎖、東京は Node.js 可） |
| ポート | 1 個 |

- **Nano（128 MB）は勧めません。** リレーはアイドルで約 50 MB ですが、応答本文 16 MiB（プロトコル上限）で
  ピーク約 148 MB まで上がる実測があります（base64 化と JSON 化で同時に数倍の複製を持つため）。
  Micro なら通常の HTML・本文取得はもちろん、上限付近まで収まります。
- 構成は**契約後に変更できません**（増やすなら作り直し）。最初から Micro を選んでください。
- PteWorker の Node.js は **22.13 以上**が実機で確認されています（`node:sqlite` の import で確認）。
  リレーが使う API（`AbortSignal.any` は 20.3 以上）には足ります。正確な版はコンソールの `logs` で
  `[launcher] node v…` を確認できます。

## 3. 配備フォルダーを組み立てる

```sh
node scripts/relay/deploy-sorahost.mjs            # 組み立てて手順を表示
node scripts/relay/deploy-sorahost.mjs --deploy   # 組み立てて sorahost-cli deploy まで実行
node scripts/relay/deploy-sorahost.mjs --out DIR  # 出力先を固定して残す
```

このスクリプトは `scripts/deploy-tools.json` のピン（`curl_version`）で
[moparisthebest/static-curl](https://github.com/moparisthebest/static-curl) の Linux x86_64 ビルドを取得し、
配布物の `sha256sum.txt` と**ハッシュを照合**してから、次を 1 つのフォルダーへまとめます（約 6.4 MB）。

```
server.mjs  env.mjs  security.mjs  transport.mjs  worker/protocol.mjs  sorahost.json  curl  cacert.pem
```

- 同梱するのは、PteWorker のコンテナに **curl / wget / python3 が無い**ためです。さらに配備先は
  読み取り専用で実行ビットも落ちるので、リレーは起動時に同梱 curl を書き込み可能な場所
  (`$TMPDIR/narou-relay-curl-<ハッシュ>`) へコピーし、実行権限を付けてから使います
  (`--version` で実行確認できたときだけ採用し、失敗したら PATH の curl → Node 実装へ順に落ちます)。
- 同じ理由で CA バンドルも同梱し、同梱 curl を使うときだけ `--cacert` で渡します。
- `.sorahostignore` は不要です（`include` の外は送られません）。

## 4. 配備する

```sh
SORAHOST_ENDPOINT=... SORAHOST_TOKEN=... npx --yes sorahost-cli deploy
```

`scripts/relay/sorahost.json` が配備の定義です。CLI は依存の取得もビルドもしないため、
**この組み立てを飛ばして `scripts/relay/` を直接配備しないでください**（同梱 curl が入りません）。

```json
{
  "mode": "node",
  "framework": "node",
  "start": "node server.mjs",
  "include": ["server.mjs", "env.mjs", "security.mjs", "transport.mjs", "worker/protocol.mjs", "curl", "cacert.pem"]
}
```

- 起動コマンドは `sorahost.json` の `start` が持ちます（Pterodactyl 画面での設定は不要）。
- 配備ごとに `/home/container/.sorahost/releases/<日時>-<hash>/` が作り直されます。リレーは状態を持たないので
  影響しません（鍵と許可リストは `.env` 側にあります）。
- 起動してすぐ落ちると配備失敗（422）扱いになり、前のリリースが動き続けます。`logs` を確認してください。

## 5. サーバー側の設定（.env）

`sorahost.json` には環境変数を渡す項目が無く、CLI は `.env` を**必ず除外**します。パネルの
ファイルマネージャまたは SFTP で **`/home/container/.env`** に置いてください（再デプロイで消えません）。

```sh
NAROU_RELAY_KEY=<Worker の NAROU_RELAY_KEY と同じ値>
NAROU_RELAY_ALLOWED_HOSTS=syosetu.org,novel18.syosetu.com   # 取得先を完全一致で列挙
```

読み込み順は **環境変数（プラットフォームの値）が最優先**、次に `NAROU_RELAY_ENV_FILE`、
無指定なら `<モジュールの隣>/.env` → `$HOME/.env` です。既にある環境変数は上書きしません。

任意の設定:

| 変数 | 用途 |
| --- | --- |
| `NAROU_RELAY_HOST` | 待受アドレスの明示（既定は上記の表のとおり） |
| `NAROU_RELAY_CURL` | 使う curl の明示（同梱 curl を使わない場合） |
| `NAROU_RELAY_CACERT` | `--cacert` に渡す CA バンドルの明示 |
| `NAROU_RELAY_ENV_FILE` | `.env` の場所を明示 |

## 6. 公開と TLS

PteWorker が発行する公開 URL は **平文 HTTP**（`http://<IP>:<port>/`）です。リレーは
`X-Proxy-Token` と転送 Cookie を載せるため、**平文のまま公開しないでください**。Cloudflare の
コネクタ（cloudflared）やリバースプロキシで HTTPS を終端し、その URL を Worker の
`NAROU_RELAY_ENDPOINT` に設定します。

- リレー用のホスト名には **Cloudflare Access を付けないでください**（Worker はログインできません。
  リレーが受け取るのは `X-Proxy-Token` だけです）。
- Worker（Cloudflare）から同じゾーンのトンネル ホスト名への `connect()` は経路が問題になりやすいため、
  別ゾーンのホスト名にするのが無難です。
- 出口 IP は SORAHOST の共有 NAT です（通信はすべて大阪の Edge を経由）。Cloudflare のレンジ外なので
  リレーの目的は満たしますが、対象サイトがその IP を弾かないかは配備後に実測してください。

## 7. 確認

```sh
curl -s http://<公開 URL>/health                      # {"ok":true}（認証不要）
python3 scripts/relay/verify_auth.py                  # 誤トークン 403 / 正トークンで取得
```

- リレーが curl を使えているかは、`/proxy` の応答の `via` で分かります（`curl` / `node-http`）。
  起動ログにも `narou relay listening on 127.0.0.1:<port> (curl: bundled|path|env)` と出ます。
  `curl: path` は同梱 curl を用意できず PATH の curl を試す状態で、それも無ければ Node 実装で取得します。
- `403` は Worker とリレーの `NAROU_RELAY_KEY` の不一致です。鍵が未設定なら Node 版は起動しません
  （起動ログに `NAROU_RELAY_KEY is required`）。起動後の `500` はリレー内部の異常です。

## 8. Cloud Run との違い（要点）

| | Cloud Run | SORAHOST (PteWorker) |
| --- | --- | --- |
| 配備 | Developer Connect + Cloud Build（`/scripts/relay` がビルド文脈） | `sorahost-cli deploy`（組み立てたフォルダー） |
| 起動 | Buildpacks の `npm start` | `sorahost.json` の `start` |
| 常駐 | 最小 0 で休止しうる | 常時起動 |
| 設定 | 環境変数（Secret Manager） | `/home/container/.env` |
| curl | イメージの curl | 同梱した静的 curl |
| TLS | Cloud Run が終端 | 自前（コネクタ / リバースプロキシ） |

同じ理由（出口 IP を Cloudflare 以外に置く）で運用し、**片方だけを配備しても Worker 側の設定は同じ**です。
`NAROU_RELAY_ENDPOINT` を切り替えれば移行できます。

## 9. 注意

- SORAHOST の利用規約 第7条は「公開プロキシ…として第三者の通信を無断で中継する行為」(13号) と
  「対象サーバーやサイトへの過度な自動巡回・データ収集」(15号) を禁止しています。本リレーは
  自前の Worker からのみ・共有鍵認証・許可ホストの完全一致・リダイレクト不追跡ですが、
  **常駐させる前に SORAHOST（Discord）へ用途を確認**しておくのが安全です。
- 対象サイトのアクセス間隔・利用規約を尊重し、Challenge の自動操作は行いません。
- 停止は PteWorker のコンソールから行えます。撤去するときはサーバーと `.sorahost.json`（配備トークン）を
  別々に整理してください。
