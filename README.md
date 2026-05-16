# narou_rs

narou_rs は、日本の Web 小説を取得・管理・変換する CLI / Web UI ツールです。 
外部から観測できる挙動、設定ファイル、YAML、出力形式について [narou.rb](https://github.com/whiteleaf7/narou) との互換性を重視しつつ、Rust で保守しやすく安全に実装しています。

README では、導入方法、基本操作、主な注意点をまとめます。詳細なコマンド互換性や未完了項目は `COMMANDS.md` を参照してください。

## 謝辞
このソフトウェアは [whiteleaf氏](https://github.com/whiteleaf7) が作成した [narou.rb](https://github.com/whiteleaf7/narou) 及び [ponpon.USA氏](https://github.com/ponponusa) の [フォーク版](https://github.com/ponponusa/narou-mod) をベースに作成されています。

素晴らしいソフトウェアを開発していただいたお二方に感謝を。

## できること

- Web 小説のダウンロード、更新、変換
- なろう・R18 なろう・カクヨムのシリーズ/コレクション URL からの一括登録
- `.narou/` 配下の管理データと設定の読み書き
- `webnovel/*.yaml` によるサイト定義ベースの取得
- 端末向け出力、メール送信、差分確認、バックアップ
- ブラウザから操作できる Web UI

## 対応サイト

小説家になろうを含めて、下記のサイトに対応しています。

+ 小説家になろう http://syosetu.com/
+ ノクターンノベルズ http://noc.syosetu.com/
+ ムーンライトノベルズ http://mnlt.syosetu.com/
+ ミッドナイトノベルズ http://mid.syosetu.com/
+ ハーメルン https://syosetu.org/
+ Arcadia http://www.mai-net.net/
+ 暁 http://www.akatsuki-novels.com/
+ カクヨム https://kakuyomu.jp/

## セットアップ

セットアップ方法は 2 つあります。通常利用では Release 版を使ってください。Rust 環境があり、自分でビルドしたい場合はリポジトリから実行できます。

### 1. Release からダウンロードして使う

[GitHub Releases](https://github.com/Rumia-Channel/narou.rs/releases) から利用環境に合う配布 zip をダウンロードし、任意の場所に展開してください。

配布 zip は `narou/` ディレクトリをルートに持つ構成です。実行に必要なファイルはその中にまとまっています。

```text
narou/
  narou_rs(.exe)
  narou_rs_updater(.exe).new
  webnovel/
  preset/
  LICENSE
  README.md
  Third-Party-License.md
  commitversion
```

`narou_rs` は、実行ファイルの近くにある `webnovel/`、`preset/`、`commitversion` を参照します。これらを分離しないでください。

Windows では、展開した `narou/` を `Path` に追加してから、小説を管理したいフォルダで `narou_rs init` を実行します。`$narouDir` は自分が zip を展開した `narou/` フォルダ、`$novelDir` は小説を管理したいフォルダに置き換えてください。

```powershell
$narouDir = "C:\Users\your-name\Downloads\narou"
$novelDir = "C:\Users\your-name\Documents\narou-novels"
$narouPath = (Resolve-Path $narouDir).Path
$currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
[Environment]::SetEnvironmentVariable("Path", "$currentPath;$narouPath", "User")
```

`Path` の変更を反映するため、PowerShell を開き直してください。その後、小説を管理したいフォルダに移動して初期化します。

```powershell
New-Item -ItemType Directory -Force -Path $novelDir
Set-Location $novelDir
narou_rs init
```

Linux / macOS でも、展開した `narou/` を `PATH` に追加してから、小説を管理したいディレクトリで `narou_rs init` を実行します。`/absolute/path/to/narou` は展開した `narou/` の絶対パス、`~/narou-novels` は小説を管理したいディレクトリに置き換えてください。

```bash
echo 'export PATH="$PATH:/absolute/path/to/narou"' >> ~/.bashrc
source ~/.bashrc
mkdir -p ~/narou-novels
cd ~/narou-novels
narou_rs init
```

Windows 向けの配布バイナリで `VCRUNTIME140.dll` が見つからない場合は、Microsoft 公式の [最新の Visual C++ 再配布可能パッケージ](https://learn.microsoft.com/cpp/windows/latest-supported-vc-redist) から `Microsoft Visual C++ Redistributable 2015-2022 x64` をインストールしてください。

Linux 向けの配布バイナリは GitHub Actions の Ubuntu 24.04 上で `*-unknown-linux-gnu` ターゲットとしてビルドしています。古い glibc の環境では `GLIBC_2.xx not found` のようなエラーで起動できない場合があるため、その場合は利用環境上で `cargo build --release` して実行してください。

### 2. リポジトリを clone して Rust で実行する

Rust のビルド環境を用意し、リポジトリを clone してからビルドします。

```powershell
$workDir = "C:\Users\your-name\Documents"
Set-Location $workDir
git clone https://github.com/Rumia-Channel/narou.rs.git
Set-Location .\narou.rs
cargo build
cargo run -- init
```

Release と同じ構成の `narou/` フォルダをリポジトリ直下に作る場合は、`cargo local-build` を使います。

```powershell
cargo local-build
```

`cargo local-build` は GitHub Actions の release と同じ構成の `narou/` フォルダを作成します。release ビルドした `narou_rs(.exe)`、`narou_rs_updater(.exe).new`、`webnovel/`、`preset/`、`LICENSE`、`README.md`、`Third-Party-License.md`、`commitversion` を `narou/` に配置します。

作成された `narou/` は Release 版と同じように `Path` に追加し、小説を管理したいフォルダで `narou_rs init` を実行してください。`narou/` の中を作業ディレクトリにはしません。

初期化時に AozoraEpub3 の場所も指定する場合は、次のように実行します。

```powershell
$aozoraDir = "C:\Users\your-name\Documents\AozoraEpub3"
cargo run -- init -p $aozoraDir -l 1.8
```

## 初期化後のディレクトリ

`narou init` を実行すると、作業ディレクトリに主に以下を作成します。

```text
.narou/                  ローカル設定、DB、キュー、タグ色など
小説データ/             ダウンロードした小説データ
webnovel/               ユーザー編集用のサイト定義 YAML
```

あわせて、ホームディレクトリ側の `~/.narousetting/global_setting.yaml` をグローバル設定として使います。

## 基本操作

Release 版や `cargo local-build` で作った `narou/` を使う場合は、`narou_rs` コマンドで操作します。

```powershell
narou_rs init
narou_rs download "https://ncode.syosetu.com/n9669bk/"
narou_rs update
narou_rs convert 1
narou_rs web
```

リポジトリから直接実行する場合は、`narou_rs` の代わりに `cargo run --` を使います。例: `cargo run -- download "https://ncode.syosetu.com/n9669bk/"`。

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
narou_rs download "https://ncode.syosetu.com/s3795b/"
narou_rs download "https://kakuyomu.jp/users/bottyan_1129/collections/16816452219618293895"
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
narou_rs web --hide-console
```

### Web 公開時の上級者向け設定

通常の `web` 利用は localhost 前提です。外部公開や reverse proxy 配下で使う場合だけ、CLI から hidden 設定を変更してください。

```powershell
narou_rs setting server-basic-auth.require-for-external-bind=false
narou_rs setting server-reverse-proxy.enable=true
```

- `server-basic-auth.require-for-external-bind` は、`server-bind=0.0.0.0` など外部公開 bind のときに Basic 認証未設定での起動を拒否する narou_rs 独自ガードです。既定値は `true` です。
- `server-reverse-proxy.enable` は nginx などの前段 proxy が付ける外側の `Host` / `Origin` を受け入れるモードです。既定値は `false` で、reverse proxy 越しに公開するときだけ `true` にしてください。
- どちらも Web UI の設定画面には表示されません。`narou_rs setting <name>=<value>` でのみ変更します。

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
- Windows では `narou_rs web --hide-console` でコンソールを出さずに起動し、タスクトレイから `終了` / `再起動` を選べます。

## 注意点

- `narou init` 前に多くのコマンドを実行しても、初期化を促す表示になります。
- `webnovel/*.yaml` を Rust 側のハードコードより優先する方針です。サイト追従が必要な場合は、まず YAML の更新を検討してください。
- 配布物を移動するときは、実行ファイルだけでなく `webnovel/` と `preset/` も一緒に配置してください。
- `send`、`mail`、AozoraEpub3 連携は、端末や SMTP の実環境設定が前提です。
- `mail` 機能と Kindle / Kobo などの実機送信は、開発者の手元に端末が無いため十分な実地確認ができていません。動作確認や不具合報告、再現情報、修正提案に協力してもらえると助かります。

## 開発用コマンド

```powershell
cargo build
cargo test
cargo check
```

この 3 つで、通常のビルド、テスト、型検査を確認できます。
