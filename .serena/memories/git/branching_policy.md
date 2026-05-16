# Git Branching Policy

- 通常の修正・軽微な機能追加・ドキュメント更新は `develop` 上で行う。作業開始前に現在ブランチと作業ツリーを確認し、`main` 上で直接作業しない。
- 大幅な変更、新機能、複数ファイルにまたがる設計変更、長時間かかる検証を伴う作業は、`develop` から機能単位のブランチを作成して進める。
- 機能ブランチ名は内容が分かる短い英数字・ハイフン形式にする。例: `fix-web-concurrency`, `feature-series-url`。
- 機能ブランチでは適切な動作テストを済ませてから `develop` に統合する。統合後も `develop` 上で必要なテストを再実行する。
- `main` への統合は、ユーザーが明示的に依頼した場合、またはリリース作業として明確に合意された場合だけ行う。`develop` は削除せず残す。
- `main` へ統合する前に、`develop` が clean であること、必要なテストが通っていること、バージョン更新や README 更新などリリースに必要な差分が揃っていることを確認する。
- タグ作成はユーザーがバージョン番号を明示した場合だけ行う。タグは `main` のリリースコミットを指すようにし、作成後に push する。
- 実装が一区切りついたら、機能単位で git commit する。無関係な変更をひとつの commit に混ぜず、レビューやロールバックがしやすい粒度に分ける。
- commit 前には `git diff` / `git status` を確認し、ユーザー由来または別作業由来の変更を混ぜない。意図しない整形差分、改行だけの変更、import 並び替えだけの変更を含めない。
- commit メッセージは英語の短い命令形または要約形にする。例: `Fix web download concurrency`, `Document release setup steps`。
- push は原則として作業単位の commit 後に行う。ユーザーが「push しないで」と明示した場合は commit までに留め、push しない。
- `develop` で作業した commit は `origin/develop` に push する。機能ブランチで作業した場合は、そのブランチを push し、`develop` 統合後に `origin/develop` も push する。
- `main` 統合後は `origin/main` を push する。リリースタグを作成した場合はタグも push する。
- `git reset --hard`、`git checkout --`、強制 push、履歴改変 rebase は、ユーザーが明示的に依頼した場合以外は行わない。
- ブランチ削除はユーザーが明示的に依頼した場合だけ行う。特に `develop` は残す。
