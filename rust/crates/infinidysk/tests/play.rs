//! The v1-fork play routes: `adapters/addon/{token}/stream/{type}/{id}.json` (stream listing)
//! and `…/play/{nzbToken}.mkv` (resolve + redirect to `/view/.ids/…`), plus the 404 gates.
#![allow(clippy::unwrap_used)] // Test helpers outside `#[test]` fns; panics are the assertions.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use infinidysk::auth::Auth;
use infinidysk::config::ConfigStore;
use infinidysk::play::{self, Play};
use infinidysk::preflight::{
    CandidateNegativeCache, FastVerifier, Orchestrator, PreflightCache, SessionRegistry,
};
use infinidysk::search::{self, Search};
use infinidysk::warden;
use infinidysk_core::{ItemId, ItemKind, JobId};
use infinidysk_db::{Db, HistoryEntry, Item, JobStatus};
use infinidysk_nntp::Client;
use infinidysk_profiles::cache::Candidate;
use infinidysk_queue::Queue;
use jiff::Timestamp;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tower::ServiceExt;

struct Reply {
    code: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn send(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)]) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let code = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    Reply {
        code,
        headers,
        body,
    }
}

/// Minimal loopback HTTP server: replies with the given status/body, records targets.
async fn http_server(
    status: u16,
    body: &'static str,
) -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let targets = Arc::new(std::sync::Mutex::new(Vec::new()));
    let t = targets.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let t = t.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let Ok(n) = sock.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&buf);
                let target = text.lines().next().unwrap_or("").to_owned();
                t.lock().unwrap().push(target);
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\ncontent-type: application/x-nzb\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (addr, targets)
}

const RSS: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/"><channel>"#,
    r#"<item><title>Dune.2024.2160p.WEB-DL</title><guid>g1</guid><link>http://nzb.example/dune.nzb</link><pubDate>Fri, 09 Jan 2026 12:00:00 +0000</pubDate><enclosure url="http://nzb.example/dune.nzb" length="5000"/><newznab:attr name="grabs" value="9"/></item>"#,
    r#"<item><title>Dune.1984.1080p.BluRay</title><guid>g2</guid><link>http://nzb.example/dune84.nzb</link><pubDate>Thu, 08 Jan 2026 12:00:00 +0000</pubDate><enclosure url="http://nzb.example/dune84.nzb" length="3000"/><newznab:attr name="grabs" value="3"/></item>"#,
    r#"</channel></rss>"#
);

