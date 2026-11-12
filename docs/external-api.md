# 外部公開 API と MCP

外部のツール (表生成・自動化・LLM など) からライブラリの読み取り専用データへアクセスするための API と、その上位で動作する MCP サーバーについて説明します。

- HTTP API: `/api/v1` (native の `narou web` と Workers の両方で動く)
- ブラウザ連携: `/api/read/v1`（既存の Web UI / Access ユーザー認証を利用）
- MCP: `POST /api/v1/mcp` (HTTP) と `narou_rs_mcp` (stdIO)

## Workers の配信用 Worker

Workers の配備は、管理・更新用の既存 Worker と、同じ名前に `-reader` を付けた
配信用 Worker に分けます。同じ D1 と S3 を参照し、作品データの複製・同期はしません。
Native の API と EPUB 生成経路は変更しません。

| Worker | 担当 |
|---|---|
| 管理・更新用 (`NAROU_WORKER_ROLE=writer`) | download / update / convert、設定・タグ・作品の変更、Queue、定期更新 |
| 配信用 (`NAROU_WORKER_ROLE=reader`) | `/api/v1`、`/api/read/v1`、作品情報・挿絵の取得、通常作品・バーチャル小説の EPUB 生成 |

同じ公開ホストの対象パスを Cloudflare の Route で配信用 Worker へ直接振り分けるため、
連携ソフトの URL・Cookie・Bearer トークン・Access の設定は変更不要です。
Web UI の `/novels/{id}/download`、既存の `/api/novels/{id}/download.epub`、
`/api/virtual_novels/{id}/download.epub` も対象です。Route はパスの途中の ID だけを
ワイルドカードにできないため、同じプレフィックスにある編集要求は
`NAROU_WRITER` Service Binding 経由で管理側へ転送します。認証・本文はそのまま引き継ぎ、
配信側で編集ハンドラを実行しません。トークンの発行・削除も管理側のままです。

配信側では作品・設定・保存方式マーカーの書き換えや旧データの補完を行いません。
ただし EPUB の有界ストリーム生成には、非公開 stage と
`.narou-stage-journal/` への書き込み・削除が必要です。この一時領域だけを許可し、
作品の保存先への書き込みは拒否します。途中終了で残った一時データは、
管理側の既存の期限付き回収が扱います。D1 の binding 自体に読み取り専用権限を
設定するものではなく、配信ルートと保存アダプタで処理を制限します。

