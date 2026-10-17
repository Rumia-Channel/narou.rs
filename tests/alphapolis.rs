#![cfg(feature = "native-runtime")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use narou_rs::application::download_targets::DownloadTargetExpander;
use narou_rs::downloader::novel_info::NovelInfo;
use narou_rs::downloader::preprocess::{MAX_PREPROCESS_JOBS, PreprocessJobs};
use narou_rs::downloader::section::{SectionCache, download_section};
use narou_rs::downloader::settings::SnapshotDownloaderSettings;
use narou_rs::downloader::site_setting::SiteSetting;
use narou_rs::downloader::toc::{fetch_toc, parse_subtitles};
use narou_rs::downloader::types::{SubtitleInfo, UpdateStatus};
use narou_rs::downloader::{Downloader, DownloaderPlatform};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::mocks::{FakeRateLimiter, MemoryNovelRepository, MemoryObjectStore, MockHttpClient};
use narou_rs::platform::{AssetStore, HttpClient, HttpMethod, HttpRequest, HttpResponse, NovelRepository, ObjectStore, PlatformFuture, SystemClock};
use narou_rs::platform::{CookieStore, HostCookie, LoginGroup};
use serde_json::json;

const TOP: &str = "https://www.alphapolis.co.jp/novel/166360898/202065994";
const BODY: &str = "https://www.alphapolis.co.jp/novel/episode_body";
const TEXTS: [&str; 3] = ["<p>甲の<ruby>物語<rt>ものがたり</rt></ruby></p>", "<p>乙の物語</p>", "<p>丙の物語</p>"];
const ILLUSTRATED: &str = r#"<p>前<img src="/upload/a.png">中<img src="https://images.example/b.jpg">後<img src="data:image/png;base64,ignored"></p>"#;
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6mP0AAAAASUVORK5CYII=";

fn site() -> SiteSetting {
    SiteSetting::load_bundled(&[include_str!("../webnovel/www.alphapolis.co.jp.yaml")]).unwrap().remove(0)
}

