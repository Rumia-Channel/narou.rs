# 小説 ZIP 転送形式 (`format_version: 1`)

> `narou export` / `narou import` が使う **小説データ転送アーカイブ** の書式です。
> 形式そのものは `src/transfer` (プラットフォーム非依存) に実装があり、CLI・Native Web
> API・Worker・ブラウザ WASM が同じ 3 つの関数を使います。

- 生成: `build_archive(&ArchiveManifest) -> Vec<u8>`
- 検証・展開: `parse_archive(&[u8]) -> ParsedArchive`
- 取り込み計画: `plan_import(&ParsedArchive, &[ExistingNovel], ConflictPolicy) -> ImportPlan`

## 1. 全体構造

```
manifest.json
novels/0/toc.yaml
novels/0/本文/1 第一話.yaml
novels/0/setting.ini
novels/0/挿絵/cover.jpg
novels/1/...
```

- `manifest.json` が **唯一の索引**。ZIP にはこの 1 ファイルと `novels/<i>/<path>` だけが
  入る (ディレクトリエントリは作らない)。
- `<i>` は **manifest 内の `novels` 配列の位置**。同じ値が作品 ID ではない
  (取り込み先では ID を再採番する)。
- `<path>` は **小説ディレクトリ基準の相対パス**、`/` 区切り。

## 2. `manifest.json`

```json
{
  "format_version": 1,
  "app_version": "0.5.4",
  "exported_at": "2026-10-27T12:34:56Z",
  "novels": [
    {
      "toc_url": "https://ncode.syosetu.com/n1234ab/",
      "title": "タイトル",
      "author": "作者",
      "sitename": "小説家になろう",
      "domain": "syosetu.com",
      "tags": ["異世界", "魔法"],
      "attribute_tag": "小説",
      "novel_type": 1,
      "ncode": "n1234ab",
      "subtitle": "あらすじ (toc.yaml の story)",
      "general_lastup": "2026-10-01T00:00:00Z",
      "last_update": "2026-10-02T00:00:00Z",
      "files": [
        { "path": "toc.yaml", "size": 1234, "sha256": "…" },
        { "path": "本文/1 第一話.yaml", "size": 5678, "sha256": "…" }
      ]
    }
  ]
}
```

### 2.1 manifest 自身のフィールド

| フィールド | 必須 | 意味 |
|---|---|---|
| `format_version` | 必須 | 現在は `1`。別の値は取り込み側で拒否する (`UnsupportedVersion`) |
| `app_version` | 任意 | 生成したアプリケーションのバージョン |
| `exported_at` | 任意 | 生成日時 (RFC 3339) |
| `novels` | 任意 | 作品の配列 (省略時は空配列。順序は正規化される) |

### 2.2 作品 (`novels[]`) のフィールド

| フィールド | 必須 | 意味 |
|---|---|---|
| `toc_url` | 必須 | **衝突キー**。完全一致で同一作品とみなす (§5) |
| `title` | 必須 | 作品名 |
| `author` | 必須 | 作者名 |
| `sitename` | 必須 | サイト名 (一覧の「サイト名」列) |
| `domain` | 任意 | サイト定義のドメイン |
| `tags` | 任意 | 通常タグ。空配列も可。**属性タグはここに入らない** (§2.4) |
| `attribute_tag` | 任意 | 作品種別の分類。`"小説"` / `"漫画"` のいずれか (§2.4) |
| `novel_type` | 必須 | 小説種別 (0 = なし / 1 = 連続 / 2 = 短編) |
| `ncode` | 任意 | サイトの N コード |
| `subtitle` | 任意 | あらすじ (`toc.yaml` の `story`) |
| `general_lastup` | 任意 | 最新話掲載日 |
| `last_update` | 任意 | 最終更新日時 |
| `files` | 任意 | 作品に属するファイル (§2.3)。省略時は空配列 (取り込みは `toc.yaml` を要求するため実質必須) |

### 2.3 ファイル (`files[]`) のフィールド

| フィールド | 必須 | 意味 |
|---|---|---|
| `path` | 必須 | 小説ディレクトリ基準の相対パス (`/` 区切り) |
| `size` | 任意 | 非圧縮サイズ。**生成時に `build_archive` が埋める** |
| `sha256` | 任意 | 内容ハッシュ (64 桁 hex)。**生成時に `build_archive` が埋める** |

生成側は `size` / `sha256` を空のまま渡してよく、組み込みの `build_archive` が計算して
manifest に書き込む。手書きで ZIP を作る場合は、この 2 個を正しく入れないと
`parse_archive` の検証で弾かれる (未指定なら検証は省略される)。

### 2.4 属性タグ (作品種別の分類)

- 分類は **メタデータ** で、通常タグではない。manifest では `attribute_tag` に
  単独で持つ (`"小説"` / `"漫画"`)。`tags` に属性名を入れてはならない。