Cloudflare Workers のメモリ上限は [1 isolate あたり 128 MB](https://developers.cloudflare.com/workers/platform/limits/#memory)
で、同じ isolate の同時要求・JavaScript・Wasm の割当を合わせた値です。
Worker の分離は更新と配信のメモリ・重い処理の実行枠を切り離しますが、
EPUB 単体の上限を増やしたり、共有 D1 / S3 の競合までなくしたりするものではありません。
配備手順は [`cloudflare-workers-migration-plan.md` §2.2.8](cloudflare-workers-migration-plan.md#228-ci-からのデプロイ-p0e-2026-09-26-実装) を参照してください。

## 認証の連鎖

外部 → [Cloudflare Zero Trust] → [Bearer トークン] → API / MCP

| レイヤー | 利用環境 | 条件 |
|---|---|---|
| Cloudflare Zero Trust (Access) の Service Token | Cloudflare Workers だけ | `CF-Access-Client-Id` と `CF-Access-Client-Secret` ヘッダが Worker の secret (`NAROU_CF_ACCESS_CLIENT_ID` / `NAROU_CF_ACCESS_CLIENT_SECRET`) と一致する |
| narou の Bearer トークン | native (VPS を含む) | `Authorization: Bearer <token>` または `x-narou-api-token: <token>` が保存済みトークンに一致する |

- **Zero Trust の内側にある場合** (Workers に Access の資格情報を設定した場合): Service Token の検証のみで認証を通過し、narou の Bearer トークンは不要です。
- **Access を設定していない場合**: narou の Bearer トークンによる認証が必要です。
- いずれの環境でも **`external-api.require-token=false`** を設定することで、Bearer トークンの検査を省略できます (Access の保護下で利用する場合など)。
- トークンが 1 件も登録されていない状態では、Bearer 認証はすべて拒否されます (fail-closed)。

### トークンの作成と確認

- CLI: `narou api create <名前>` / `narou api list` / `narou api delete <ID>` /
  `narou api info`
- Web UI: 設定ページの「外部API」タブ (トークンの作成・表示・削除)
- 保存先: 在庫 `api_tokens` (Native は `.narou/api_tokens.yaml` / SQLite
  `app_state`、Worker は D1 `app_state`)

トークン本体は**平文**で保存されます (`server-basic-auth.password` と同様の扱い)。これは後から値を確認できる利便性を優先した仕様であり、トークンは `narou api list` や設定ページからいつでも確認可能です。紛失した場合は再作成してください。

### Cloudflare Access の資格情報を Web UI で確認する

Workers 環境では Access の Service Token は CI 経由で secret として登録されます。Web UI の「外部API」タブには、その secret (`NAROU_CF_ACCESS_CLIENT_ID` / `NAROU_CF_ACCESS_CLIENT_SECRET`) の値が表示されるため、外部ツールへ簡単にコピー＆ペーストできます (※ native 環境にはこの項目はありません)。

## 既存のユーザー認証で読むブラウザ API

`/api/read/v1` は `/api/v1` の外部ツール用トークン認証とは別の読み取り専用入口です。
Native は既存の UI Cookie / Basic 認証、Worker は既存の UI 認証
（Access が境界ならそのユーザーセッション）をそのまま使います。
新しいトークン発行・Access アプリ・ポリシーの追加は不要です。

| メソッド | パス | 内容 |
|---|---|---|
| GET / HEAD | `/api/read/v1/novels` | 一覧（`limit=all`、検索・並び順は既存 API と共通、JSON 固定） |
| GET / HEAD | `/api/read/v1/novels/{id}` | 作品情報（JSON） |
| GET / HEAD | `/api/read/v1/novels/{id}/epub` | 既存の EPUB 配信・オンデマンド生成 |
| GET / HEAD | `/api/read/v1/favorite-tags` | お気に入りタグ（`success` と `tags`） |

クロスオリジン連携はグローバル設定 `external-api.allowed-origins` が空なら拒否します。
許可する Origin をカンマ区切りで指定してください
（例: `https://dantalian.rumia-ch.uk,http://127.0.0.1:8080`）。
scheme・host・port が一致する Origin だけを許可し、`*` や `null` は許可しません。
設定は narou.rs の既存グローバル設定画面 / API から変更でき、Worker の判定は
isolate キャッシュや read replica を介さず primary の最新設定を読みます。

ブラウザは narou.rs を一度開いて既存のログインを済ませ、`credentials: "include"`、
`cache: "no-store"`、`referrerPolicy: "no-referrer"` で直接取得します。
長期トークン・Service Token・独自の認証ヘッダーを連携先へ渡してはいけません。
Cookie の SameSite / 第三者 Cookie 制限はブラウザのまま適用されます。
Native の SameSite=Strict Cookie を使う場合は同じ site に配置してください。

許可済み Origin の CORS 応答には credentials と `Vary: Origin` を付けます。
preflight は GET / HEAD と `Range` だけを許可し、管理・更新 API は公開しません。
実際の GET / HEAD では既存のユーザー認証が必要です。
Access 側の OPTIONS 設定は変更しません。連携クライアントは独自ヘッダーを付けず、
EPUB の単一範囲 `Range: bytes=0-0` または通常 GET を使います。
応答は `Cache-Control: no-store` です。


## 出力形式

`format` パラメータで選ぶ。

| 値 | Content-Type | 形 |
|---|---|---|
| `json` (既定) | `application/json` | 1 つの JSON ドキュメント |
| `jsonl` | `application/x-ndjson` | 1 行 1 レコード |
| `xml` | `application/xml` | XML |

一覧の `json` だけは件数のラッパーが付く (`records_total` /
`records_filtered` / `offset` / `limit` / `data`)。`jsonl` は 1 行 1
作品、`xml` は `<novels>` の下に `<novel>` が並ぶ。

未知の `format` はどのデータ取得エンドポイントでも 400 になる。
タグ一覧の `jsonl` は 1 行 1 タグ、配列の `xml` は必ず 1 つのルート要素に包む。

## エンドポイント

| メソッド | パス | 内容 |
|---|---|---|
| GET | `/api/v1` | 案内 (エンドポイント一覧) |
| GET | `/api/v1/novels` | 一覧 |
| GET | `/api/v1/novels/{id}` | 作品 1 件の情報 |
| GET | `/api/v1/novels/{id}/toc` | 目次 (バーチャル小説は連結) |
| GET | `/api/v1/novels/{id}/sections` | 全話の本文 |
| GET | `/api/v1/novels/{id}/sections/{key}` | 1 話の本文 |
| GET | `/api/v1/novels/{id}/epub` | EPUB (バーチャル小説は結合 EPUB) |
| GET | `/api/v1/tags` | タグと使用数 |
| POST | `/api/v1/mcp` | MCP (JSON-RPC 2.0) |

エラーは出力形式に関係なく `{"error": {"code": "...", "message": "..."}}`
で返す。入力不正は 400 (`bad_request`)、作品・目次・本文の欠落は
404 (`not_found`)、状態競合は 409 (`conflict`)、保存データの破損や
ストレージ障害は 500 (`internal_error`)。未認証は 401
(`authentication_required`)。Native／Workers で同じ対応を使う。

## 一覧の絞り込み

| パラメータ | 内容 |
|---|---|
| `q` (別名 `search`) | 自由検索。`tag:` / `author:` / `site:` / `title:` / `status:`、`-` での否定、`a\|b` の OR、引用符で囲んだフレーズ |
| `title` | 題名の部分一致 |
| `author` | 作者名の部分一致 |
| `site` | サイト名 (完全一致) |
| `tag` | タグ (完全一致) |
| `ncode` | ncode (完全一致) |
| `attribute` | 属性タグ (`小説` / `漫画`) |
| `novel_type` | 種別 (0 / 1 = 連載、2 = 短編) |
| `end` / `suspend` / `frozen` | 完結 / 中断 / 凍結で絞る |
| `include_hidden` | 非表示設定で隠した作品も含める (既定 false) |
| `virtual` | バーチャル小説の扱い (下記) |
| `sort` | 並び順 (`id` / `last_update` / `general_lastup` / `title` / `author` / `sitename` / `length` / `general_all_no` など) |
| `order` | `asc` (既定) / `desc` |
| `offset` | 0 以上の開始位置 (既定 0)。`limit=all` と併用でき、末尾以降は空の一覧 |
| `limit` | 件数上限 (既定 100、上限 10000)。`limit=all` または `limit=0` で無制限 |

### バーチャル小説の扱い

| `virtual` | 出力 |
|---|---|
| `group` (既定) | バーチャル小説を 1 行にまとめ、メンバーは出さない (Web UI と同じ) |
| `expand` | メンバーを個別の行として出す (バーチャル小説の行は出さない) |
| `exclude` | バーチャル小説もメンバーも出さない (どのバーチャル小説にも入っていない元の作品だけ) |

バーチャル小説の行には `virtual_novel` (構成メンバー) が、メンバー側の行に
は `virtual_parent_id` が入る。

## 目次と本文

- 目次の各要素は `key` (本文取得用の識別子)、`index` (話番号)、
  `member_id`、`chapter`、`subchapter`、`subtitle`、`href` などを持つ。
- 通常の作品では `key` は話番号 (`"3"`)、バーチャル小説では
  `メンバーID:話番号` (`"12:3"`)。
- 本文は `body` (HTML) / `introduction` / `postscript` / `data_type` を返す。
  変換済みの青空文庫テキストではない。
- 全話取得は目次にある本文をすべて読み出せた場合だけ成功する。
  1 話でも欠落・破損していれば上記のエラーを返し、欠けた話を飛ばした
  部分的な本文を 200 として返さない。
- 通常作品の EPUB は native では保存済みのファイルを返し、無ければその場で
  生成する (`lite` ビルド / 外部 AozoraEpub3)。Worker は都度生成する。
  バーチャル小説は native・Worker とも、現在のメンバーの目次・本文・挿絵を
  登録順に結合し、仮想小説自身の変換設定で毎回生成する。保存済みの結合 EPUB
  でメンバー変更を隠さないため、通常作品のファイルキャッシュは使わない。
  Content-Type は `application/epub+zip`。

## MCP

`POST /api/v1/mcp` は JSON-RPC 2.0。対応するメソッドは `initialize` /
`notifications/initialized` / `ping` / `tools/list` / `tools/call`。

| ツール | 引数 | 内容 |
|---|---|---|
| `list_novels` | 一覧の絞り込みと同じ引数 + `format` | 一覧 (JSON / JSON Lines / XML) |
| `get_novel` | `id` | 作品 1 件 |
| `get_novel_toc` | `id` | 目次 |
| `get_novel_section` | `id`, `key` | 1 話の本文 |
| `list_tags` | — | タグ一覧 |

### stdIO 版

`narou_rs_mcp` は同じツールを stdIO (1 行 1 JSON) で提供する。ローカル
ライブラリを直接読むので、`narou web` を起動する必要はない。

```json
{
  "mcpServers": {
    "narou": {
      "command": "narou_rs_mcp"
    }
  }
}
```

## 実装

| 場所 | 内容 |
|---|---|
| `src/application/external_api.rs` | 一覧・目次・本文の仕様と JSON / JSON Lines / XML の描画 (native / Worker 共通) |
| `src/application/mcp.rs` | MCP の JSON-RPC とツール |
| `src/application/api_tokens.rs` | Bearer トークンの保存と管理 |
| `src/web/external.rs` | native の HTTP 層 |
| `worker_entry/src/webui/external.rs` | Worker の HTTP 層 |
| `src/bin/mcp.rs` | stdIO 版 MCP (`narou_rs_mcp`) |
