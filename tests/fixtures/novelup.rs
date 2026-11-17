// Synthetic fixtures for the novelup.plus definition.
//
// The markup mirrors the real pages (TOC page, episode page, author work list)
// but only keeps the parts the definition reads, so the tests exercise the YAML
// rules instead of the site's full HTML. No account data or cookies are here.

/// 目次頁 1 頁目: 章見出し + 2 話 + 作品情報 + タグ + ページ送り。
pub const TOC_PAGE1: &str = r#"
<h1 class="storyTitle">白衣のすきま日常録</h1>
<a class="storyAuthor" href="https://novelup.plus/user/813181327/profile">灯里</a>
<p class="state_lamp d-flex flex-wrap gap-4"><span class="private essay">エッセイ/評論/コラム</span><span class="private">長編</span><span class="private">連載中</span></p>
<p class="totalEpisode">総エピソード数：2話</p>
<div class="novel_synopsis">
<p>医療現場のドタバタ、しんみり、ホラー
二行目</p>
</div>
<table class="storyMeta">
<tr><th>タグ</th><td><a href="https://novelup.plus/search?q=%E5%8C%BB%E7%99%82&amp;search[tag]=1">医療</a><a href="https://novelup.plus/search?q=%E3%83%9B%E3%83%A9%E3%83%BC&amp;search[tag]=1">ホラー</a></td></tr>
<tr><th>初掲載日</th><td>2024年5月29日 7時34分</td></tr>
<tr><th>最終更新日</th><td>2026年11月16日 8時15分</td></tr>
<tr><th>文字数</th><td>46,844文字</td></tr>
</table>
<nav class="pagination_wrapper">
<ul class="pagination">
<li><span class="page">1-100件</span></li>
<li class="active" aria-current="page"><span>1</span></li>
<li><a class="js-gtag" data-label="2ページ目へ" href="https://novelup.plus/story/759726165?p=2">2</a></li>
<li><a class="js-gtag" data-label="最後のページへ" href="https://novelup.plus/story/759726165?p=2" aria-label="最後のページへ">&rsaquo;&rsaquo;</a></li>
</ul>
</nav>
<div class="episodeList">
<div class="episodeListItem chapter">第一部</div>
<div class="episodeListItem ">
<a class="episodeTitle" href="https://novelup.plus/story/759726165/907418396" data-number="001">心臓に悪い</a>
<div class="episodeDate">
<p class="publishDate">24/5/29 7:36</p>
</div>
</div>
<div class="episodeListItem ">
<a class="episodeTitle" href="https://novelup.plus/story/759726165/971245150" data-number="002">可愛いんだかなんだか……</a>
<div class="episodeDate">
<p class="publishDate">26/11/16 8:15</p>
</div>
</div>
</div>
"#;

/// 目次頁 2 頁目 (終端): 次の頁へのリンクが無い。
pub const TOC_PAGE2: &str = r#"
<nav class="pagination_wrapper">
<ul class="pagination">
<li><span class="page">101-102件</span></li>
<li><a class="js-gtag" data-label="1ページ目へ" href="https://novelup.plus/story/759726165?p=1">1</a></li>
<li class="active" aria-current="page"><span>2</span></li>
</ul>
</nav>
<div class="episodeList">
<div class="episodeListItem ">
<a class="episodeTitle" href="https://novelup.plus/story/759726165/122387897" data-number="003">公衆電話</a>
<div class="episodeDate">
<p class="publishDate">26/11/16 8:20</p>
</div>
</div>
</div>
"#;

/// 短編の目次頁: 1 話だけ、`data-number` は空 (話 ID を話数として使う)。
pub const SHORT_TOC: &str = r#"
<h1 class="storyTitle">自惚れ</h1>
<a class="storyAuthor" href="https://novelup.plus/user/891476674/profile">ぴろわんこ</a>
<p class="state_lamp"><span class="private">短編</span><span class="private">完結済</span></p>
<p class="totalEpisode">総エピソード数：1話</p>
<div class="novel_synopsis"><p>酒を飲んで、調子に乗った男がいました。</p></div>
<div class="episodeList">
                           <div class="episodeListItem ">
  <a class="episodeTitle"
    href="https://novelup.plus/story/874968302/766439560"
    data-number="">
    心
  </a>
  <div class="episodeDate">
    <p class="publishDate">26/11/16 8:15</p>
  </div>
</div>
</div>
"#;

/// 話頁: 前書き・本文 (ルビと挿絵) ・後書き、コメント欄のスタンプ画像。
pub const EPISODE: &str = r#"
<div class="novel_foreword"><p>前書きの一行目
前書きの二行目</p></div>
<div class="episode_title"><h1>心臓に悪い</h1></div>
<div class="content">
<p id="episode_content">本文の一行目

