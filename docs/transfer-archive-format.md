# 小説 ZIP 転送形式 (`format_version: 2`、通常作品の旧形式 `1` も読み取り可)

> `narou export` / `narou import` が使う **小説データ転送アーカイブ** の書式です。
> 形式そのものは `src/transfer` (プラットフォーム非依存) に実装があり、CLI・Native Web
> API・Worker・ブラウザ WASM が生成・検証・取り込み計画を共有します。

- 逐次生成: `ArchiveStream<W>` / `AsyncArchiveStream<W>` (ZIP64、64 KiB 単位の I/O)
- クライアント検証: `ValidatedArchive` (immutable な範囲読み出し、全エントリ検証後だけ個別展開)
- Native CLI の一時保存: `StagedArchive` (全検証が済むまで既存データを変更しない)
- 取り込み計画: `plan_manifest`、選別: `transfer::projection` (`runtime` / `complete`)
- 全量 utility: `build_archive` / `parse_archive` / `plan_import` (fixture 等向け。本番の Web 転送には使わない)

**未公開の別ブランチ実装**: Web は `/api/transfer/v2` のセッション・生バイト転送へ切り替え、ZIP の生成・検証・展開をブラウザの Dedicated Worker + WASM が担当する。ZIP 全体 POST・サーバー ZIP・Base64 応答への互換フォールバックは持たない。統合動作検証は未完了。実行場所と公開・再開境界は [クライアント主体の転送設計](client-transfer-design.md) を参照。

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
- エントリの物理的な順番は固定しない。逐次生成では `manifest.json` を最後に書き、
  ZIP の data descriptor を使う。索引の内容と取り込み時の検証規則は同じ。

## 2. `manifest.json`

```json
{
  "format_version": 2,
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
| `format_version` | 必須 | 現在は `2`。通常作品の旧形式 `1` も受け付ける。それ以外は `UnsupportedVersion` |
| `app_version` | 任意 | 生成したアプリケーションのバージョン |
| `exported_at` | 任意 | 生成日時 (RFC 3339) |
| `novels` | 任意 | 作品の配列 (省略時は空配列。位置は ZIP 内の `novels/<i>` と対応する) |

### 2.2 作品 (`novels[]`) のフィールド

| フィールド | 必須 | 意味 |
|---|---|---|
| `toc_url` | 必須 | **衝突キー**。完全一致で同一作品とみなす (§5)。バーチャル小説はライブラリ ID を含まない永続的な識別 URL (§2.5) |
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
| `files` | 任意 | 作品に属するファイル (§2.3)。通常作品は `toc.yaml` が必要。バーチャル小説は自身の変換設定・明示的な表紙だけ (§2.5) |
| `virtual_members` | 任意 | バーチャル小説のメンバーの `toc_url` を結合順に並べた配列。通常作品では省略 (§2.5) |

### 2.3 ファイル (`files[]`) のフィールド

| フィールド | 必須 | 意味 |
|---|---|---|
| `path` | 必須 | 小説ディレクトリ基準の相対パス (`/` 区切り) |
| `size` | 任意 | 非圧縮サイズ。生成側が読みながら計算する |
| `sha256` | 任意 | 内容ハッシュ (64 桁 hex)。生成側が読みながら計算する |

逐次生成側は各ファイルを読みながら `size` / `sha256` を計算し、最後に manifest を書き込む。
手書きの ZIP に値がある場合は、全検証経路で実データと突き合わせる
(未指定ならその照合は省略される)。サイズ上限・CRC・パス等の検査は省略しない。

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

### 2.5 バーチャル小説

- バーチャル小説だけを選択しても、メンバーの実データを自動的に同梱する。個別選択と親経由で重複選択したメンバーは 1 回だけ格納する。既存の所属規則と同じく、1 メンバーを複数の親へ所属させる構成は拒否する。
- 通常作品の本文・挿絵を先に書き、最後に親のメタデータと `virtual_members` を書く。メンバーの本文を親へ複製・連結した ZIP は作らない。
- `virtual_members` は空でない配列で、順序が結合順。同じアーカイブ内の通常作品の `toc_url` だけを参照する。参照先の欠落・重複・曖昧な URL・自己参照・バーチャル小説の入れ子は、既存データを変更する前に拒否する。
- 親自身のファイルは `setting.ini` / `replace.txt` / `converter.yaml` / `cover.jpg` / `cover.png` / `cover.jpeg` だけ。親の `toc.yaml` は不要で、本文・挿絵は各メンバーが保持する。
- 親の `toc_url` は `https://narou.rs/virtual/<32桁の小文字hex乱数>`。新規作成時に発行し、転送後も保持する。旧親のローカル ID 付き URL は、最初の書き出し時に一度だけこの形式へ保存し直す。同じ番号の別ライブラリの親と衝突させない。
- 親の元の `ncode` と内部タグ `vs_<ID>` は移さない。取り込み先で新しい ID を採番し、その ID から再生成する。タイトル・作者・通常タグ・属性・親の変換設定・明示的な表紙を保持する。
- 取り込みは通常作品を先に復元し、Create / Overwrite / Skip で得た取り込み先 ID に参照を置き換えてから親を登録する。失敗したメンバーに依存する親は公開しない。既に復元できた通常作品は保持する。
- 親のレコードとメンバー構成は、Native の同じ在庫更新境界、Worker の同じ D1 CAS トランザクションで公開する。Overwrite は親の ID・保存先を維持し、構成順と同梱された設定を復元する。Skip は既存の親を変更しない。
- 親の話数・最新掲載日・単一種別の属性は復元したメンバーから集計する。混在時はアーカイブの属性選択を保持する。親の通常タグは既存・アーカイブ・メンバーの和集合とし、同時に追加されたタグを消さない。
- この構造は形式 `2` のみ。構成を持たず `vs_<ID>` だけを持つ旧アーカイブは、壊れた親として登録せず拒否する。形式 `1` の通常作品は従来どおり読み取れる。

