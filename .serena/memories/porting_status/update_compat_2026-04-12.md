2026-04-12 update互換の現状を更新。

今回完了したこと:
- `src/downloader/mod.rs`
  - ダイジェスト化検知時の Ruby 互換選択肢 1-8 を実装。
    - 1: このまま更新
    - 2: 更新キャンセル
    - 3: キャンセルして凍結
    - 4: バックアップ作成
    - 5: 最新あらすじ表示
    - 6: ブラウザで開く
    - 7: 保存フォルダを開く
    - 8: 変換
  - `UpdateStatus::Canceled` を実際に返すようにした。
  - section更新時に Ruby 版同様 `本文/cache/<timestamp>/` へ旧sectionを退避するよう実装。
  - 空の cache ディレクトリは削除。
  - `SuspendDownload` を通常失敗ではなくバッチ中断側へ流せるよう command 側と接続。
  - `auto-add-tags` 設定対応を追加。site YAML の `tags` パターンから取得したタグを DB タグへ追加。
  - `confirm_over18` サイトで over18 確認を出し、拒否時は `Canceled` を返すようにした。
    - ただし Ruby 版のような global_setting への永続化はまだ未実装。
- `src/compat.rs`
  - `set_frozen_state()` 追加。`freeze.yaml` と `frozen` タグを同時に更新。
  - `convert_existing_novel()` 追加。update/digest/hotentry 側の変換処理を共通化。
  - 変換後の `copy-to` / 端末送信もこの共通経路に集約。
- `src/commands/update.rs`
  - `Canceled` を扱うように修正。
  - `SuspendDownload` 時は Ruby 版に寄せてバッチ中断側へ unwind。
  - 通常 update の自動変換は `compat::convert_existing_novel()` を使うように変更。
  - hotentry は既存の変換を維持しつつ、copy/send 後段は引き続き動く状態。
- `src/commands/download.rs`
  - `Canceled` を扱うように修正。
  - `SuspendDownload` 時はバッチ中断側へ unwind。
- `src/commands/manage.rs`
  - freeze/unfreeze 時に `freeze.yaml` と `frozen` タグの両方を更新するよう修正。

検証:
- `cargo check` 成功
- `cargo test update -- --nocapture` 成功
  - 既存の update 系ユニットテスト2件通過
  - ただし新規 digest/cache/canceled/auto-add-tags 専用テストはまだ未追加

まだ残っている主な差分:
- `mail hotentry` は未実装（mail コマンド自体未実装）
- `confirm_over18?` の結果を global_setting `over18` に保存して次回以降スキップする Ruby 互換は未実装
- Worker cancel / event 周り / 詳細出力など Ruby の周辺挙動は未確認箇所あり
- hotentry の copy-to grouping / send 周りは Rust 独自 device モデル前提なので Ruby の全 device 実装との完全一致は未確認
- section hash cache 永続化との完全外部互換は未確認

COMMANDS.md は今回の状態に合わせて更新済み。