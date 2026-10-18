//! `narou author` — authors whose works are tracked.
//!
//! An author is a *watch target*: the store keeps only the site and the author
//! page (`src/author.rs`), and `narou update` checks every entry after the
//! novel pass, downloading the works that are not in the library yet. Which
//! works those are comes from the site definition (`author_url` recognises the
//! page, `author_novel_pattern` lists its works), so a new site needs no code.

use std::collections::HashSet;

use narou_rs::author;
use narou_rs::downloader::Downloader;
use narou_rs::error::{NarouError, Result};

use crate::commands::download::{self, DownloadOptions};

/// Subcommands of `narou author`.
#[derive(clap::Subcommand, Debug)]
pub enum AuthorAction {
    /// Track an author page (e.g. `https://mypage.syosetu.com/2842627/`).
    Add {
        /// Author page URL.
        url: String,
    },
    /// List tracked authors.
    List,
    /// Stop tracking an author (URL, or the number from `list`).
    Remove {
        /// Author page URL, or its number in `narou author list`.
        target: String,
    },
    /// Check every tracked author now and download new works.
    Check {
        /// List the works that would be added without downloading them.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}

/// Run `narou author`.
pub async fn cmd_author(action: AuthorAction) -> Result<()> {
    match action {
        AuthorAction::Add { url } => add(&url),
        AuthorAction::List => list(),
        AuthorAction::Remove { target } => remove(&target),
        AuthorAction::Check { dry_run } => {
            let report = check_tracked_authors(None, dry_run).await?;
            report.print();
            Ok(())
        }
    }
}

/// Register an author page.
fn add(url: &str) -> Result<()> {
    // 実効定義を使う: SQLite モードでは管理 API (`/api/sites`) が編集した
    // 規則がストア側にあり、`SiteSetting::load_all()` (ファイル直読み) では
    // 見えない。YAML モードでも `load_all` と同じ定義になる。
    let settings = narou_rs::downloader::site_setting::effective_site_settings();
    let author = narou_rs::application::authors::author_for_url(url, &settings)?;
    if author::add_author(&author)? {
        println!("作者を登録しました: {} ({})", author.url, author.site);
        println!("  `narou update` のたびに確認し、新しい作品があれば追加します。");
    } else {
        println!("{} は既に登録されています。", author.url);
    }
    Ok(())
}

/// Show the tracked authors.
fn list() -> Result<()> {
    let authors = author::authors_for_current_root()?;
    if authors.is_empty() {
        println!("追跡中の作者はいません。");
        println!("  narou author add https://mypage.syosetu.com/<id>/ で登録できます。");
        return Ok(());
    }
    println!("追跡中の作者: {} 件", authors.len());
    for (index, author) in authors.iter().enumerate() {
        println!("  {}. [{}] {}", index + 1, author.site, author.url);
    }
    Ok(())
}

/// Stop tracking one author.
fn remove(target: &str) -> Result<()> {
    let authors = author::authors_for_current_root()?;
    let url = if let Ok(position) = target.parse::<usize>() {
        authors
            .get(position.checked_sub(1).ok_or_else(|| NarouError::Database(format!("{target} 番目の作者はいません")))?)
            .map(|author| author.url.clone())
            .ok_or_else(|| NarouError::Database(format!("{target} 番目の作者はいません")))?
    } else {
        target.to_string()
    };
    if author::remove_author(&url)? {
        println!("{url} の追跡をやめました。");
    } else {
        println!("{url} は登録されていません。");
    }
    Ok(())
}

/// Outcome of one author sweep, for the CLI and the update command.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuthorCheckReport {
    /// Authors that were checked.
    pub checked: usize,
    /// Works that were not in the library and were downloaded.
    pub added: usize,
    /// Works that were already known (skipped).
    pub known: usize,
    /// Authors whose page or definition could not be used.
    pub failed: usize,
}

impl AuthorCheckReport {
    pub fn print(&self) {
        if self.checked == 0 && self.failed == 0 {
            return;
        }
        println!(
            "作者の確認: {} 件を確認、新規 {} 件を追加 (既知 {} 件)",
            self.checked, self.added, self.known
        );
    }
}

/// Check every tracked author and download the works that are new.
///
/// Runs after the novel pass of `narou update`, so a novel an author has just
/// published is picked up in the same run. Each new work goes through the
/// ordinary download path (`narou download <url>`), which is what records it,
/// tags it and converts it.
pub async fn check_tracked_authors(
    user_agent: Option<&str>,
    dry_run: bool,
) -> Result<AuthorCheckReport> {
    // 新規作品の判定と追加は小説データベースを触るので、他コマンドと同じく
    // ここで初期化しておく (`narou update` から呼ばれる場合は初期化済み)。
    narou_rs::db::init_database()
        .map_err(|error| NarouError::Database(format!("Error initializing database: {error}")))?;
    let authors = author::authors_for_current_root()?;
    let mut report = AuthorCheckReport::default();
    if authors.is_empty() {
        return Ok(report);
    }
    // 一覧の取得は展開器 (Downloader) が持つ実効定義・保存済みログインを
    // 借りた共通実装 (`author_listing::AuthorListing`)。`add` と同じ
    // `author_url` 定義の探索を、`download <作者URL>` の展開とも共用する。
    let downloader = Downloader::with_user_agent(user_agent)?;
    let expander = downloader.download_target_expander();
    let mut seen: HashSet<String> = HashSet::new();

    for author in &authors {
        // 同じ作者ページを複数の定義が扱うことがある (なろう と R18 版)。
        // それぞれの作品一覧 API を叩き、見つかった作品をまとめて扱う
        // (一件も読めなかったときだけ失敗)。
        let urls = match expander.author_works(&author.url).await {
            Ok(urls) => urls,
            Err(error) => {
                eprintln!("作者 {} の確認に失敗しました: {error}", author.url);
                report.failed += 1;
                continue;
            }
        };
        let mut added = 0usize;
        let mut known = 0usize;
        for url in &urls {
            if !seen.insert(url.clone()) {
                // 同じ作品を複数の作者が挙げていても一度だけ処理する。
                continue;
            }
            if download::target_is_known(url) {
                known += 1;
                continue;
            }
            if dry_run {
                // 何が増えるかだけを見せる (数千作品の作者でも確かめられる)。
                println!("  {url}");
                added += 1;
                continue;
            }
            let code = download::cmd_download(
                DownloadOptions {
                    targets: vec![url.clone()],
                    force: false,
                    no_convert: false,
                    freeze: false,
                    remove: false,
                    mail: false,
                    user_agent: user_agent.map(str::to_string),
                },
                &narou_rs::progress::console_sink(),
            )
            .await;
            if code == 0 {
                added += 1;
            } else {
                report.failed += 1;
            }
        }
        report.checked += 1;
        report.added += added;
        report.known += known;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use narou_rs::downloader::site_setting::SiteSetting;

    #[test]
    fn author_pages_are_recognised_from_the_definition() {
        let settings = SiteSetting::load_all().expect("site definitions");
        let setting = settings
            .iter()
            .find(|setting| setting.matches_author_url("https://mypage.syosetu.com/2842627/"))
            .expect("なろう定義が作者ページを認識する");
        assert_eq!(setting.domain, "ncode.syosetu.com");
        assert!(
            settings
                .iter()
                .filter(|setting| setting.matches_author_url("https://ncode.syosetu.com/n1234ab/"))
                .count()
                == 0,
            "作品ページを作者ページと誤認しない"
        );
    }

    #[test]
    fn the_kakuyomu_definition_reads_the_authors_own_works_page() {
        let settings = SiteSetting::load_all().expect("site definitions");
        let setting = settings
            .iter()
            .find(|setting| setting.domain == "kakuyomu.jp")
            .expect("カクヨム定義");
        assert!(setting.matches_author_url("https://kakuyomu.jp/users/sokin"));
        // プロフィールページは他作者の作品も並ぶので、本人の作品一覧ページを取る。
        assert_eq!(
            setting.author_fetch_url("https://kakuyomu.jp/users/sokin"),
            "https://kakuyomu.jp/users/sokin/works"
        );
        // 作品リンクは相対 (id だけ) なので、定義側で絶対 URL を組み立てる。
        let html = r#"
            <a href="/works/1177354054880842657">作品1</a>
            <a href="/works/1177354054880842657/episodes/123">作品1 の話</a>
            <a href="/works/16816452218689030051">作品2</a>
        "#;
        assert_eq!(
            setting.author_novel_urls(html).expect("pattern"),
            vec![
                "https://kakuyomu.jp/works/1177354054880842657".to_string(),
                "https://kakuyomu.jp/works/16816452218689030051".to_string()
            ]
        );
    }

    #[test]
    fn the_r18_definition_reads_works_from_the_mypage() {
        let settings = SiteSetting::load_all().expect("site definitions");
        let setting = settings
            .iter()
            .find(|setting| setting.domain == "novel18.syosetu.com")
            .expect("R18 定義");
        // R18 のマイページ (xmypage.syosetu.com/<x ID>/) を作者ページとして扱う。
        assert!(setting.matches_author_url("https://xmypage.syosetu.com/x8094bm/"));
        assert!(
            !setting.matches_author_url("https://mypage.syosetu.com/2842627/"),
            "通常のマイページはなろう側の定義が扱う"
        );
        // 実ページにある形のリンクから作品 URL を拾う (情報ページは拾わない)。
        let html = r#"
            <a href="https://novel18.syosetu.com/n0316gv/" class="c-novel-list__title">作品1</a>
            <a href="https://novel18.syosetu.com/novelview/infotop/ncode/n0316gv/" class="c-novel-list__novel-info">情報</a>
            <a href="https://novel18.syosetu.com/n9878es/" class="c-novel-list__title">作品2</a>
        "#;
        assert_eq!(
            setting.author_novel_urls(html).expect("pattern"),
            vec![
                "https://novel18.syosetu.com/n0316gv/".to_string(),
                "https://novel18.syosetu.com/n9878es/".to_string()
            ]
        );
    }

    #[test]
    fn the_narou_definition_lists_works_from_its_api() {
        let settings = SiteSetting::load_all().expect("site definitions");
        let setting = settings
            .iter()
            .find(|setting| setting.domain == "ncode.syosetu.com")
            .expect("なろう定義");
        // なろう API の応答: `ncode` は大文字で返る。
        let body = r#"[{"allcount":2},{"ncode":"N5181MT"},{"ncode":"N6275MR"}]"#;
        assert_eq!(
            setting.author_novel_urls(body).expect("pattern"),
            vec![
                "https://ncode.syosetu.com/n5181mt/".to_string(),
                "https://ncode.syosetu.com/n6275mr/".to_string()
            ]
        );
    }

    #[test]
    fn definitions_without_the_pattern_report_nothing() {
        let settings = SiteSetting::load_all().expect("site definitions");
        // 作者ページを持たない定義は何も返さない。
        let setting = settings
            .iter()
            .find(|setting| setting.domain == "www.akatsuki-novels.com")
            .expect("暁 定義");
        assert_eq!(setting.author_novel_urls("<a href=x>"), None);
        assert!(!setting.matches_author_url("https://www.akatsuki-novels.com/users/1"));
    }

    #[test]
    fn pixiv_author_pages_list_novels_and_series() {
        let settings = SiteSetting::load_all().expect("site definitions");
        let setting = settings
            .iter()
            .find(|setting| setting.domain == "www.pixiv.net")
            .expect("pixiv 定義");
        let page = "https://www.pixiv.net/users/6519870";
        assert!(setting.matches_author_url(page));
        assert_eq!(
            setting.author_fetch_url(page),
            "https://www.pixiv.net/ajax/user/6519870/profile/all"
        );

        // preprocess が出す目印から作品を読む。
        let source = concat!(
            "author_novel::https://www.pixiv.net/novel/show.php?id=2594847\n",
            "author_novel::https://www.pixiv.net/novel/series/272850\n",
            "author_series::272850\n",
        );
        assert_eq!(
            setting.author_novel_urls(source),
            Some(vec![
                "https://www.pixiv.net/novel/show.php?id=2594847".to_string(),
                "https://www.pixiv.net/novel/series/272850".to_string(),
            ])
        );
        assert_eq!(setting.author_series_ids(source), vec!["272850"]);

        // シリーズの 1 話を除くための URL は author_url の captures を使う。
        let captures = setting
            .extract_author_url_captures(page)
            .expect("user_id capture");
        assert_eq!(
            setting.author_series_episodes_fetch_url(&captures, "272850"),
            Some("https://www.pixiv.net/ajax/novel/series/272850/content_titles".to_string())
        );
    }

    #[test]
    fn author_registration_follows_the_effective_definitions() {
        // SQLite モードでは管理 API (`/api/sites`) が編集した定義がプロセスの
        // 実効定義になる。`add` はファイル (`webnovel/`) ではなくそれを読む:
        // 一時ディレクトリには定義ファイルが 1 つも無い。
        let temp = tempfile::tempdir().unwrap();
        let _dir = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let custom = SiteSetting::load_bundled(&[r#"
name: CustomAuthor
domain: custom.example
top_url: https://custom.example
url: https://custom\.example/book/(?<ncode>\d+)
author_url: ^https?://custom\.example/user/(?<author_id>\d+)/?
author_novel_pattern: '(?<novel_url>https?://custom\.example/book/\d+)'
sitename: CustomAuthor
toc_url: https://custom.example/book/\k<ncode>
"#])
        .unwrap();
        narou_rs::downloader::site_setting::install_effective_site_settings(custom);

        let added = add("https://custom.example/user/5/");
        let authors = narou_rs::author::authors_for_current_root();
        // 後始末を先に行う (プロセスグローバルなので、失敗しても戻す)。
        narou_rs::downloader::site_setting::install_effective_site_settings(
            SiteSetting::load_all().unwrap_or_default(),
        );

        added.expect("実効定義の作者ページを登録できる");
        let authors = authors.expect("登録済みの作者を読める");
        assert!(
            authors.iter().any(|author| author.url == "https://custom.example/user/5/"
                && author.site == "custom.example"),
            "got {authors:?}"
        );
    }

    #[test]
    fn removing_zero_does_not_remove_the_first_tracked_author() {
        let temp = tempfile::tempdir().unwrap();
        let _dir = crate::test_support::set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let author = narou_rs::author::AuthorRecord::new("example.test", "https://example.test/users/1");
        narou_rs::author::add_author(&author).unwrap();
        assert!(remove("0").is_err());
        assert_eq!(narou_rs::author::authors_for_current_root().unwrap(), vec![author]);
    }
}
