# narou.rb Porting Status (updated 2026-04-11)

## Completed
- 全CLIサブコマンド実装: init, download, update, convert, list, tag, freeze, remove, web
- DL: なろう(n8858hb, 24セクション), カクヨム(ID=2, 294セクション), syosetu.org
- Convert: なろう版はnarou.rb参照データと完全互換
- カクヨム版: 構造完全一致、+493行の差（auto_indent bug が原因）
- Web API: 30+ endpoints (Axum)
- 3-tier HTTP fetch: curl crate → reqwest → wget
- YAML駆動前処理: pest DSL parser 実装済み
- モジュール分割: 10ファイル → 66ファイル (2026-04-11、詳細は project_structure/module_split_2026-04-11)

## Known Issues
- カクヨム版 auto_indent bug: +493行 (根本原因特定済み、修正方針3案あり)
  - `auto_indent` の regex が `\n` にマッチして `\u{3000}\n` に変換
  - 各セクションで1行ずつ増え、全体で+493行

## Next Priorities
- P0: auto_indent bug 修正
- P1: YAML駆動前処理の完成 (kakuyomu_preprocess 暫定ハードコード除去)
- P1: Unit/integration tests for DSL and converter parity
- P2: Queue worker, clippy/fmt cleanup

## 参照データ
- カクヨム: `sample/1177354055617350769 .../kakuyomu_jp_1177354055617350769.txt` (25,273行)
- 出力先: `sample/novel/小説データ/カクヨム/.../output/「先輩の妹じゃありません！」.txt`

## 全修正済みバグ (31件)
AGENTS.md の「全修正済みバグ一覧」を参照。
