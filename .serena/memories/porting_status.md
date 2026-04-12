# narou.rb Porting Status (updated 2026-04-12)

## ⚠ 互換性の要件レベル（妥協なし）
- 外部から観測できる挙動の互換性は**妥協せず完璧に**追求する。
- **設定ファイルの位置**: `.narou/local_setting.yaml`、`~/.narousetting/global_setting.yaml` など Ruby 版と同一パス。
- **設定ファイルの読み書き互換**: Rust が書いた YAML を Ruby が読め、Ruby が書いた YAML を Rust が読めること。`---` ヘッダ有無は許容、意味論は一致。
- **全設定項目の読み書き**: Rust 側に未実装機能の設定項目も `narou setting` で読み取り・設定・削除可能。`default.*`、`force.*`、`default_args.*` の動的変数名もすべて受け付ける。
- **CLI の引数・戻り値・エラー・終了コード**: Ruby 版と同一。
- **`webnovel/*.yaml`・`.narou/` 配下のデータ構造**: Ruby 版が読める形式。
- **最終的な変換出力ファイル**: narou.rb の出力と同一。
- 内部実装は異なってよい。上記外部互換性を満たす限り自由。

## ⚠ COMMANDS.md 同期ルール
- `COMMANDS.md` は narou.rb 全24コマンドのオプション・挙動と Rust 側実装状況を管理するマスタードキュメント。
- **コマンドの新規実装・オプション追加・フラグ追加・挙動変更を行うたびに、必ず `COMMANDS.md` の該当箇所をリアルタイムに更新する。**
- 更新内容: Rust 列の ✅/🟡/❌ マーク、実装状況サマリの完了度、不足動作リストの削除・追加。
- 実装が完了したコマンドは「部分」→「完了」に昇格。
- 全24コマンドが narou.rb と完全互換になるまで同期作業を継続。
- **このメモリにも常に最新の実装状況を反映する。**
- **完了判定は細かく行う。** Rust 側に処理や help 定義があるだけでは ✅ 完了にしない。Ruby 版 `sample/narou/lib/command/*.rb` と CLI オプション、help 文、Examples、Configuration/Variable List、設定項目、終了コード、エラー文、未実装の周辺動作を突き合わせ、外部から観測できる挙動が一致していることを確認する。
- `help` は未実装コマンド分も narou.rb から移植する方針。したがって Rust 実装済みコマンドとの比較ではなく、Ruby 版の各 command ファイルの `OptionParser` help と比較して完了判定する。
- 既存の ✅ でも、同じ節に「未実装」「不足動作」が残っている、または Ruby 版との差分がある場合は 🟡 部分へ戻す。完了度は楽観的に維持しない。

## 実装状況サマリ (2026-04-12 時点)

| コマンド | 完了度 | 備考 |
|---------|:------:|------|
| `init` | ✅ 完了 | AozoraEpub3 設定含め完全 |
| `download` | 🟡 部分 | `--force`/`-f`, `--no-convert`/`-n`, `--freeze`/`-z`, `--remove`/`-r` 実装済み。Nコード指定時の `\k<ncode>` 展開修正済み。`--mail`/`-m` スタブ。インタラクティブモード実装済み。 |
| `update` | 🟡 部分 | Ruby版ターゲット解決、既存DBのtoc_url/sitename優先、あらすじ正規化比較、freeze.yaml参照、完結タグ同期、`--gl`主要挙動は実装済み。ただし hotentry、`update.strong`、周辺出力/イベント細部が未完 |
| `convert` | 🟡 部分 | `--device`, `--no-epub`, `--output` 等不足 |
| `list` | 🟡 部分 | `--latest`, `--reverse`, `--url`, `--filter` 等不足 |
| `tag` | 🟡 部分 | `--color`, `--clear`, `--list` 不足 |
| `freeze` | 🟡 部分 | 全オプションは実装済み。ただし `.narou/freeze.yaml` 互換と Ruby版ターゲット解決が未完 |
| `remove` | 🟡 部分 | `--yes`, `--with-file` 不足 |
| `web` | 🟡 部分 | APIのみ。HTML UIなし |
| `setting` | 🟡 部分 | 基本読み書きは実装済み。ただし `default.*`/`force.*`/`default_args.*` のコマンド外部挙動、全設定項目網羅、device関連自動変更が未完 |
| `diff` | ❌ 未実装 | 差分表示 |
| `send` | ❌ 未実装 | USB 経由端末送信 |
| `mail` | ❌ 未実装 | Send-to-Kindle |
| `backup` | ❌ 未実装 | ZIP バックアップ |
| `clean` | ❌ 未実装 | ゴミファイル削除 |
| `help` | 🟡 部分 | トップレベルは概ね実装済み。各コマンド `-h` は Ruby版詳細ヘルプとの差分あり |
| `version` | ❌ 未実装 | `-v`/`--version` も現状は未認識エラー |
| `log` | ❌ 未実装 | ログ表示 |
| `folder` | ❌ 未実装 | フォルダを開く |
| `browser` | ❌ 未実装 | ブラウザで開く |
| `alias` | ❌ 未実装 | ID別名管理 |
| `inspect` | ❌ 未実装 | 小説状態調査 |
| `csv` | ❌ 未実装 | CSV エクスポート/インポート |
| `trace` | ❌ 未実装 | デバッグ用 |

### グローバル機能 (2026-04-12 完了)
- ✅ `--no-color`: NO_COLOR 環境変数設定 + global_setting.yaml の `no-color` キー参照
- ✅ `--multiple`: カンマ区切り引数展開（`multiple-delimiter` 設定対応）
- ✅ `--time`: 実行時間表示（at_exit パターン）
- ✅ `--backtrace`: panic時のフルスタックトレース表示
- ✅ `--user-agent <UA>`: カスタム User-Agent
- 🟡 コマンドショートカット: 解決テーブルは Ruby版と同一の逆順ハッシュ構築。ただし未実装コマンドへ解決されると clap の未認識サブコマンドエラーになるため全24コマンド実装までは部分扱い
- ❌ `-v`/`--version`: version コマンド変換前処理はあるが、Rust 側に `version` サブコマンドが未定義のため現状は未認識エラー
- ✅ 引数なし → help コマンドフォールバック
- ✅ `default_args.<cmd>` 注入: local_setting.yaml から引数なし+TTY時のみ

## 実装優先度
- P0: 既存9コマンドの不足オプション補完 (download flags, list flags, convert flags)
- P1: ユーティリティコマンド (help, version, folder, browser, alias, backup, clean, csv, inspect, log)
- P3: 端末連携 (send, mail, diff)
- ~~P4: グローバル機能 (--no-color, --multiple, --time, --backtrace, ショートカット)~~ ✅ 完了

## 参照データ
- カクヨム: `sample/1177354055617350769 .../kakuyomu_jp_1177354055617350769.txt` (25,273行)
- 出力先: `sample/novel/小説データ/カクヨム/.../output/「先輩の妹じゃありません！」.txt`

## 全修正済みバグ (31件)
AGENTS.md の「全修正済みバグ一覧」を参照。
