# ログイン情報ストアの持ち方 (2026-09-24)

## 単位

**1 ログイン = 1 ブラウザセッション = 複数ホストの Cookie**。
括りは「サイト」(サイト定義 `webnovel/*.yaml` のドメイン。定義が無ければホスト。親ドメインのキーは配下の定義へ寄せる)。

```rust
LoginGroup { id: String, site: String, label: Option<String>, cookies: Vec<HostCookie{host, cookie}>, added_at }
```

- 在庫は**サイトごとに 1 エントリ** (`login_cookie`、SQLite `app_state` / `.narou/login_cookie.yaml`)。
- 配列の並び = **試行順**。ログイン壁は成功で打ち切り、部分一覧は「欠けが消えた／話数が増えた」で打ち切り。
- 送信は `merged_cookie()` が 1 本の `Cookie:` ヘッダに畳む (名前が衝突したら具体的なホスト優先)。
- 小説は成功したログインの `id` を `login_session` に持ち、次回以降は最初のリクエストから送る。
- 暗号化の associated data は在庫のキー (= サイト名)。ホスト名で束ねた旧暗号文も読める。

## 「取り込んだ YAML 単位で名前をつける」

- `narou login import <file> --name 本垢` / Web UI 取り込み欄の名前 (空欄ならファイル名) / `narou_rs_login --name 本垢`。
- 1 ファイルに複数セッションがあるときだけ `名前 1`, `名前 2` と番号を足す (`login::apply_import_name`)。
- ホストごとに書かれた旧形式 (版 1 `cookies:` / 版 2 `credentials:`) は、読み込み時に**ホストごとに並べ直してから位置で畳む** (`login::fold_host_entries` → `platform::fold_per_host_lists`) ので、1 ファイル = 1 ログインで入る。同じ位置 = 同じアカウント。
- 書き出しは version 3 (`sites:`)。v1 / v2 も読める。

## 過去に分解されたデータの修復

`LoginGroup::merge_by_site` は同サイトのログインを畳むとき、**ホストが重ならず Cookie 名も衝突しない**組み合わせを 1 ログインにまとめる (`fold_session_fragments`)。別アカウントは `PHPSESSID` などの名前が衝突するので分かれたまま残る。`groups()` は畳み込みが起きたら書き戻す。

## 操作

- CLI: `list` / `import [--name]` / `export` / `rename <site> <番号> <名前>` / `order <site> 2,1` / `clear [<site>] [--index N]`。Cookie の直接登録 (`set`/`add`) は廃止。
- Web: `GET /api/login`、`POST /api/login/import|rename|order`、`DELETE /api/login[/{site}[/{index}]]`。一覧の読み込みはタブ表示時 (`switchTab`) に行う (pane が DOM に乗る前は `#login-hosts` が無い)。
- 実機検証は `C:\Users\rumia\Documents\WebNovel` (ユーザーが自由に使ってよいと許可済み)。