本文の二行目<ruby><rb>漢字</rb><rp>(</rp><rt>かんじ</rt><rp>)</rp></ruby>
<span class="illust"><img src="https://novelup.plus/uploads/1/2.jpg" alt="挿絵"></span>
</p>
</div>
<div class="novel_afterword"><p class="afterword">後書きの一行目
後書きの二行目</p></div>
<div class="comment_item">
<picture>
<source srcset="https://novelup.plus/s3/stamp/8007.webp">
<img loading="lazy" src="https://novelup.plus/s3/stamp/8007.png" alt="スタンプ">
</picture>
</div>
"#;

/// 作者の作品一覧 1 頁目 (作品カード + 次の頁へのリンク)。
pub const AUTHOR_WORKS_PAGE1: &str = r#"
<div class="sumally ">
  <p class="story_name"><a href="https://novelup.plus/story/759726165" data-ga4-action="transition" data-ga4-option="{&quot;section&quot;:&quot;production_card&quot;}">白衣のすきま日常録</a></p>
  <p class="story_comment">医療現場のドタバタ、しんみり、ホラー</p>
  <p class="story_author_name"><a href="https://novelup.plus/user/813181327/profile" data-ga4-action="transition">灯里</a></p>
  <p class="read_button"><a href="https://novelup.plus/story/759726165" class="button default no-arrow"><span>この作品を読む</span></a></p>
  <div class="story_info">
    <p class="story_genre essay">エッセイ/評論/コラム</p>
    <p class="story_short">長編</p>
    <p class="story_episode_count">66話</p>
    <p class="story_length">46,844字</p>
  </div>
  <p class="story_update">2026年11月16日更新</p>
</div>
<div class="sumally ">
  <p class="story_name"><a href="https://novelup.plus/story/590182848" data-ga4-action="transition">冥界往還奇譚</a></p>
  <p class="story_author_name"><a href="https://novelup.plus/user/813181327/profile" data-ga4-action="transition">灯里</a></p>
  <p class="read_button"><a href="https://novelup.plus/story/590182848" class="button default no-arrow"><span>この作品を読む</span></a></p>
</div>
<nav class="pagination_wrapper">
  <ul class="pagination">
    <li>
      <span class="page">
        1-2件
      </span>
    </li>
    <li class="active" aria-current="page"><span>1</span></li>
    <li><a class="js-gtag" data-category="作者情報（作品）" data-action="クリック" data-label="2ページ目へ" href="https://novelup.plus/user/813181327/story?sort=1&amp;p=2">2</a></li>
    <li><a class="js-gtag" data-category="作者情報（作品）" data-action="クリック" data-label="最後のページへ" href="https://novelup.plus/user/813181327/story?sort=1&amp;p=2" aria-label="最後のページへ">››</a></li>
  </ul>
</nav>
"#;

/// 作者の作品一覧 2 頁目 (終端): 次の頁へのリンクが無い。
pub const AUTHOR_WORKS_PAGE2: &str = r#"
<div class="sumally ">
  <p class="story_name"><a href="https://novelup.plus/story/439593115" data-ga4-action="transition">灯里の5～7行小説</a></p>
  <p class="story_author_name"><a href="https://novelup.plus/user/813181327/profile" data-ga4-action="transition">灯里</a></p>
  <p class="read_button"><a href="https://novelup.plus/story/439593115" class="button default no-arrow"><span>この作品を読む</span></a></p>
</div>
<nav class="pagination_wrapper">
  <ul class="pagination">
    <li>
      <span class="page">
        3-3件
      </span>
    </li>
    <li><a class="js-gtag" data-category="作者情報（作品）" data-action="クリック" data-label="1ページ目へ" href="https://novelup.plus/user/813181327/story?sort=1&amp;p=1">1</a></li>
    <li class="active" aria-current="page"><span>2</span></li>
  </ul>
</nav>
"#;

/// プロフィール頁: 作品の一部と「作品」タブへのリンクだけが載る。
pub const AUTHOR_PROFILE: &str = r#"
<h1>灯里</h1>
<div class="tab_set type_period">
  <a class="profile page" href="https://novelup.plus/user/813181327/profile"><div><p>プロフィール</p></div></a>
  <a class="works " href="https://novelup.plus/user/813181327/story"><div><p>作品</p></div></a>
</div>
<div class="title_tag">
  <h2>作品（13件）</h2>
  <a href="https://novelup.plus/user/813181327/story" class="button_set see_more arrow white"><div><p>一覧</p></div></a>
</div>
<div class="sumally ">
  <p class="story_name"><a href="https://novelup.plus/story/759726165" data-ga4-action="transition">白衣のすきま日常録</a></p>
  <p class="story_author_name"><a href="https://novelup.plus/user/813181327/profile" data-ga4-action="transition">灯里</a></p>
</div>
"#;
