# SORAHOST による外部 HTTP 取得リレー

## 1. 目的と対象

Cloudflare Workers が直接取得できないサイトを、SORAHOST の出口 IP から取得するためのリレーです。
**Cloud Run 版と同じ `scripts/relay/` のコードをそのまま配備します。** ホストごとの差は次の 2 点だけで、
`scripts/relay/env.mjs` と `scripts/relay/transport.mjs` が吸収します。

| 差 | Cloud Run | SORAHOST (PteWorker) | SORAHOST (Node.js egg) |
| --- | --- | --- | --- |
| 待受ポート | `PORT`（Cloud Run が注入） | `SERVER_PORT`（実機報告。`PORT` があればそちら） | `SERVER_PORT`（Pterodactyl が注入） |
| 待受アドレス | `K_SERVICE` があるので `0.0.0.0` | ループバック（`127.0.0.1`）が要件 | **`0.0.0.0`**（`NAROU_RELAY_HOST` で指定） |
| 設定の渡し方 | 環境変数（Secret Manager など） | `sorahost.json` に env を書く項目が無いので `/home/container/.env` | Startup 変数は egg 固定なので `/home/container/.env` |
| curl | イメージの curl、無ければ Node 実装へフォールバック | コンテナに curl が無いので**同梱した静的 curl** を使う | 同梱した静的 curl を使う（無ければ PATH） |

**SORAHOST の「実行環境 Node.js」には 2 種類あり、配備方法だけが違います。** どちらも同じ
組み立て済みフォルダーと `.env` を使います。

- **PteWorker**（`sorahost-cli deploy` で配備し、`sorahost.json` の `start` で起動）→ §4.1
- **Pterodactyl の Node.js egg**（Nano / Micro などの Node.js プラン。パネル/SFTP でファイルを置き、
  起動コマンドで `node server.mjs` を実行）→ §4.2

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
node scripts/relay/deploy-sorahost.mjs --runtime DIR  # 起動用の実行環境を用意 (再利用可)
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

### 4.1 PteWorker（`sorahost-cli`）へ配備する

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

### 4.2 Pterodactyl の Node.js egg（Nano / Micro など）: 自動更新 + `relay.js`

SORAHOST の Node.js プランは Pterodactyl の Node.js egg で動きます。`sorahost.json` も
`sorahost-cli` も使いません。**GitHub からの自動更新に任せ、MAIN FILE を `relay.js` にするだけ**で
起動できます（手動アップロード不要）。

Startup の設定:

| 項目 | 値 |
| --- | --- |
| GIT REPO ADDRESS | `https://github.com/Rumia-Channel/narou.rs` |
| INSTALL BRANCH | `main` |
| AUTO UPDATE | ON |
| USER UPLOADED FILES | OFF |
| MAIN FILE | `relay.js` |
| DOCKER IMAGE | Nodejs 24 |

起動の流れ:

1. egg が `git clone` / `git pull` で `/home/container` を最新にする（AUTO UPDATE は標準のまま）
2. 標準の起動コマンドが `relay.js` を実行する
3. `relay.js` が `scripts/relay/deploy-sorahost.mjs` の `prepareRuntime` で
   `/home/container/relay-runtime/` を用意する
   - コードはリポジトリと**ハッシュ一致**するか確認する（違えば新しいバージョンへ更新）
   - 静的 curl は `scripts/deploy-tools.json` の**ピン留めした SHA-256**と一致するものを再利用し、
     無い・壊れているときだけ再取得する（公開 `sha256sum.txt` との照合も行う）
   - CA バンドルは前回のマニフェストのハッシュで検証して再利用する
   - 準備は一時ディレクトリで行い、**成功したときだけ切り替える**（失敗しても前の環境は壊れない）
4. `relay-runtime/server.mjs` を子プロセスで起動する。ポートは `SERVER_PORT` / `PORT` をそのまま使う
5. `SIGTERM` / `SIGINT` を子プロセスへ伝えて終了する。出力はそのままコンソールへ流れる

`relay-runtime/` は `.gitignore` 済みの生成物なので、`git pull` の邪魔をしません。初回だけ静的 curl と
CA をダウンロードし（約 6.5 MB）、以降の再起動はハッシュ確認だけで済みます。壊れたときは
`relay-runtime/` を消して再起動すれば作り直します。

手動で同じ準備をする場合:

```sh
node scripts/relay/deploy-sorahost.mjs --runtime ./relay-runtime   # 用意 + 再利用
```

#### 起動コマンドの制約（重要）

標準の起動コマンドは次の判定で `.js` を `node` に渡そうとしますが、**右辺が引用符付きのため
常に偽**になり、必ず `ts-node` 側へ落ちます。

```bash
if [[ "${MAIN_FILE}" == "*.js" ]]; then /usr/local/bin/node ...; else /usr/local/bin/ts-node --esm ...; fi
```

そのため `relay.js` は **`node` でも `ts-node` でも実行できる構文**で書いています（静的な
`import` / `require` を使わず、`async main` の中でだけ動的 `import` する）。実際に
`node relay.js` と `ts-node --esm /home/container/relay.js`（TypeScript 5.9 + ts-node 10.9.2）の
両方で起動を確認済みです。

- 待受アドレスはこの egg では **`0.0.0.0`** が必要です（PteWorker のループバックとは逆）。
  §5 の `.env` に `NAROU_RELAY_HOST=0.0.0.0` を入れてください。未設定なら `relay.js` が警告します。