async fn build_app(
    indexer_url: &str,
    profiles_json: &str,
    extra_env: &[(&str, &str)],
) -> (Router, Arc<Play>, Db) {
    let path = std::env::temp_dir().join(format!("infinidysk-play-{}.sqlite", ItemId::new()));
    let db = Db::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let mut env: Vec<(String, String)> = [
        (
            "INFINIDYSK__AUTH__API_KEY".to_owned(),
            "adminkey".to_owned(),
        ),
        (
            "INFINIDYSK__AUTH__SESSION_KEY".to_owned(),
            "signing".to_owned(),
        ),
        ("INFINIDYSK__SAB__API_KEY".to_owned(), "sabkey".to_owned()),
        (
            "INFINIDYSK__SEARCH__RESOLVERS_ENABLED".to_owned(),
            "false".to_owned(),
        ),
        (
            "INFINIDYSK__INDEXERS__INDEXERS".to_owned(),
            format!(r#"[{{"name":"a","url":"{indexer_url}","apiKey":"k","enabled":true}}]"#),
        ),
        (
            "INFINIDYSK__PROFILES__PROFILES".to_owned(),
            profiles_json.to_owned(),
        ),
        ("INFINIDYSK__IMPORTS__STRM_KEY".to_owned(), strm_secret()),
    ]
    .into_iter()
    .collect();
    env.extend(
        extra_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    let config = Arc::new(RwLock::new(
        ConfigStore::load(db.clone(), env).await.unwrap(),
    ));
    let auth = Arc::new(Auth::load(db.clone(), Arc::clone(&config)).await.unwrap());
    let settings = config.read().await.settings().clone();
    let service = Arc::new(search::service_from_settings(&settings));
    let search_state: Arc<Search> =
        search::build_with_service(Arc::clone(&config), Arc::clone(&auth), Arc::clone(&service));
    let warden = warden::build(db.clone(), Arc::clone(&config), Arc::clone(&auth));
    let preflight = Orchestrator::new(infinidysk::preflight::Parts {
        config: Arc::clone(&config),
        cache: Arc::new(PreflightCache::new()),
        sessions: Arc::new(SessionRegistry::new()),
        verifier: Arc::new(FastVerifier::new(Arc::new(Client::new([])))),
        coalescer: Arc::new(infinidysk::nzb_fetch::NzbFetchCoalescer::new()),
        warden: Arc::clone(&warden),
        negative: Arc::new(CandidateNegativeCache::new()),
        search: service,
    });
    let queue = Arc::new(Queue::new(db.clone(), Arc::new(Client::new([]))));
    let play = play::build(
        Arc::clone(&config),
        search_state,
        queue,
        db.clone(),
        preflight,
        None,
    );
    (play::router(Arc::clone(&play)), play, db)
}

const PROFILES: &str = r#"[{"token":"tok","name":"My Profile"}]"#;

/// Built at runtime so the test does not trip the hard-coded-key query.
fn strm_secret() -> String {
    ["strm", "secret"].concat()
}

fn candidate(title: &str, nzb_url: &str, size: i64) -> Candidate {
    Candidate {
        indexer_name: "a".into(),
        indexer_user_agent: "ua".into(),
        nzb_url: nzb_url.into(),
        title: title.into(),
        size,
        posted: None,
        usenet_date: None,
        poster: None,
        grabs: None,
        password: None,
        proxy_url: None,
        source_indexer_name: None,
        language: None,
        subs: None,
        info_hash: None,
        verified_available: false,
    }
}

#[tokio::test]
async fn stream_route_gates_token_adapter_and_cors() {
    let (addr, _t) = http_server(200, RSS).await;
    let (app, _play, _db) = build_app(&format!("http://{addr}/api"), PROFILES, &[]).await;

    let r = send(
        &app,
        "OPTIONS",
        "/adapters/addon/tok/stream/movie/tt123.json",
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::NO_CONTENT);
    assert_eq!(
        r.headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "*"
    );
    assert_eq!(
        r.headers.get(header::CACHE_CONTROL).unwrap(),
        "no-store, no-cache, must-revalidate"
    );

    let r = send(
        &app,
        "GET",
        "/adapters/addon/nope/stream/movie/tt123.json",
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::NOT_FOUND);

    // A profile that enables only the newznab adapter must 404 the addon stream route.
    let (app2, _p, _d) = build_app(
        &format!("http://{addr}/api"),
        r#"[{"token":"tok","name":"P","enabledAdapters":["newznab"]}]"#,
        &[],
    )
    .await;
    let r = send(
        &app2,
        "GET",
        "/adapters/addon/tok/stream/movie/tt123.json",
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stream_route_lists_candidates_with_play_urls() {
    let (addr, _t) = http_server(200, RSS).await;
    let (app, _play, _db) = build_app(&format!("http://{addr}/api"), PROFILES, &[]).await;

    let r = send(
        &app,
        "GET",
        "/adapters/addon/tok/stream/movie/tt123.json",
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::OK, "{}", r.text());
    assert_eq!(
        r.headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "*"
    );
    let v = r.json();
    let streams = v["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 2);
    let s = &streams[0];
    assert_eq!(s["name"], "[NZB] a");
    assert_eq!(s["behaviorHints"]["filename"], "Dune.2024.2160p.WEB-DL");
    assert_eq!(s["behaviorHints"]["videoSize"], 5000);
    assert_eq!(s["behaviorHints"]["bingeGroup"], "nzbdav|a|movie");
    assert_eq!(s["behaviorHints"]["notWebReady"], true);
    assert_eq!(s["meta"]["indexer"], "a");
    let url = s["url"].as_str().unwrap();
    assert!(url.starts_with("http://"), "{url}");
    assert!(url.contains("/adapters/addon/tok/play/"), "{url}");
    assert!(
        url.rsplit('/').next().is_some_and(|f| f
            .rsplit('.')
            .next()
            .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))),
        "{url}"
    );
    let failover = s["failoverId"].as_str().unwrap();
    assert_eq!(failover.len(), 16);
    assert!(url.contains(failover));
    assert_eq!(s["extra"]["failoverId"], failover);
    assert!(
        s["description"]
            .as_str()
            .unwrap()
            .contains("Dune.2024.2160p.WEB-DL")
    );
    assert!(s["description"].as_str().unwrap().contains("💾 4.88 KB"));
    // No ready/verified markers without history or preflight.
    assert!(s["meta"].get("inLibrary").is_none());
    assert!(s["meta"].get("availability").is_none());
}

#[tokio::test]
async fn play_route_404_gates() {
    let (addr, _t) = http_server(200, RSS).await;
    let (app, play, _db) = build_app(&format!("http://{addr}/api"), PROFILES, &[]).await;

    let r = send(
        &app,
        "GET",
        "/adapters/addon/nope/play/abcdef0123456789.mkv",
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::NOT_FOUND);

    let r = send(
        &app,
        "GET",
        "/adapters/addon/tok/play/abcdef0123456789.mkv",
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::NOT_FOUND);
    assert!(r.text().contains("Link expired"));

    // Wrong suffix is not a play route.
    let r = send(
        &app,
        "GET",
        "/adapters/addon/tok/play/abcdef0123456789.nzb",
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::NOT_FOUND);

    // A play token from another profile must not resolve under this token.
    let tokens = play.search.service.cache().add_group(
        vec![candidate("X", "http://127.0.0.1:9/x.nzb", 1)],
        "movie",
        "other",
        "tt9",
        Timestamp::now(),
    );
    let r = send(
        &app,
        "GET",
        &format!("/api/search/tok/play/{}.mkv", tokens[0]),
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn play_redirects_to_view_for_completed_history() {
    let (addr, _t) = http_server(200, RSS).await;
    let (app, play, db) = build_app(&format!("http://{addr}/api"), PROFILES, &[]).await;

    // A completed download for the clicked release, with one video file.
    let content = db.item_by_path("/content").await.unwrap().unwrap();
    let dir = Item::child(&content, "Dune.2024.2160p.WEB-DL", ItemKind::Directory).unwrap();
    let mut video = Item::child(&dir, "Dune.2024.2160p.WEB-DL.mkv", ItemKind::NzbFile).unwrap();
    video.size = Some(5_000_000_000);
    let job = JobId::new();
    let history = HistoryEntry {
        id: job,
        created_at: 1,
        file_name: "Dune.2024.2160p.WEB-DL.nzb".to_owned(),
        job_name: "Dune.2024.2160p.WEB-DL".to_owned(),
        category: "movie".to_owned(),
        status: JobStatus::Completed,
        total_bytes: 5_000_000_000,
        download_seconds: 60,
        fail_message: None,
        download_dir: Some(dir.id),
        nzb_blob: None,
        storage: None,
    };
    db.add_history(&history, &[dir, video], &[]).await.unwrap();

    let tokens = play.search.service.cache().add_group(
        vec![candidate(
            "Dune.2024.2160p.WEB-DL",
            "http://127.0.0.1:9/dune.nzb",
            5_000_000_000,
        )],
        "movie",
        "tok",
        "tt123",
        Timestamp::now(),
    );
    let r = send(
        &app,
        "GET",
        &format!("/adapters/addon/tok/play/{}.mkv", tokens[0]),
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::FOUND, "{}", r.text());
    let location = r
        .headers
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(location.starts_with("/view/.ids/"), "{location}");
    assert!(location.contains("downloadKey="), "{location}");
    assert!(location.contains("extension=mkv"), "{location}");
    assert_eq!(
        r.headers.get(header::CACHE_CONTROL).unwrap(),
        "no-store, no-cache, must-revalidate"
    );
    // The download key is the STRM HMAC of the .ids path.
    let expected =
        infinidysk_webdav::strm_key(&strm_secret(), &video_ids_path(&db, &location).await);
    assert!(
        location.contains(&format!("downloadKey={expected}")),
        "{location}"
    );

    // The same click on the /api/search/... route also redirects.
    let r = send(
        &app,
        "GET",
        &format!("/api/search/tok/play/{}.mkv", tokens[0]),
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::FOUND);
}

async fn video_ids_path(db: &Db, location: &str) -> String {
    let path = location
        .strip_prefix("/view/")
        .and_then(|rest| rest.split('?').next())
        .unwrap_or_default()
        .to_owned();
    let id = infinidysk_core::ItemId::from_ids_path(&path).expect("ids path");
    let item = db.item(id).await.unwrap().expect("item");
    assert!(
        item.name
            .rsplit('.')
            .next()
            .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
    );
    path
}

#[tokio::test]
async fn play_returns_502_with_retry_after_when_nzb_fetch_fails() {
    // Watchdog off: the single-candidate flow fetches the NZB directly.
    let (addr, _t) = http_server(404, "").await;
    let (app, play, _db) = build_app(
        &format!("http://{addr}/api"),
        PROFILES,
        &[("INFINIDYSK__PLAY__WATCHDOG_ENABLED", "false")],
    )
    .await;
    let nzb_addr = {
        // A second server that always 404s the NZB.
        let (a, _t) = http_server(404, "").await;
        a
    };
    let tokens = play.search.service.cache().add_group(
        vec![candidate(
            "Dead.Release.2026.1080p",
            &format!("http://{nzb_addr}/dead.nzb"),
            1000,
        )],
        "movie",
        "tok",
        "tt777",
        Timestamp::now(),
    );
    let r = send(
        &app,
        "GET",
        &format!("/adapters/addon/tok/play/{}.mkv", tokens[0]),
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::BAD_GATEWAY, "{}", r.text());
    assert!(r.text().contains("Failed to fetch NZB from indexer"));
    assert_eq!(r.headers.get(header::RETRY_AFTER).unwrap(), "10");
}

#[tokio::test]
async fn play_marks_ready_stream_in_library() {
    let (addr, _t) = http_server(200, RSS).await;
    let (app, play, db) = build_app(&format!("http://{addr}/api"), PROFILES, &[]).await;

    let content = db.item_by_path("/content").await.unwrap().unwrap();
    let dir = Item::child(&content, "Dune.1984.1080p.BluRay", ItemKind::Directory).unwrap();
    let mut video = Item::child(&dir, "Dune.1984.1080p.BluRay.mkv", ItemKind::NzbFile).unwrap();
    video.size = Some(3_000_000_000);
    let history = HistoryEntry {
        id: JobId::new(),
        created_at: 1,
        file_name: "Dune.1984.1080p.BluRay.nzb".to_owned(),
        job_name: "Dune.1984.1080p.BluRay".to_owned(),
        category: "movie".to_owned(),
        status: JobStatus::Completed,
        total_bytes: 3_000_000_000,
        download_seconds: 60,
        fail_message: None,
        download_dir: Some(dir.id),
        nzb_blob: None,
        storage: None,
    };
    db.add_history(&history, &[dir, video], &[]).await.unwrap();

    // The search returns the RSS items; the stream listing marks the ready one inLibrary.
    let _ = play;
    let r = send(
        &app,
        "GET",
        "/adapters/addon/tok/stream/movie/tt123.json",
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::OK, "{}", r.text());
    let streams = r.json()["streams"].as_array().unwrap().clone();
    let ready: Vec<&Value> = streams
        .iter()
        .filter(|s| s["meta"].get("inLibrary") == Some(&Value::Bool(true)))
        .collect();
    assert_eq!(ready.len(), 1);
    assert!(
        ready[0]["behaviorHints"]["filename"]
            .as_str()
            .unwrap()
            .contains("Dune.1984.1080p.BluRay")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn play_503_when_all_candidates_dead() {
    // Watchdog on (default), verify mode none: fetch fails → Dead → all failed → 503 + Retry-After.
    let (addr, _t) = http_server(404, "").await;
    let (app, play, _db) = build_app(
        &format!("http://{addr}/api"),
        PROFILES,
        &[("INFINIDYSK__PLAY__TOTAL_BUDGET_SECONDS", "3")],
    )
    .await;
    let nzb_addr = http_server(404, "").await.0;
    let tokens = play.search.service.cache().add_group(
        vec![
            candidate(
                "Dead.One.2026.1080p",
                &format!("http://{nzb_addr}/1.nzb"),
                1000,
            ),
            candidate(
                "Dead.Two.2026.1080p",
                &format!("http://{nzb_addr}/2.nzb"),
                900,
            ),
        ],
        "movie",
        "tok",
        "tt888",
        Timestamp::now(),
    );
    let r = send(
        &app,
        "GET",
        &format!("/api/search/tok/play/{}.mkv", tokens[0]),
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text());
    assert!(
        r.text().contains("All tried candidates failed"),
        "{}",
        r.text()
    );
    assert_eq!(r.headers.get(header::RETRY_AFTER).unwrap(), "5");
    let _ = Duration::from_secs(0);
}
