# narou_rs

narou_rs は、日本の Web 小説を取得・管理・変換する CLI / Web UI ツールです。 
外部から観測できる挙動、設定ファイル、YAML、出力形式について [narou.rb](https://github.com/whiteleaf7/narou) との互換性を重視しつつ、Rust で保守しやすく安全に実装しています。

詳細なコマンド互換性や未完了項目は `COMMANDS.md` を参照してください。README では、普段使うコマンド、動作、注意点をまとめます。

## 謝辞
このソフトウェアは [whiteleaf氏](https://github.com/whiteleaf7) が作成した [narou.rb](https://github.com/whiteleaf7/narou) 及び [ponpon.USA氏](https://github.com/ponponusa) の [フォーク版](https://github.com/ponponusa/narou-mod) をベースに作成されています。

素晴らしいソフトウェアを開発していただいたお二方に感謝を。

## できること

- Web 小説のダウンロード、更新、変換
- `.narou/` 配下の管理データと設定の読み書き
- `webnovel/*.yaml` によるサイト定義ベースの取得
- 端末向け出力、メール送信、差分確認、バックアップ
- ブラウザから操作できる Web UI

## セットアップ

### リポジトリから実行する場合

```powershell
cargo build
cargo run -- init
```

初期化後は、必要に応じて AozoraEpub3 の場所を設定します。

```powershell
cargo run -- init -p "C:\path\to\AozoraEpub3" -l 1.8
```

### 配布バイナリを使う場合

配布 zip は `narou/` ディレクトリをルートに持つ構成です。実行に必要なファイルはその中にまとまっています。

```text
narou/
  narou_rs(.exe)
  webnovel/
  preset/
  LICENSE
  commitversion
```

`narou_rs` は、実行ファイルの近くにある `webnovel/`、`preset/`、`commitversion` を参照します。これらを分離しないでください。

## 初期化後のディレクトリ

`narou init` を実行すると、作業ディレクトリに主に以下を作成します。

```text
.narou/                  ローカル設定、DB、キュー、タグ色など
小説データ/             ダウンロードした小説データ
webnovel/               ユーザー編集用のサイト定義 YAML
```

あわせて、ホームディレクトリ側の `~/.narousetting/global_setting.yaml` をグローバル設定として使います。

## よく使う流れ

```powershell
cargo run -- init
cargo run -- download "https://ncode.syosetu.com/n9669bk/"
cargo run -- update
cargo run -- convert 1
cargo run -- web
```

サンプルデータ付きの検証環境を使う場合は、`sample/novel/` をカレントディレクトリにして実行してください。

```powershell
Set-Location .\sample\novel
cargo run -- convert 1
```

## 主要コマンド

| コマンド | 主な用途 |
| --- | --- |
| `init` | 作業ディレクトリの初期化、AozoraEpub3 設定 |
| `download` | 新規ダウンロード |
| `update` | 既存小説の更新 |
| `convert` | テキスト変換、端末向け出力 |
| `list` | 登録小説の一覧表示 |
| `tag` | タグの追加、削除、色設定 |
| `freeze` | 更新対象からの除外、再開 |
| `remove` | 小説情報または保存ファイルの削除 |
| `setting` | 設定値の参照、変更、削除 |
| `diff` | raw データや本文差分の確認 |
| `send` | 端末向け送信 |
| `mail` | メール送信 |
| `web` | Web UI の起動 |
| `backup` | バックアップ作成 |
| `clean` | 孤立データや不要データの掃除 |
| `folder` | 保存フォルダを開く |
| `browser` | 掲載ページや感想ページを開く |
| `alias` | 別名の登録、削除、一覧 |
| `inspect` | 変換時の調査ログ確認 |
| `csv` | CSV export / import |
| `log` | ログ表示 |
| `trace` | panic 時のトレース表示 |
| `help` | ヘルプ表示 |
| `version` | バージョン情報表示 |

すべてのコマンド仕様、オプション、完了度は `COMMANDS.md` にまとめています。

## よく使う例

### ダウンロード

```powershell
narou_rs download "https://ncode.syosetu.com/n9669bk/"
narou_rs download n9669bk
narou_rs download tag:未読
```

### 更新

```powershell
narou_rs update
narou_rs update 1 2 3
narou_rs update --gl
```

### 変換

```powershell
narou_rs convert 1
narou_rs convert 1 --inspect
narou_rs convert .\input.txt --enc shift_jis
```

### Web UI

```powershell
narou_rs web
narou_rs web --port 8888 --no-browser
```

## グローバルオプション

主なグローバルオプションは以下です。

| オプション | 意味 |
| --- | --- |
| `--no-color` | カラー表示を無効化 |
| `--multiple` | 複数引数を区切り文字で展開 |
| `--time` | 実行時間を表示 |
| `--backtrace` | エラー時に詳細を表示 |
| `--user-agent <UA>` | User-Agent を明示指定 |
| `-h`, `--help` | ヘルプ表示 |
| `-v`, `--version` | バージョン表示 |

`default_args.<command>` や `force.*` などの設定も [narou.rb](https://github.com/whiteleaf7/narou) 互換を意識して扱います。

## 動作上の要点

- 作業ディレクトリ単位で `.narou/` を持つ設計です。`download`、`update`、`convert` などは基本的に初期化済みディレクトリで実行してください。
- サイトごとの取得・抽出ルールは `webnovel/*.yaml` を使います。ユーザーがこの YAML を編集すると、挙動もそれに追従します。
- 保存データや設定ファイルは [narou.rb](https://github.com/whiteleaf7/narou) 互換の YAML / ディレクトリ構成を重視しています。
- 変換結果は青空文庫向け整形を基準にし、設定や device 指定に応じて追加出力を行います。
- `update` は `general_lastup`、差分 cache、strong update、freeze などの挙動を持ちます。
- `web` は localhost 利用を基本にしています。非 loopback で公開する場合は認証設定を行ってください。

## 注意点

- `narou init` 前に多くのコマンドを実行しても、初期化を促す表示になります。
- `webnovel/*.yaml` を Rust 側のハードコードより優先する方針です。サイト追従が必要な場合は、まず YAML の更新を検討してください。
- 配布物を移動するときは、実行ファイルだけでなく `webnovel/` と `preset/` も一緒に配置してください。
- `send`、`mail`、AozoraEpub3 連携は、端末や SMTP の実環境設定が前提です。
- `mail` 機能と Kindle / Kobo などの実機送信は、開発者の手元に端末が無いため十分な実地確認ができていません。動作確認や不具合報告、再現情報、修正提案に協力してもらえると助かります。
- `cargo run` でサンプル小説を扱うときは、`sample/novel/` をカレントディレクトリにしないと `.narou/` が見つかりません。

## 開発用コマンド

```powershell
cargo build
cargo test
cargo check
```

この 3 つで、通常のビルド、テスト、型検査を確認できます。