- ポートは Pterodactyl が `SERVER_PORT` で渡すので、こちらで指定する必要はありません。

#### 既知の阻害要因: ts-node と TypeScript 7

SORAHOST の Nodejs 24 イメージ（parkervcp/yolks 系）は `npm install -g npm@latest typescript ts-node`
で作られており、**イメージのビルド時期によっては TypeScript 7 と ts-node 10.9.2 が非互換**になります。
この場合、ts-node は `relay.js` を読む前に次のエラーで落ちます（`narou relay:` の行が 1 つも出ない）。

```
TypeError: Cannot read properties of undefined (reading 'fileExists')
    at readConfig (.../ts-node/dist/configuration.js:91:33)
```

この状態は**リポジトリ側では回避できません**（ts-node が起動する前の段階で失敗するため）。
SORAHOST 側への依頼は次のいずれかが最小です。

1. 起動コマンドの判定を直す（`[[ "${MAIN_FILE}" == *.js ]]` にする、または `.js` / `.mjs` を `node` で実行する）
2. イメージの `typescript` を 5 系に固定する

まず起動ログを見て、上記エラーが出るかどうかを確認してください。

### 4.3 手動でファイルを置く場合（フォールバック）

起動コマンドを編集できず、自動更新も使えない環境向けの手順です。

1. `node scripts/relay/deploy-sorahost.mjs --out .\relay-deploy` でフォルダーを作る
2. パネルのファイルマネージャか SFTP で、その**中身**を `/home/container` へ置く
   （`server.mjs` `env.mjs` `security.mjs` `transport.mjs` `worker/protocol.mjs` `curl` `cacert.pem`。
   `sorahost.json` は PteWorker 用なので不要）
3. 起動コマンド（Startup）を次の 1 行に差し替える

```bash
cd /home/container && exec /usr/local/bin/node ./server.mjs
```

- `exec` を付けると停止シグナルが node に直接届きます。
- git で配る場合だけ、先頭の `git pull` と `npm install` の部分を残して構いません
  （`/home/container/package.json` が無ければ `npm install` は走りません）。
- 起動コマンドを編集できない場合は `MAIN_FILE=server.mjs` を試すことになります。`ts-node --esm` 経由に
  なるため動作は保証できません（失敗するなら SORAHOST に起動コマンドの変更を依頼してください）。

## 5. サーバー側の設定（.env）

`sorahost.json` には環境変数を渡す項目が無く、CLI は `.env` を**必ず除外**します。Pterodactyl の
Startup 変数も egg 側の名前（`MAIN_FILE` など）に固定されています。パネルのファイルマネージャまたは
SFTP で **`/home/container/.env`** に置いてください（PteWorker は再デプロイでも消えません）。

```sh
NAROU_RELAY_KEY=<Worker の NAROU_RELAY_KEY と同じ値>
NAROU_RELAY_ALLOWED_HOSTS=syosetu.org,novel18.syosetu.com   # 取得先を完全一致で列挙
NAROU_RELAY_HOST=0.0.0.0                                    # Node.js egg のときだけ必須
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

SORAHOST の公開先はどちらの製品も **平文 HTTP**（PteWorker は `http://<IP>:<port>/`、
Pterodactyl の Node.js egg は割り当て IP:ポートそのもの）です。リレーは
`X-Proxy-Token` と転送 Cookie を載せるため、**平文のまま公開しないでください**。Cloudflare の
コネクタ（cloudflared）やリバースプロキシで HTTPS を終端し、その URL を Worker の
`NAROU_RELAY_ENDPOINT` に設定します。

- Node.js egg の場合は、起動コマンドから `cloudflared tunnel run --token ...` を同居させるか、
  前段にリバースプロキシを置いてください。
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
  起動ログにも `narou relay listening on <host>:<port> (curl: bundled|path|env)` と出ます。
  `curl: path` は同梱 curl を用意できず PATH の curl を試す状態で、それも無ければ Node 実装で取得します。
- `403` は Worker とリレーの `NAROU_RELAY_KEY` の不一致です。鍵が未設定なら Node 版は起動しません
  （起動ログに `NAROU_RELAY_KEY is required`）。起動後の `500` はリレー内部の異常です。

## 8. Cloud Run との違い（要点）

| | Cloud Run | SORAHOST (PteWorker) | SORAHOST (Node.js egg) |
| --- | --- | --- | --- |
| 配備 | Developer Connect + Cloud Build（`/scripts/relay` がビルド文脈） | `sorahost-cli deploy`（組み立てたフォルダー） | GitHub clone + AUTO UPDATE（MAIN FILE = `relay.js`） |
| 起動 | Buildpacks の `npm start` | `sorahost.json` の `start` | `relay.js` → `relay-runtime/server.mjs` |
| 実行環境の準備 | Buildpacks が npm install | 配備時にフォルダーへ同梱 | `relay.js` が起動時に準備（検証済みは再利用） |
| 待受 | `0.0.0.0`（`K_SERVICE`） | ループバック | `0.0.0.0`（`NAROU_RELAY_HOST`） |
| 常駐 | 最小 0 で休止しうる | 常時起動 | 常時起動 |
| 設定 | 環境変数（Secret Manager） | `/home/container/.env` | `/home/container/.env` |
| curl | イメージの curl | 同梱した静的 curl | 同梱した静的 curl（初回だけ取得） |
| TLS | Cloud Run が終端 | 自前（コネクタ / リバースプロキシ） | 自前（コネクタ / リバースプロキシ） |

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