fn cover_html(paid: bool, r18: bool, rented: bool) -> String {
    let episode = |id, title, order, public| json!({
        "episodeNo": id, "url": format!("/novel/166360898/202065994/episode/{id}"),
        "mainTitle": title, "dispOrder": order, "upTime": "2026.10.17 11:12", "isPublic": public,
        "rental": if public { serde_json::Value::Null } else { json!({"isFree":false,"isOnRental":rented,"coin":35}) },
    });
    let data = json!({"content":{"id":202065994,"title":"作品の題名","user":{"name":"作者"}},
        "isPartialEpisode":false,"chapterEpisodes":[
            {"title":"","episodes":[episode(99,"序",1,!paid)],"extraEpisode":null},
            {"title":"第一章","episodes":[episode(7,"中",9,true),episode(5,"終",48,true)],"extraEpisode":null},
        ]});
    format!(r#"<script type="application/json" id="app-cover-data">{data}</script>
<script>dataLayer.push({{"gtm_novel_cont_view_array":{{"r_rated_flag":{r18}}}}});</script>
<div class="p-content-info__abstract">あらすじ<br>二行目</div>
<ul class="c-tags p-content-info__tags"><li><a href="/tag/1" class="c-tag">本人のタグ</a></li></ul>
<div class="p-sidebar-content-info__summary"><span class="c-attribute-tag">完結</span><span class="c-attribute-tag">短編</span></div>
<span class="p-sidebar-content-info__detail-label">初回公開日時</span><span>2026.06.27 02:00</span>
<span class="p-sidebar-content-info__detail-label">更新日時</span><span>2026.10.17 11:12</span>
<span class="p-sidebar-content-info__detail-label">文字数</span><span>3,333</span>
<a class="c-tag">他作品のタグ</a><a href="/novel/99/88/episode/777">おすすめ</a>"#)
}

fn subtitle(id: usize) -> SubtitleInfo {
    SubtitleInfo { index:id.to_string(), href:format!("/novel/166360898/202065994/episode/{id}"),
        chapter:String::new(), subchapter:String::new(), subtitle:"話".into(), file_subtitle:"話".into(),
        subdate:String::new(), subupdate:None, download_time:None }
}

#[derive(Default)]
struct ReaderFixture { paid: bool, r18: bool, captcha: bool, illustrated: bool, login_wall: bool, anonymous_toc: AtomicUsize }

impl HttpClient for ReaderFixture {
    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
        Box::pin(async move {
            let signed_in=request.header("Cookie").is_some_and(|cookie|cookie.split(';').any(|part|part.trim()=="alpl_v2_front_session=active"));
            if request.url == TOP && !signed_in {
                self.anonymous_toc.fetch_add(1,Ordering::SeqCst);
                if self.login_wall {
                    return Ok(HttpResponse {status:200, headers:vec![("Content-Type".into(),"text/html".into())],
                        body:br#"<form id="UserLoginForm"></form>"#.to_vec()});
                }
            }
            if request.method == HttpMethod::Get && ["https://www.alphapolis.co.jp/upload/a.png", "https://images.example/b.jpg", "https://images.example/ui.png"].contains(&request.url.as_str()) {
                return Ok(HttpResponse {status:200, headers:vec![("Content-Type".into(),"image/png".into())],
                    body:base64::Engine::decode(&base64::engine::general_purpose::STANDARD,PNG).unwrap()});
            }
            let (status, text, headers) = if request.url == TOP {
                (200, cover_html(self.paid, self.r18, signed_in), vec![("Content-Type".into(),"text/html".into())])
            } else if request.method == HttpMethod::Get && request.url.starts_with(&format!("{TOP}/episode/")) {
                let id = request.url.rsplit('/').next().unwrap();
                if !signed_in && (self.login_wall || (self.paid && id=="99")) {
                    return Ok(HttpResponse {status:403, headers:Vec::new(), body:b"rental or login required".to_vec()});
                }
                (200, format!(r#"<img src="data:image/svg+xml;base64,UI"><img src="https://images.example/ui.png">
<script>$.ajaxSetup({{headers: {{'X-CSRF-TOKEN': "csrf"}}}});
$('div#novelBody').load('{BODY}', {{'episode': {id}, 'token': 'public-token'}});</script>"#),
                    vec![("Set-Cookie".into(), "sid=csrf-session; Path=/; Secure; HttpOnly".into())])
            } else if request.url == BODY && request.method == HttpMethod::Post {
                if request.header("Cookie").is_none_or(|cookie|!cookie.split(';').any(|part|part.trim()=="sid=csrf-session"))
                    || request.header("X-CSRF-TOKEN") != Some("csrf")
                    || request.header("X-Requested-With") != Some("XMLHttpRequest")
                {
                    (419, "CSRF session mismatch".into(), Vec::new())
                } else if self.captcha {
                    (200, "<div class=\"g-recaptcha\"></div>".into(), Vec::new())
                } else if self.illustrated {
                    (200, ILLUSTRATED.into(), Vec::new())
                } else {
                    let fields: HashMap<_,_> = url::form_urlencoded::parse(request.body.as_deref().map(Vec::as_slice).unwrap_or_default()).into_owned().collect();
                    let id = fields.get("episode").and_then(|value| value.parse::<usize>().ok())
                        .ok_or_else(|| NarouError::Http("missing episode".into()))?;
                    (200, TEXTS[id % 3].into(), Vec::new())
                }
            } else {
                return Err(NarouError::Http("unexpected fixture request".into()));
            };
            Ok(HttpResponse {status, headers, body:text.into_bytes()})
        })
    }
}

#[tokio::test]
async fn complete_toc_preserves_array_order_and_own_metadata() {
    let setting=site();
    let source=fetch_toc(&ReaderFixture::default(), &FakeRateLimiter::new(), &setting, TOP, &mut PreprocessJobs::new()).await.unwrap();
    let rows=parse_subtitles(&setting, &source, &HashMap::new()).unwrap();
    assert_eq!(rows.iter().map(|row| row.index.as_str()).collect::<Vec<_>>(), ["99","7","5"]);
    assert_eq!(rows.iter().map(|row| row.chapter.as_str()).collect::<Vec<_>>(), ["","第一章",""]);
    assert_eq!(rows[1].subdate, "2026-10-17 11:12");
    let info=NovelInfo::from_novel_info_source(&setting, &source);
    assert_eq!(info.title.as_deref(), Some("作品の題名"));
    assert_eq!(info.author.as_deref(), Some("作者"));
    assert_eq!(info.tags.as_deref(), Some("本人のタグ"));
    assert_eq!(info.end, Some(true));
    assert_eq!(info.length, Some(3333));
    assert_eq!(info.raw_captures.get("ga").map(String::as_str), Some("3"));
    assert_eq!(info.general_lastup.unwrap().to_rfc3339(), "2026-10-17T02:12:00+00:00");
    assert_eq!(info.novelupdated_at, info.general_lastup);
}

#[tokio::test]
async fn distinct_form_bodies_work_beyond_the_novel_job_cache_limit() {
    let setting=site(); let http=ReaderFixture::default(); let limiter=FakeRateLimiter::new();
    let mut jobs=PreprocessJobs::new(); let mut cache=SectionCache::new();
    for id in 1..=MAX_PREPROCESS_JOBS+1 {
        let (element, _)=download_section(&http, &limiter, &mut cache, &setting, &subtitle(id), TOP, &mut jobs).await.unwrap();
        assert_eq!(element.body.trim(), TEXTS[id % 3], "episode {id}");
    }
}

#[tokio::test]
async fn illustrations_come_from_the_body_not_page_controls_or_data_uris() {
    let store=Arc::new(MemoryObjectStore::new());
    let mut downloader=Downloader::with_platform_and_storage_and_settings_and_support(
        DownloaderPlatform {http:Arc::new(ReaderFixture {illustrated:true,..Default::default()}),
            rate_limiter:Arc::new(FakeRateLimiter::new()), novels:Arc::new(MemoryNovelRepository::new()),
            objects:store.clone(), assets:store.clone(), clock:Arc::new(SystemClock)},
        vec![site()], HashMap::new(), Arc::new(SnapshotDownloaderSettings::new(HashMap::new(),Some(false),HashMap::new())),
    ).unwrap();
    let result=downloader.download_novel(TOP).await.unwrap();
    assert_eq!(result.status,UpdateStatus::Ok);
    let request=narou_rs::platform::ObjectListRequest::new(narou_rs::platform::ObjectPrefix::new("novels/").unwrap(),std::num::NonZeroUsize::new(100).unwrap());
    let page=ObjectStore::list_page(store.as_ref(),&request).await.unwrap();
    let cache=page.objects.iter().find(|object|object.key.as_ref().ends_with("/.illustration_cache.yaml")).expect("saved illustration index");
    let bytes=ObjectStore::read_small(store.as_ref(),&cache.key).await.unwrap().unwrap();
    let index:narou_rs::illustration_store::IllustrationIndex=serde_yaml::from_slice(&bytes).unwrap();
    assert_eq!(index.source_count(),2);
    assert_eq!(index.filename_for_source("https://images.example/ui.png"),None);
    let filename=index.filename_for_source("https://www.alphapolis.co.jp/upload/a.png").expect("body illustration");
    assert_eq!(index.filename_for_source("https://images.example/b.jpg"),Some(filename));
    let prefix=cache.key.as_ref().strip_suffix("/.illustration_cache.yaml").unwrap();
    let image=narou_rs::platform::ObjectKey::try_new(format!("{prefix}/挿絵/{filename}")).unwrap();
    let stored=ObjectStore::read_small(store.as_ref(),&image).await.unwrap().unwrap();
    assert_eq!(stored,base64::Engine::decode(&base64::engine::general_purpose::STANDARD,PNG).unwrap());
}

#[tokio::test]
async fn captcha_is_an_error_not_saved_text() {
    let setting=site(); let limiter=FakeRateLimiter::new();
    let error=download_section(&ReaderFixture {captcha:true,..Default::default()}, &limiter, &mut SectionCache::new(), &setting, &subtitle(1), TOP, &mut PreprocessJobs::new()).await.unwrap_err();
    assert!(matches!(error,NarouError::SiteSetting(_)));
}

#[tokio::test]
async fn r18_consent_denial_leaves_the_library_without_a_novel() {
    let store=Arc::new(MemoryObjectStore::new());
    let objects:Arc<dyn ObjectStore>=store.clone(); let assets:Arc<dyn AssetStore>=store;
    let novels=Arc::new(MemoryNovelRepository::new());
    let mut downloader=Downloader::with_platform_and_storage_and_settings_and_support(
        DownloaderPlatform {http:Arc::new(ReaderFixture {r18:true,..Default::default()}), rate_limiter:Arc::new(FakeRateLimiter::new()),
            novels:novels.clone(), objects, assets, clock:Arc::new(SystemClock)},
        vec![site()], HashMap::new(), Arc::new(SnapshotDownloaderSettings::new(HashMap::new(),Some(false),HashMap::new())),
    ).unwrap();
    let result=downloader.download_novel(TOP).await.unwrap();
    assert!(matches!(result.status,UpdateStatus::Canceled));
    assert_eq!(novels.count(&narou_rs::platform::NovelFilter::default()).await.unwrap(),0);
}

#[tokio::test]
async fn saved_parent_domain_login_is_selected_and_reused_for_protected_alphapolis_content() {
    for (login_wall,paid,error_marker) in [(true,false,true),(true,false,false),(false,true,true)] {
        let http=Arc::new(ReaderFixture {login_wall,paid,..Default::default()});
        let cookies=Arc::new(narou_rs::platform::mocks::MemoryCookieStore::new());
        let groups=["expired","active"].into_iter().enumerate().map(|(index,value)|{
            LoginGroup::new("www.alphapolis.co.jp",vec![HostCookie {host:"alphapolis.co.jp".into(),cookie:format!("alpl_v2_front_session={value}")}])
                .with_id(format!("account-{index}"))
        }).collect::<Vec<_>>();
        cookies.save_groups("www.alphapolis.co.jp",&groups).await.unwrap();
        let store=Arc::new(MemoryObjectStore::new());
        let novels=Arc::new(MemoryNovelRepository::new());
        let setting=if error_marker {site()} else {
            let mut yaml:serde_yaml::Value=serde_yaml::from_str(include_str!("../webnovel/www.alphapolis.co.jp.yaml")).unwrap();
            yaml["error_message"]=serde_yaml::Value::Null;
            SiteSetting::load_bundled(&[&serde_yaml::to_string(&yaml).unwrap()]).unwrap().remove(0)
        };
        let mut downloader=Downloader::with_platform_and_storage_and_settings_and_support(
            DownloaderPlatform {http:http.clone(),rate_limiter:Arc::new(FakeRateLimiter::new()),
                novels:novels.clone(),objects:store.clone(),assets:store,clock:Arc::new(SystemClock)},
            vec![setting],HashMap::new(),Arc::new(SnapshotDownloaderSettings::new(HashMap::new(),Some(false),HashMap::new())),
        ).unwrap();
        let without_login=downloader.download_novel(TOP).await;
        assert!(match &without_login {
            Err(_) => true,
            Ok(result) => matches!(result.status,UpdateStatus::Failed|UpdateStatus::Canceled),
        },"unauthenticated result for wall={login_wall}, paid={paid}, error_marker={error_marker}: {without_login:?}");
        assert_eq!(novels.count(&narou_rs::platform::NovelFilter::default()).await.unwrap(),0);
        let mut downloader=downloader.with_cookie_store(cookies);
        let result=downloader.download_novel(TOP).await.unwrap();
        assert_eq!(result.status,UpdateStatus::Ok);
        let record=novels.find_by_toc_url(TOP).await.unwrap().unwrap();
        assert!(record.requires_login);
        assert_eq!(record.login_session.as_deref(),Some("account-1"));
        let anonymous_before=http.anonymous_toc.load(Ordering::SeqCst);
        let update=downloader.download_novel(TOP).await.unwrap();
        assert_eq!(update.status,UpdateStatus::None);
        assert_eq!(http.anonymous_toc.load(Ordering::SeqCst),anonymous_before);
    }
}

#[tokio::test]
async fn author_pages_follow_each_page_and_ignore_manga_external_and_description_links() {
    let sites=vec![site()]; let http=MockHttpClient::new(); let limiter=FakeRateLimiter::new();
    let author="https://www.alphapolis.co.jp/author/detail/166360898";
    http.add_text(author,200,r#"<div class="content-title"><h2 class="title"><a href="/novel/166360898/11">一</a></h2></div>
<a href="/novel/99/888">プロフィール内リンク</a>
<div class="content-title"><h2 class="title"><a href="/manga/166360898/12">漫画</a></h2></div>
<div class="content-title"><h2 class="title"><a href="https://example.com/novel/1" class="external">外部登録</a></h2></div>
<span class="current">1</span><span><a href="https://www.alphapolis.co.jp/author/detail/166360898?page=2">2</a></span>"#);
    http.add_text(format!("{author}?page=2"),200,r#"<span><a href="https://www.alphapolis.co.jp/author/detail/166360898?page=1">1</a></span><span class="current">2</span>
<div class="content-title"><h2 class="title"><a href="/novel/166360898/13">二</a></h2></div>"#);
    let result=DownloadTargetExpander::new(&http,&limiter,&sites).author_works(author).await.unwrap();
    assert_eq!(result, ["https://www.alphapolis.co.jp/novel/166360898/11", "https://www.alphapolis.co.jp/novel/166360898/13"]);
}