## 3. 決定性

バッファ生成 API `build_archive` は、同じ入力から **同じバイト列** を生成する。

- 作品は通常作品 → バーチャル親の順に分け、それぞれ `(toc_url, title)` で安定ソートする。ファイルは `path` でソートし、`virtual_members` の結合順は変えない。
- ZIP のタイムスタンプは固定 (`zip::DateTime::default()` = ZIP epoch)。
- 圧縮は Deflate、バックエンドは pure Rust の `zlib-rs` (wasm32 でも動く)。

→ ハッシュを取り直したいとき、タイムスタンプを固定したいときは、
**再生成すれば同じ ZIP になる**。

逐次生成 API `ArchiveStream` / `AsyncArchiveStream` は作品・ファイルを書き込んだ順に記録し、
1 ファイルの全内容も完成 ZIP もサーバーのメモリに保持しない。ZIP 索引と manifest の
メタデータは保持する。エントリ順・圧縮器・data descriptor の有無が異なるため、
whole-value utility とのバイト一致は保証しない。同じ形式・検証規則で読み取れる。

## 4. 検証 (取り込み時)

取り込み時は次のすべてを検査する。1 つでも欠ければ既存データを変更せず中止し、非公開の一時データを回収する。

### 4.1 サイズ・個数と作業領域

| 対象 | 制約 |
|---|---|
| ZIP 全体・展開後合計・1 ファイル | 逐次経路には旧 512 MiB / 256 MiB の固定上限を置かない。ZIP64 と保存先の空き容量に従う |
| `manifest.json` / wire manifest | 直列化後 64 MiB 以下 |
| ZIP エントリ / 作品数 | 50,000 / 10,000 以下 |
| WASM / Worker の保持メタデータ | 24 MiB 以下。capacity・再確保中の旧新領域・管理領域を含めて確保前に判定 |
| JS↔WASM / codec の I/O | 1 回 64 KiB 以下 |
| HTTP バイナリ本文 | フレーミング込み 8 MiB 以下、最大 100 レコード |
| 制御 JSON / ページ | 256 KiB 以下、ページは最大 100 項目 |

本文・ZIP 全体を `Vec` / `String` / `Blob` に連結して上限を回避しない。展開時は宣言サイズとの一致・CRC・SHA・パス・索引整合を検査する。小さな ZIP でも展開は逐次で、巨大な割り当てを起こさない。中断時は出力を完了扱いにしない。

未知の JSON 項目は構文検証しながら読み飛ばす。Native の既存グローバル在庫を更新する原子的なメタデータ処理だけは、ローカルで 8 MiB を超えることがある。この例外を Worker の処理・通信・本文へ適用しない。

明示的な全量 utility には別の保護が残る: `UTILITY_ARCHIVE_BUDGET_BYTES` / `UTILITY_TOTAL_BUDGET_BYTES` は 512 MiB、`UTILITY_FILE_BUDGET_BYTES` は 256 MiB。本番の逐次転送の容量制限ではない。

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

