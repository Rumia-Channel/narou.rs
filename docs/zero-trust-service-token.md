# Cloudflare Zero Trust の Service Token（手動で用意する）

外部公開 API (`/api/v1`) を Cloudflare Workers で使うとき、Cloudflare Zero
Trust (Access) の **Service Token** を必要とする。この token は **手動で
作る**。CI は token を作らない。

## なぜ CI では作らないか

- Service Token は Cloudflare アカウントに対する資格情報で、CI が持つ
  `CLOUDFLARE_API_TOKEN` (アカウント管理権限) を使って自動発行させると、
  配備のたびに資格情報を minted できる状態になる。発行権限を持つのは
  運用者に限ったほうが安全である。
- Cloudflare の Service Token の secret は **発行時だけ** 表示される
  (一覧 API は Client ID しか返さない)。自動生成すると、その値をどこへ
  保存するかが必然的に CI 側に残る。

## 手順

1. Cloudflare ダッシュボード → Zero Trust → Access → Service Auth →
   Service tokens → **Add a service token**
   - Name: 運用先で分かる名前にする (例 `narou-rs-api`)。
   - Duration: 期限を設定する (ローテーションを想定)。
2. 発行された **Client ID** と **Client Secret** を控える。
3. **GitHub Actions 側の secret** として登録する（Cloudflare の
   コンソールに入れるのではなく、CI へ渡す環境変数）。
   - `CF_ACCESS_CLIENT_ID`
   - `CF_ACCESS_CLIENT_SECRET`

   置く先はリポジトリの Settings → Secrets and variables → Actions、
   もしくは **GitHub Environment "Cloudflare"** の secret
   （デプロイジョブは `environment: Cloudflare` を使うので、そこで
   定義すればこのジョブだけに見える）。

   Cloudflare Secrets Store を使う運用なら、代わりに**名前だけ**を
   登録する（値はストアに置く）。
   - `NAROU_CF_ACCESS_CLIENT_ID_SECRET_NAME` … ストアのエントリ名
   - `NAROU_CF_ACCESS_CLIENT_SECRET_SECRET_NAME` … ストアのエントリ名

4. 次のデプロイで、CI が Worker secret
   (`NAROU_CF_ACCESS_CLIENT_ID` / `NAROU_CF_ACCESS_CLIENT_SECRET`) として
   投入する (`worker_entry/ci/deploy_worker.py` の `secret_file`、
   `wrangler secret put`)。**Cloudflare 側へ手で入れる必要はない。**
   Secrets Store を使う場合は、CI が `<名前>_STORE` バインディングを作り、
   Worker はそこから読む（値は GitHub 側に残らない）。
5. Worker の設定ページ「外部API」タブに Client Id / Client Secret が
   表示される。外部ツールへ転記する。

### 値が置かれる場所（まとめ）

| 値 | 置く場所 | 設定する場所 |
|---|---|---|
| `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET` | GitHub Actions の secret（リポジトリ、または Environment "Cloudflare"） | 運用者が手動で登録 |
| `NAROU_CF_ACCESS_CLIENT_ID` / `NAROU_CF_ACCESS_CLIENT_SECRET` | Cloudflare の Worker secret | CI がデプロイ時に自動で投入 |
| `NAROU_CF_ACCESS_CLIENT_ID_STORE` / `NAROU_CF_ACCESS_CLIENT_SECRET_STORE` 経由 | Cloudflare Secrets Store | ストアに手動で登録し、CI は名前だけ受け取る |

## 動作

- Access を設定した Worker は、`CF-Access-Client-Id` /
  `CF-Access-Client-Secret` ヘッダが secret と一致すれば通す。narou の
  Bearer トークンは求めない (`external-api.require-token` の値的影响を
  受けない)。
- Access を設定していない Worker (secret 未投入) は、narou の Bearer
  トークンを要求する。トークンが 1 件も無ければ通らない (fail closed)。

## 確認

```powershell
curl -H "CF-Access-Client-Id: <Client ID>" `
     -H "CF-Access-Client-Secret: <Client Secret>" `
     "https://<worker domain>/api/v1/novels?limit=1"
```

200 が返れば Zero Trust 層は通っている。401 なら secret が未投入か、一致していない。

## ローテーション

1. 新しい Service Token を作る。
2. `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET` を差し替える。
3. デプロイし直す (Worker secret が更新される)。
4. 古い token を Zero Trust 側で削除する。