- `attribute_tag` は **省略可**。値が違うもの (「小説」「漫画」以外) は
  `parse_archive` が `InvalidArchive` として拒否する。
- **旧形式 (属性名が `tags` に埋まっていたアーカイブ)** は読み取り時に移行される:
  - `tags` に属性名が 1 種類だけあれば `attribute_tag` に移し、`tags` から除く。
  - 同じ属性名が重複しているだけなら、1 種類として扱う。
  - 「小説」と「漫画」の両方があれば **矛盾** として扱い、分類は付けない
    (`attribute_tag` は付けない)。矛盾のまま取り込まない。
  - 属性名が `tags` にない旧アーカイブは `attribute_tag` なし。分類は
    **取り込み先 repository 側の初期化**に任せる (サイト定義のルールで決まる)。
- 書き出し (`build_archive`) も同じ正規化を行うので、`tags` に属性名が残る
  アーカイブは生成されない。

## 3. 決定性

同じ入力からは **同じバイト列** になるように作られている。

- 作品は `(toc_url, title)` で安定ソート (`sort_by` は安定なので完全に等しい作品の
  順序は保つ)。ファイルは `path` でソート。
- ZIP のタイムスタンプは固定 (`zip::DateTime::default()` = ZIP epoch)。
- 圧縮は Deflate、バックエンドは pure Rust の `zlib-rs` (wasm32 でも動く)。

→ ハッシュを取り直したいとき、タイムスタンプを固定したいときは、
**再生成すれば同じ ZIP になる**。

## 4. 検証 (取り込み時)

`parse_archive` は次のすべてを検査する。1 つでも欠ければ取り込みは **何も書かず** 中止する。

### 4.1 サイズ・個数の上限

| 上限 | 値 | 対象 |
|---|---|---|
| `MAX_ARCHIVE_BYTES` | 512 MiB | ZIP ファイル全体 |
| `MAX_TOTAL_UNCOMPRESSED_BYTES` | 512 MiB | 全エントリの展開後合計 (manifest を含む) |
| `MAX_FILE_BYTES` | 256 MiB | 1 エントリ |
| `MAX_MANIFEST_BYTES` | 64 MiB | `manifest.json` |
| `MAX_ENTRIES` | 50,000 | ZIP エントリ数 (manifest を含む) |
| `MAX_NOVELS` | 10,000 | 1 アーカイブ中の作品数 |

展開は `take` で上限付き読み出しなので、zip bomb (展開サイズが ZIP の何倍もあるもの) は
上限で止まる。

### 4.2 パスの安全性

`validate_relative_path` が **ZIP 側と manifest 側の両方** に適用される。

- 空パス、絶対パス (`/` 始まり)、ドライブ相対パス (`C:`)、`\` を含むパス、NUL を含むパス
- `.` / `..` の構成要素、空の構成要素
- 制御文字、および Windows で禁止される `< > : " | ? *`
- 末尾の `.` / ` ` (Windows の禁止)
- Windows の予約デバイス名 (`CON` / `PRN` / `AUX` / `NUL` / `COM1`-`COM9` / `LPT1`-`LPT9` ほか)
- パス全体 4096 バイト、1 構成要素 255 文字まで

### 4.3 一致関係

- ZIP のエントリ名と manifest の `files[]` は **完全一致**が必要。
  manifest に無いエントリ / ZIP に無い manifest エントリはいずれもエラー。
- 大文字小文字を畳んだ重複 (`A.txt` と `a.txt`) はエラー。
- 同じ作品の中で `a` と `a/b` の両方が使われる (ファイルとディレクトリの衝突) のはエラー。
- `manifest.json` は **1 つだけ**。 symlink エントリは明示的に拒否する (暗号化エントリは読み出し時にエラーになる)。

### 4.4 内容検証

- manifest に `size` があれば、ZIP 側の宣言サイズと一致すること。
- manifest に `sha256` があれば、展開後のハッシュが一致すること (大文字小文字は区別しない)。

### 4.5 属性タグ

- `attribute_tag` があるとき、値は `"小説"` / `"漫画"` のどちらかでなければ
  `InvalidArchive` として拒否する。
- 属性名が `tags` に残っていても、解析時に `attribute_tag` へ移す (§2.4)。

## 5. 取り込みの衝突規則

衝突キーは **`toc_url` の完全一致**。タイトルやファイルパスでは判定しない。

| 既存 | `ConflictPolicy` | 動作 |
|---|---|---|
| 無い | どちらでも | **Create**: 新しい ID を採番し、作品フォルダを作ってファイルを書き込み、 レコードを公開する |
| ある | `Skip` (既定) | スキップ (既存は触らない) |
| ある | `Overwrite` | **Overwrite**: 既存の ID とフォルダを保持し、**アーカイブに含まれるファイルだけ**置換する |