- Worker の Create が失敗・中断したときは、レコードが未公開であることを確認してから作成途中のファイルを削除する。公開状況の照会や削除自体に失敗した場合はエラーを記録し、後始末タスクを無限に再起動しない。公開済みか判断できない状態ではファイルを保全するため、作成途中のデータが残る場合がある。

- Overwrite では、**アーカイブに無いファイルは残る** (変換履歴・新しい話・利用者の
  編集を消さない)。メタデータは、既存レコードが空の項目だけ埋める。
- 通常作品の例外は **属性タグ** (`attribute_tag`): アーカイブが分類を持っていれば
  取り込み先の分類をそれで置き換える (アーカイブ側が正)。アーカイブに分類が
  無い場合、既存レコードの分類はそのままにする。
- 新規作成時、アーカイブに `attribute_tag` が無ければ分類は付けない。
  分類は **取り込み先 repository 側の初期化**に任せる (§2.4)。
- 同じ `toc_url` が 1 アーカイブ内に複数あると、そのうち 1 件だけ処理し、他はエラーとして
  報告する (アーカイブはライブラリ.Inject 前の状態で planning されるため)。
- 取り込みで再利用するのは **`toc_url` だけ**。`file_title` / 保存フォルダ名 /
  `original_url` / `raw_title` / ログイン状態は **転送されない**。保存フォルダ名は
  ncode とタイトルから取り込み側で再計算する。

## 6. 通信前の用途別選別

Web の既定は `runtime`。書き出しはサーバー側のファイル列挙後、**snapshot 作成・本文読み出し・通信の前**に選別する。取り込みは端末内で ZIP 全体を検証し、**セッション作成・アップロードの前**に選別する。サーバーもパスから再分類し、クライアント申告の role を信用しない。

| 対象 | `runtime` | `complete` | 理由・例外 |
|---|---|---|---|
| 現行本文 `本文/*.yaml` / `toc.yaml` | 含む | 含む | SQLite / D1 でも必要な論理データ |
| 作品メタデータ / 親の構成 | 含む | 含む | manifest 内で運ぶ。ローカル ID は再採番 |
| `setting.ini` / `replace.txt` / `converter.yaml` / 明示的な表紙 | 含む | 含む | 利用者設定 |
| `挿絵/*` / `.illustration_cache.yaml` | 含む | 含む | 索引は名前に cache があっても必須 |
| `raw/*` | 既定で除外 | 対応先だけ含む | Native は任意追加可。Worker は新規保存しない |
| `本文/cache/*` / `cache/*` | 既定で除外 | 含む | 任意の過去本文・差分履歴 |
| アプリ生成物と証明できた出力 | 除外 | 含む | EPUB / `novel.txt` など |
| 未知・利用者追加のファイル | 含む | 含む | 拡張子が EPUB でも生成物と証明できなければ残す |
| DB 本体・WAL/SHM・資格情報・在庫・一時/ロック | 除外 | 除外 | Web の内部 role。ライブラリ全体のバックアップではない |

`complete` でも宛先の非対応データは送らない。選別は元ライブラリや元 ZIP を削除しない。SQLite の物理 row、D1 の row、FS mirror があるかどうかだけで本文 YAML・設定・索引を不要と判断しない。

Native CLI の `narou export` はローカルで完全保全用 ZIP を作る従来の入口で、作品内の通常ファイルを対象にする (`backup/`、ロック・一時ファイル、symlink / reparse point は除外)。専用の一時ディレクトリに保存先の論理ファイルを集め、S3 の挿絵も収集する。CLI の ZIP 取り込みは `sqlite.mirror-files=false` へは fail closed。Web v2 の論理参照による公開とは別の制約である。

## 7. 挿絵 (`挿絵/`) と索引

- ZIP は `挿絵/` をそのまま運ぶ。取り込み時、挿絵は SplitStore 経由なので
  **S3 バックエンドなら S3 プールへルーティング**される (ローカルなら `挿絵/` へ)。
- `.illustration_cache.yaml` は「取得済み挿絵の索引」(source URL / mitemin ID / hash →
  ローカルファイル名)。ZIP に同梱されるので、取り込み先でも索引が復元される。
- Worker の書き出しは作品フォルダの一覧に加え、索引内のファイル名を共通の
  挿絵論理キーへ解決して実体を確認する。S3 の共有プールにだけある画像も
  `挿絵/<ファイル名>` として同梱し、旧作品別配置への読み取り互換を維持する。
