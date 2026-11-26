//! The v1-fork search routes: `api/search-indexers`, `adapters/newznab/{token}/api`,
//! `adapters/addon/{token}/manifest.json` (auth, CORS, caps/feed XML, dedupe + ordering).
#![allow(clippy::unwrap_used)] // Test helpers outside `#[test]` fns; panics are the assertions.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use infinidysk::auth::Auth;
use infinidysk::config::ConfigStore;
use infinidysk::search::{self, Search};
use infinidysk_core::ItemId;
use infinidysk_db::Db;
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

/// Minimal loopback Newznab server: replies with the given RSS body, records targets.
async fn newznab_server(body: &'static str) -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
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
                let head = String::from_utf8_lossy(&buf).into_owned();
                let target = head
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_owned();
                t.lock().unwrap().push(target);
                let out = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(out.as_bytes()).await;
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

async fn build_app(indexer_url: &str, profiles_json: &str) -> Router {
    let path = std::env::temp_dir().join(format!("infinidysk-search-{}.sqlite", ItemId::new()));
    let db = Db::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let env = [
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
    ];
    let config = Arc::new(RwLock::new(
        ConfigStore::load(db.clone(), env).await.unwrap(),
    ));
    let auth = Arc::new(Auth::load(db, Arc::clone(&config)).await.unwrap());
    let settings = config.read().await.settings().clone();
    let state = Arc::new(Search {
        service: Arc::new(search::service_from_settings(&settings)),
        config,
        auth,
    });
    search::router(state)
}

const PROFILES: &str = r#"[{"token":"tok","name":"My Profile"}]"#;

#[tokio::test]
async fn manifest_requires_known_token_and_addon_adapter_with_cors() {
    let (addr, _t) = newznab_server(RSS).await;
    let app = build_app(&format!("http://{addr}/api"), PROFILES).await;

    let r = send(&app, "OPTIONS", "/adapters/addon/tok/manifest.json", &[]).await;
    assert_eq!(r.code, StatusCode::NO_CONTENT);
    assert_eq!(
        r.headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "*"
    );
    assert_eq!(
        r.headers.get(header::CACHE_CONTROL).unwrap(),
        "no-store, no-cache, must-revalidate"
    );

    let r = send(&app, "GET", "/adapters/addon/nope/manifest.json", &[]).await;
    assert_eq!(r.code, StatusCode::NOT_FOUND);

    let r = send(&app, "GET", "/adapters/addon/tok/manifest.json", &[]).await;
    assert_eq!(r.code, StatusCode::OK);
    let m = r.json();
    assert_eq!(m["id"], "nzbdav.profile.tok");
    assert_eq!(m["name"], "My Profile");
    assert_eq!(m["resources"], serde_json::json!(["stream"]));
    assert_eq!(m["types"], serde_json::json!(["movie", "series"]));
    assert_eq!(
        m["idPrefixes"],
        serde_json::json!(["tt", "tmdb", "tvdb", "kitsu", "mal", "anilist"])
    );
    assert_eq!(m["behaviorHints"]["configurable"], false);
    assert_eq!(m["behaviorHints"]["configurationRequired"], false);
    assert!(m["version"].as_str().is_some_and(|v| !v.is_empty()));

    // A profile that enables only the newznab adapter must 404 the addon manifest.
    let app2 = build_app(
        &format!("http://{addr}/api"),
        r#"[{"token":"tok","name":"P","enabledAdapters":["newznab"]}]"#,
    )
    .await;
    let r = send(&app2, "GET", "/adapters/addon/tok/manifest.json", &[]).await;
    assert_eq!(r.code, StatusCode::NOT_FOUND);
    let r = send(&app2, "GET", "/adapters/newznab/tok/api?t=caps", &[]).await;
    assert_eq!(r.code, StatusCode::OK);
}

#[tokio::test]
async fn search_indexers_requires_api_key_and_dedupes_sorts() {
    let (addr, targets) = newznab_server(RSS).await;
    let app = build_app(&format!("http://{addr}/api"), PROFILES).await;

    // No key -> 401.
    let r = send(&app, "GET", "/api/search-indexers?q=dune", &[]).await;
    assert_eq!(r.code, StatusCode::UNAUTHORIZED);

    // Missing q -> 400.
    let r = send(&app, "GET", "/api/search-indexers?apikey=sabkey", &[]).await;
    assert_eq!(r.code, StatusCode::BAD_REQUEST);

    // SAB-style query apikey works; results sorted by posted desc, indexer status present.
    let r = send(
        &app,
        "GET",
        "/api/search-indexers?q=dune&apikey=sabkey&limit=9999",
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::OK);
    let body = r.json();
    assert_eq!(body["status"], true);
    assert_eq!(body["indexers"][0]["name"], "a");
    assert_eq!(body["indexers"][0]["ok"], true);
    assert_eq!(body["indexers"][0]["resultCount"], 2);
    let titles: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Dune.2024.2160p.WEB-DL", "Dune.1984.1080p.BluRay"]);
    // limit clamped to 100 max but the indexer got t=search&q=dune&limit=100.
    let recorded = targets.lock().unwrap().clone();
    assert!(recorded[0].contains("t=search"), "{}", recorded[0]);
    assert!(recorded[0].contains("limit=100"), "{}", recorded[0]);
    assert!(recorded[0].contains("apikey=k"), "{}", recorded[0]);

    // Admin key via header also works.
    let r = send(
        &app,
        "GET",
        "/api/search-indexers?q=dune",
        &[("x-api-key", "adminkey")],
    )
    .await;
    assert_eq!(r.code, StatusCode::OK);
}

#[tokio::test]
async fn newznab_adapter_serves_caps_and_movie_feed_with_play_tokens() {
    let (addr, _targets) = newznab_server(RSS).await;
    let app = build_app(&format!("http://{addr}/api"), PROFILES).await;

    // Unknown token -> 404.
    let r = send(&app, "GET", "/adapters/newznab/nope/api?t=caps", &[]).await;
    assert_eq!(r.code, StatusCode::NOT_FOUND);

    // caps advertises the limit.
    let r = send(&app, "GET", "/adapters/newznab/tok/api?t=caps", &[]).await;
    assert_eq!(r.code, StatusCode::OK);
    assert_eq!(
        r.headers.get(header::CONTENT_TYPE).unwrap(),
        "application/xml; charset=utf-8"
    );
    let xml = r.text();
    assert!(
        xml.contains(r#"<limits max="100" default="100"/>"#),
        "{xml}"
    );
    assert!(xml.contains(r#"<tv-search available="yes" supportedParams="imdbid,season,ep"/>"#));

    // t=movie with an imdb id runs the profile search and returns a feed with play-token links.
    let r = send(
        &app,
        "GET",
        "/adapters/newznab/tok/api?t=movie&imdbid=0133093",
        &[],
    )
    .await;
    assert_eq!(r.code, StatusCode::OK);
    let xml = r.text();
    assert!(
        xml.contains("<newznab:response offset=\"0\" total=\"2\"/>"),
        "{xml}"
    );
    assert!(xml.contains("Dune.2024.2160p.WEB-DL"), "{xml}");
    assert!(xml.contains("/api/search/tok/nzb/"), "{xml}");
    assert!(xml.contains(".nzb\""), "{xml}");

    // Unsupported op -> empty feed.
    let r = send(&app, "GET", "/adapters/newznab/tok/api?t=search&q=x", &[]).await;
    assert_eq!(r.code, StatusCode::OK);
    assert!(
        r.text()
            .contains(r#"<newznab:response offset="0" total="0"/>"#)
    );
}
