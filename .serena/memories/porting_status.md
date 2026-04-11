# narou.rb Porting Status (updated 2026-04-12)

## ⚠ COMMANDS.md 同期ルール
- `COMMANDS.md` は narou.rb 全24コマンドのオプション・挙動と Rust 側実装状況を管理するマスタードキュメント。
- **コマンドの新規実装・オプション追加・フラグ追加・挙動変更を行うたびに、必ず `COMMANDS.md` の該当箇所をリアルタイムに更新する。**
- 更新内容: Rust 列の ✅/🟡/❌ マーク、実装状況サマリの完了度、不足動作リストの削除・追加。
- 実装が完了したコマンドは「部分」→「完了」に昇格。
- 全24コマンドが narou.rb と完全互換になるまで同期作業を継続。
- **このメモリにも常に最新の実装状況を反映する。**

## 実装状況サマリ (2026-04-12 時点)

| コマンド | 完了度 | 備考 |
|---------|:------:|------|
| `init` | ✅ 完了 | AozoraEpub3 設定含め完全 |
| `download` | 🟡 部分 | `--force`, `--no-convert`, `--freeze` 不足 |
| `update` | 🟡 部分 | `--all/--force/--no-convert/--sort-by` 実装済。`--gl`, `--convert-only-new-arrival`, `--ignore-all` 不足 |
| `convert` | 🟡 部分 | `--device`, `--no-epub`, `--output` 等不足 |
| `list` | 🟡 部分 | `--latest`, `--reverse`, `--url`, `--filter` 等不足 |
| `tag` | 🟡 部分 | `--color`, `--clear`, `--list` 不足 |
| `freeze` | ✅ 完了 | `--list`, `--on` 不足だが基本機能あり |
| `remove` | 🟡 部分 | `--yes`, `--with-file` 不足 |
| `web` | 🟡 部分 | APIのみ。HTML UIなし |
| `setting` | ❌ 未実装 | 設定読み書き・一覧・バリデーション |
| `diff` | ❌ 未実装 | 差分表示 |
| `send` | ❌ 未実装 | USB 経由端末送信 |
| `mail` | ❌ 未実装 | Send-to-Kindle |
| `backup` | ❌ 未実装 | ZIP バックアップ |
| `clean` | ❌ 未実装 | ゴミファイル削除 |
| `help` | ❌ 未実装 | clap --help のみ |
| `version` | ❌ 未実装 | clap --version のみ |
| `log` | ❌ 未実装 | ログ表示 |
| `folder` | ❌ 未実装 | フォルダを開く |
| `browser` | ❌ 未実装 | ブラウザで開く |
| `alias` | ❌ 未実装 | ID別名管理 |
| `inspect` | ❌ 未実装 | 小説状態調査 |
| `csv` | ❌ 未実装 | CSV エクスポート/インポート |
| `trace` | ❌ 未実装 | デバッグ用 |

## 完了済み基盤機能
- DL: なろう(n8858hb, 24セクション), カクヨム(ID=2, 294セクション), syosetu.org
- Convert: なろう版はnarou.rb参照データと完全互換
- カクヨム版: 構造完全一致、+493行の差（auto_indent bug が原因）
- Web API: 30+ endpoints (Axum)
- 3-tier HTTP fetch: curl crate → reqwest → wget
- YAML駆動前処理: pest DSL parser 実装済み
- モジュール分割: 10ファイル → 66ファイル
- 進捗バー (indicatif MultiProgress)
- R18 sitename 動的抽出
- 自動変換パイプライン (DL/Update → Convert)
- update コマンド: 凍結チェック、ターゲット解決(tag:NAME/^tag:NAME)、メタデータ変更検出、ステータスメッセージ、小説間インターバル

## Known Issues
- カクヨム版 auto_indent bug: +493行 (根本原因特定済み、修正方針3案あり)

## 実装優先度
- P0: 既存9コマンドの不足オプション補完 (download flags, list flags, convert flags)
- P1: `setting` コマンド (local_setting/global_setting 読み書き)
- P2: ユーティリティコマンド (help, version, folder, browser, alias, backup, clean, csv, inspect, log)
- P3: 端末連携 (send, mail, diff)
- P4: グローバル機能 (--no-color, --multiple, --time, --backtrace, ショートカット)

## 参照データ
- カクヨム: `sample/1177354055617350769 .../kakuyomu_jp_1177354055617350769.txt` (25,273行)
- 出力先: `sample/novel/小説データ/カクヨム/.../output/「先輩の妹じゃありません！」.txt`

## 全修正済みバグ (31件)
AGENTS.md の「全修正済みバグ一覧」を参照。