- 外部ツールで作られた挿絵には索引の行が無い。`narou illust orphan` の到達可能判定は
  索引と `raw/*.html` から辿れるものだけを「到達可能」とし、それ以外は孤立扱いにする。
  索引に載せるには、ファイル名を `i<数字>.<ext>` (mitemin 形式) か `<sha256>.<ext>`
  (変換器が参照する正本形式) にするか、`raw/*.html` を同梱する。
- 本文の `<img src="挿絵/foo.png">` は「安全な相対パス」なら変換器がそのまま残す。
  絶対パスや `..` を含む参照は **変換器が捨てる**。

## 8. 入口

| 入口 | 用途 |
|---|---|
| `narou export <target>... [-o file.zip]` | ローカルで ZIP を作る (既定名 `narou-export-<timestamp>.zip`) |
| `narou import <file.zip> [--overwrite\|--skip]` | ローカル ZIP を取り込む (既定 Skip) |
| `GET /api/transfer/v2/capabilities` | 宛先・role・codec・上限の照会 |
| `POST /api/transfer/v2/imports` / `exports` | クライアント UUID と秘密値に束縛したセッション作成 |
| `.../imports/{id}/manifest/chunks/{n}` / `chunks` | 検証・選別後の manifest / ファイルをバイナリレコードで PUT |
| `.../exports/{id}/manifest/chunks/{n}` / `batches/{n}` | 固定 snapshot のメタデータ / 生データを GET |
| `.../{direction}/{id}/catalog` / `files` / `plan` | 有界ページで確認・計画・再開位置を取得 |
| `.../{direction}/{id}/advance` | bounded な準備・公開・回復を進める |

Web の旧 `/api/transfer/export` / `import` と `progress=1` は使わない。HTTP は ZIP ではなく生バイトと小さな制御 JSON を運ぶ。制御変更は operation ID + `If-Match`、ファイルは offset・サイズ・SHA と受領記録で重複を検出する。公開は作品単位で、完了済み作品は後続の中断で巻き戻さない。

### 8.1 Dedicated Worker の WASM API

- `codec_info()`: 実際に実行する protocol / ABI / build ID。
- `open_archive(read_range, size, limits)`: immutable な端末ファイルを範囲読み出し。サイズは十進文字列。
- `validate_archive(handle, progress)`: 全エントリを検証。成功前に manifest や本文をアップロードしない。
- `catalog_page(handle, cursor)`: role・件数等の表示用ページ。全 manifest の複製は返さない。
- `write_transfer_manifest(handle, selection, write_chunk)`: 選別済み NDJSON を逐次出力し、ファイル ID 対応を保持。
- `stream_entry(handle, entry_id, confirmed_offset, expected_prefix_sha256, write_chunk)`: 確認済み prefix を再検証してから残りを展開。
- `close_archive(handle)`: handle と作業領域を解放。
- `build_zip(read_manifest, manifest_size, read_chunk, write_chunk)`: 固定 snapshot を ZIP64 へ逐次圧縮。サイズ・SHA が変われば失敗。

入出力コールバックは各回を `await` し、64 KiB を超えない。文字・JSON 行境界とは限らないため増分処理する。WASM の memory view を JS へ保持させず、所有する bounded chunk を渡す。

配布物の生成・検証:

```sh
node scripts/build-transfer-wasm.mjs
node scripts/build-transfer-wasm.mjs --verify --assert-lean
```

実 WASM の回帰 (`wasm-bindgen` CLI は Cargo.lock と同じ版):

```sh
cargo build --manifest-path transfer_wasm/Cargo.toml --target wasm32-unknown-unknown --release
wasm-bindgen target/wasm32-unknown-unknown/release/narou_transfer_wasm.wasm --target nodejs --out-dir target/transfer-wasm-smoke
node transfer_wasm/tests/streaming.mjs target/transfer-wasm-smoke/narou_transfer_wasm.js
```

## 9. バージョン

- 書き出しは `format_version: 2`。構成情報を理解しない旧リーダーが親を通常作品として誤登録しないよう、バーチャル小説の導入時に形式を更新した。
- 形式 `1` の通常作品は引き続き読み取れる。`attribute_tag` (§2.4) のない旧アーカイブは `tags` からの移行で読み取れる。
- `1` / `2` 以外は `UnsupportedVersion` として拒否する。
- 取り込み側は「manifest に無いものは壊さない」を原則にしている (古いアーカイブで
  新しいネタが失われない)。
