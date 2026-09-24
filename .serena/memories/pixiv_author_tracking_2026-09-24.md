# Pixiv の作者 (= ユーザー) 追跡 (2026-09-24)

`narou author add https://www.pixiv.net/users/<id>` で登録し、`narou author check [--dry-run]` と `narou update` の後段で新着作品を追加する。

## 仕組み (すべて site 定義側)

`webnovel/www.pixiv.net.yaml`:

```yaml
author_url: ^https?://\k<domain>/users/(?<user_id>\d+)
author_api_url: \k<top_url>/ajax/user/\k<user_id>/profile/all
author_series_episodes_url: \k<top_url>/ajax/novel/series/\k<series_id>/content_titles
author_series_episodes_pattern: '"id":"(?<novel_id>\d+)"'
author_comic_series_pages_url: \k<top_url>/ajax/series/\k<series_id>?p=\k<page>&lang=ja
author_comic_series_pages_pattern: '"workId":"(?<novel_id>\d+)"'
```

`preprocess:` の作者ブランチが、一覧 JSON から以下を emit する:

| 目印 | 意味 |
|---|---|
| `author_novel::<作品URL>` | 追加する作品。単体小説 `/novel/show.php?id=`、小説シリーズ `/novel/series/`、漫画シリーズ `/ajax/series/`、単体イラスト `/artworks/` |
| `author_series::<id>` | 小説シリーズ。`content_titles` で 1 話を除く対象 |
| `author_comic_series::<id>` | 漫画シリーズ。`?p=N`（12 件ずつ、空ページまで）でページを除く対象 |

- `illusts` / `manga` は空のとき `[]` で返るので `!body.illusts.is_array` で守る。
- DSL のオブジェクトはキー昇順 → `/artworks/` は id の文字列昇順に出る (narou_bridge の `sorted()` と同じ)。
- 取得側 (`Downloader::author_novel_urls`) は本文を `pretreatment_source` に通してから目印を読み、シリーズ内の話/ページを `url_mentions_id` (数字トークン一致) で取り除く。読めなかったシリーズは除かずに残す。

## 実機メモ

- 作者の作品一覧は**ログイン付きのほうが多く返る** (匿名では `novelSeries` が一部しか来ない。R-18 も落ちる)。
- 実測: `/users/6519870` → 30 件 (単体小説 13 / 小説シリーズ 16 / 漫画シリーズ 1、artworks 0 = 唯一の illust がシリーズ内なので除外)。`/users/11` → artworks の目印が 2556 件 (匿名の 2274 件 + R-18 分)。
- `author check` は既定で**実際に DL する**。件数だけ見たいときは `--dry-run`。
- **サイト定義はライブラリの `webnovel/` が優先**されます。既存ライブラリでは `webnovel/www.pixiv.net.yaml` を更新しないと新機能が効きません (`WebNovel` は更新済み)。