- Overwrite では、**アーカイブに無いファイルは残る** (変換履歴・新しい話・利用者の
  編集を消さない)。メタデータは、既存レコードが空の項目だけ埋める。
- 例外は **属性タグ** (`attribute_tag`) だけ: アーカイブが分類を持っていれば
  取り込み先の分類をそれで置き換える (アーカイブ側が正)。アーカイブに分類が
  無い場合、既存レコードの分類はそのままにする。
- 新規作成時、アーカイブに `attribute_tag` が無ければ分類は付けない。
  分類は **取り込み先 repository 側の初期化**に任せる (§2.4)。
- 同じ `toc_url` が 1 アーカイブ内に複数あると、そのうち 1 件だけ処理し、他はエラーとして
  報告する (アーカイブはライブラリ.Inject 前の状態で planning されるため)。
- 取り込みで再利用するのは **`toc_url` だけ**。`file_title` / 保存フォルダ名 /
  `original_url` / `raw_title` / ログイン状態は **転送されない**。保存フォルダ名は
  ncode とタイトルから取り込み側で再計算する。

## 6. 何を同梱するか (native の `narou export`)

小説ディレクトリ (`小説データ/<sitename>[/<subdir>]/<file_title>/`) の
**通常ファイル全部**が対象。

| 対象 | 同梱 | 備考 |
|---|---|---|
| `toc.yaml` | ✅ | **必須**。無い作品はその場でエラー (取り込みにも必要) |
| `本文/*.yaml` | ✅ | |
| `setting.ini` / `replace.txt` / `converter.yaml` | ✅ | |
| `挿絵/*` | ✅ | S3 バックエンドのときは一時実体化して収集する |
| `.illustration_cache.yaml` | ✅ | 取得済み挿絵の索引 (§7) |
| `raw/*.html` | ✅ | 生 HTML (挿絵の索引を張り直す材料) |
| `cache/*` | ✅ | 差分退避 |
| `backup/` | ❌ | `narou backup` が除外するディレクトリと同じ |
| `*.lock` / `*.tmp` | ❌ | 実行時の一時ファイル |
| symlink / リパースポイント | ❌ | 小説ディレクトリの外へ出ないため辿らない |

- SQLite 管理でファイルを持っていない作品 (`sqlite.mirror-files=false`) は、
  書き出しの間だけ実体化して収集し、終わったら **自分が書いた分だけ** 削除する。
- `sqlite.mirror-files=false` のライブラリへの **取り込み** は fail closed
  (ミラーが narou.rb 向けの写しなので)。

## 7. 挿絵 (`挿絵/`) と索引

- ZIP は `挿絵/` をそのまま運ぶ。取り込み時、挿絵は SplitStore 経由なので
  **S3 バックエンドなら S3 プールへルーティング**される (ローカルなら `挿絵/` へ)。
- `.illustration_cache.yaml` は「取得済み挿絵の索引」(source URL / mitemin ID / hash →
  ローカルファイル名)。ZIP に同梱されるので、取り込み先でも索引が復元される。
- 外部ツールで作られた挿絵には索引の行が無い。`narou illust orphan` の到達可能判定は
  索引と `raw/*.html` から辿れるものだけを「到達可能」とし、それ以外は孤立扱いにする。
  索引に載せるには、ファイル名を `i<数字>.<ext>` (mitemin 形式) か `<sha256>.<ext>`
  (変換器が参照する正本形式) にするか、`raw/*.html` を同梱する。
- 本文の `<img src="挿絵/foo.png">` は「安全な相対パス」なら変換器がそのまま残す。
  絶対パスや `..` を含む参照は **変換器が捨てる**。

## 8. 入口

| 入口 | 用途 |
|---|---|
| `narou export <target>... [-o file.zip]` | 現在のライブラリから ZIP を作る (既定名 `narou-export-<timestamp>.zip`) |
| `narou import <file.zip> [--overwrite\|--skip]` | ZIP を取り込む (既定は Skip) |
| `GET /api/transfer/export?ids=1,2` | Native Web API の書き出し (`application/zip`) |
| `POST /api/transfer/import[?overwrite=true]` | Native Web API の取り込み (本文の body limit 512 MiB) |
| Worker の `/api/transfer/*` | 同じ形式 (D1 + S3 バックエンド) |
| `transfer_wasm` | ブラウザ側の `buildZip` / `parseZip` (同じコア) |

## 9. バージョン

- `format_version` は **1**。別の値のアーカイブは取り込み側で拒否する
  (`UnsupportedVersion`)。将来フィールドを追加するときは、**後方互換な追加**に留める
  (manifest の任意フィールドはそのまま無視される)。
- `attribute_tag` (§2.4) は後方互換な追加として `format_version: 1` のまま
  導入した。旧形式のアーカイブは `tags` からの移行で読み取れる (§2.4)。
- 取り込み側は「manifest に無いものは壊さない」を原則にしている (古いアーカイブで
  新しいネタが失われない)。
