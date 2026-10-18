# Pixiv の作者 (= ユーザー) 追跡 (2026-09-24)

`narou author add https://www.pixiv.net/users/<id>` で登録し、`narou author check [--dry-run]` と `narou update` の後段で新着作品を追加する。

## 仕組み (すべて site 定義側)

`webnovel/www.pixiv.net.yaml`:

```yaml
author_url: ^https?://\k<domain>/users/(?<user_id>\d+)
author_api_url: \k<top_url>/ajax/user/\k<user_id>/profile/all
author_series_episodes_url: \k<top_url>/ajax/novel/series/\k<series_id>/content_titles
author_series_episodes_pattern: '"id":"(?<novel_id>\d+)"'
author_series_episode_work_url: \k<top_url>/novel/show.php?id=\k<novel_id>
author_comic_series_pages_url: \k<top_url>/ajax/series/\k<series_id>?p=\k<page>&lang=ja
author_comic_series_pages_pattern: '"workId":"(?<novel_id>\d+)"'
author_comic_series_page_work_url: \k<top_url>/artworks/\k<novel_id>
```

`preprocess:` の作者ブランチが、一覧 JSON から以下を emit する:

| 目印 | 意味 |
|---|---|
| `author_novel::<作品URL>` | 追加する作品。単体小説 `/novel/show.php?id=`、小説シリーズ `/novel/series/`、漫画シリーズ `/ajax/series/`、単体イラスト `/artworks/` |
| `author_series::<id>` | 小説シリーズ。`content_titles` で 1 話を除く対象 |
| `author_comic_series::<id>` | 漫画シリーズ。`?p=N`（12 件ずつ、空ページまで）でページを除く対象 |

- `illusts` / `manga` は空のとき `[]` で返るので `!body.illusts.is_array` で守る。
- DSL のオブジェクトはキー昇順 → `/artworks/` は id の文字列昇順に出る (narou_bridge の `sorted()` と同じ)。
- 取得側 (`Downloader::author_works` / `AuthorListing`) は同じDSL/取得policyで目印を読み、上のURLテンプレートまたはURL captureで所属作品を組み立て、URL完全一致で除く。数値IDだけでは比べず、同じ番号の別種作品や親シリーズを保持する。`url_mentions_id` と旧ID-only helperは削除済み。所属を読めなかったシリーズは単体作品を除かず残す。
- Workers の作者確認成功は列挙と子Download投入まで。子は独立したretry/checkpointを持つ。実Worker fixtureで親成功→子403/retryable→続きから成功、次のscheduledで新作追加、管理済みURL除外まで確認した（2026-10-18、未リリース）。

## 実機メモ

- 作者の作品一覧は**ログイン付きのほうが多く返る** (匿名では `novelSeries` が一部しか来ない。R-18 も落ちる)。
- 実測: `/users/6519870` → 30 件 (単体小説 13 / 小説シリーズ 16 / 漫画シリーズ 1、artworks 0 = 唯一の illust がシリーズ内なので除外)。`/users/11` → artworks の目印が 2556 件 (匿名の 2274 件 + R-18 分)。
- `author check` は既定で**実際に DL する**。件数だけ見たいときは `--dry-run`。
- 実効定義はversion gate付き。現在の同梱Pixivは1.5（未リリース）で、古いユーザーコピーはruntimeで無視され、同版以上のキーだけが優先される。Native YAMLはwebnovel、Native SQLiteはオブジェクトストア、WorkerはD1がユーザー定義の保存先。管理APIのraw表示とruntimeの実効マージを混同しない。今回ユーザーの元ライブラリや本番定義は変更していない。
