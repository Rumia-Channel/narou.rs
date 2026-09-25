# narou.rs 移植状況（2026-09 更新）

最新の正本はリポジトリの `AGENTS.md` と `COMMANDS.md`。古い日付の監査表ではなく、各コマンドの当該節と実装を確認する。

- narou.rb 由来 24 コマンドと Rust 拡張 `db` / `illust` / `login` の計 27 コマンド。✅ 23、🟡 4（`download` / `update` / `convert` / `web`）、❌ 0。`db` はトップレベル help 一覧の 26 件には載っていない。完了印は Ruby 版との全側面の突き合わせが必要であり、実装があるだけで昇格させない。
- Cargo.toml の現行バージョンは 0.4.3（リリース済み）。Web UI の 0.4.3 新機能ツアーは、この版以降にアップデートした利用者に表示される。
- 既定ストレージは YAML。SQLite は Web UI のツアーまたは `.narou/storage-backend` による選択制。ローカル設定などは SQLite 時 `app_state`、`~/.narousetting/global_setting.yaml` は SQLite 選択時もファイルのまま。`narou-compat=true` はローカル YAML の前方互換モード。
- `login` は複数資格情報・順序変更・セッション ID・暗号化保存・Web UI 管理に対応し、`narou login -h` も `add` / `order` / `clear --index` を掲載済み。`clear --index` は host 必須（省略時はエラー）。配布 zip には署名済みの `narou_rs_login` を同梱し、`scripts/package-release.ps1` が Windows 実行ファイルの Authenticode 署名を検証する。
- `COMMANDS.md`、`AGENTS.md` の変更と同時に、このメモリの要約も同期する。`sample/narou` はローカルの Ruby 参照ソースで、必ず存在するとは限らない。