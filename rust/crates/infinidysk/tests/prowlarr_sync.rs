//! The Prowlarr pull-sync route and service: `GET|POST /api/prowlarr-sync` (admin API key),
//! merge into the indexer settings, persisted sync status, env-managed guard, failure path.
#![allow(clippy::unwrap_used)] // Test helpers outside `#[test]` fns; panics are the assertions.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use infinidysk::auth::Auth;
use infinidysk::config::ConfigStore;
use infinidysk::prowlarr_sync;
use infinidysk::search;
use infinidysk_core::ItemId;
use infinidysk_db::Db;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tower::ServiceExt;

struct Reply {
    code: StatusCode,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

async fn send(app: &Router, method: &str, uri: &str) -> Reply {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let code = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    Reply { code, body }
}

/// Loopback Prowlarr: `/api/v1/indexer` returns the given JSON; records the API key header.
async fn prowlarr_server(
    indexers_json: &'static str,
) -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let keys = Arc::new(std::sync::Mutex::new(Vec::new()));
    let k = keys.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let k = k.clone();
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
                let key = head
                    .lines()
                    .find_map(|l| {
                        l.split_once(':')
                            .filter(|(n, _)| n.trim().eq_ignore_ascii_case("x-api-key"))
                            .map(|(_, v)| v.trim().to_owned())
                    })
                    .unwrap_or_default();
                k.lock().unwrap().push(format!("{target} key={key}"));
                let body = if target.starts_with("/api/v1/indexer") {
                    indexers_json
                } else {
                    "{}"
                };
                let out = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(out.as_bytes()).await;
            });
        }
    });
    (addr, keys)
}

const REMOTE_INDEXERS: &str = concat!(
    r#"["#,
    r#"{"id":1,"name":"NZBgeek","enable":true,"supportsSearch":true,"protocol":"usenet"},"#,
    r#"{"id":2,"name":"TorrentOnly","enable":true,"supportsSearch":true,"protocol":"torrent"},"#,
    r#"{"id":3,"name":"DisabledUsenet","enable":false,"supportsSearch":true,"protocol":"usenet"}"#,
    r#"]"#
);

async fn build_app(
    prowlarr_url: &str,
    extra_env: &[(&str, &str)],
) -> (Router, Arc<tokio::sync::RwLock<ConfigStore>>) {
    let path = std::env::temp_dir().join(format!("infinidysk-prowlarr-{}.sqlite", ItemId::new()));
    let db = Db::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let mut env: Vec<(String, String)> = vec![
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
            "INFINIDYSK__PROWLARR__URL".to_owned(),
            prowlarr_url.to_owned(),
        ),
        ("INFINIDYSK__PROWLARR__API_KEY".to_owned(), "pk".to_owned()),
        (
            "INFINIDYSK__PROWLARR__SYNC_ENABLED".to_owned(),
            "true".to_owned(),
        ),
    ];
    for (k, v) in extra_env {
        env.push(((*k).to_owned(), (*v).to_owned()));
    }
    let config = Arc::new(RwLock::new(
        ConfigStore::load(db.clone(), env).await.unwrap(),
    ));
    let auth = Arc::new(Auth::load(db, Arc::clone(&config)).await.unwrap());
    let search_state = search::build(Arc::clone(&config), Arc::clone(&auth)).await;
    let sync = prowlarr_sync::build(Arc::clone(&config), search_state, auth);
    (prowlarr_sync::router(sync), config)
}

#[tokio::test]
async fn prowlarr_sync_merges_remote_indexers_and_persists_status() {
    let (addr, keys) = prowlarr_server(REMOTE_INDEXERS).await;
    let (app, config) = build_app(&format!("http://{addr}"), &[]).await;

    // No API key -> 401.
    let r = send(&app, "GET", "/api/prowlarr-sync").await;
    assert_eq!(r.code, StatusCode::UNAUTHORIZED);

    // GET before any sync: configured, but no attempt yet.
    let r = send(&app, "GET", "/api/prowlarr-sync?apikey=adminkey").await;
    assert_eq!(r.code, StatusCode::OK);
    let body = r.json();
    assert_eq!(body["status"], true);
    assert_eq!(body["configured"], true);
    assert_eq!(body["syncEnabled"], true);
    assert_eq!(body["lastAttemptAt"], Value::Null);
    assert_eq!(body["indexerConfigJson"], Value::Null);

    // POST runs the sync now.
    let r = send(&app, "POST", "/api/prowlarr-sync?apikey=sabkey").await;
    assert_eq!(r.code, StatusCode::OK);
    let body = r.json();
    assert_eq!(body["status"], true);
    assert_eq!(body["error"], Value::Null);
    assert_eq!(body["remoteIndexerCount"], 3);
    assert_eq!(body["added"], 1);
    assert_eq!(body["skipped"], 2);
    assert!(body["lastAttemptAt"].as_i64().is_some_and(|v| v > 0));
    assert!(body["lastSuccessAt"].as_i64().is_some_and(|v| v > 0));
    // The response carries the merged indexer config with the API key masked.
    let cfg: Value = serde_json::from_str(body["indexerConfigJson"].as_str().unwrap()).unwrap();
    assert_eq!(cfg["indexers"][0]["name"], "NZBgeek");
    assert_eq!(cfg["indexers"][0]["apiKey"], "********");
    assert_eq!(cfg["indexers"][0]["prowlarrIndexerId"], 1);
    assert!(
        body["profileConfigJson"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );

    // The Prowlarr API key header was sent to the right resource.
    let recorded = keys.lock().unwrap().clone();
    assert!(
        recorded
            .iter()
            .any(|r| r.contains("/api/v1/indexer") && r.contains("key=pk")),
        "{recorded:?}"
    );

    // Persisted: indexers merged into the settings, status stored.
    {
        let store = config.read().await;
        let s = store.settings().clone();
        drop(store);
        assert_eq!(s.indexers.indexers.len(), 1);
        assert_eq!(s.indexers.indexers[0].name, "NZBgeek");
        assert_eq!(s.indexers.indexers[0].prowlarr_indexer_id, Some(1));
        assert_eq!(s.indexers.indexers[0].api_key, "pk");
        let status = s.prowlarr.sync_status.as_ref().expect("status persisted");
        assert_eq!(status.added, 1);
        assert!(status.last_success_at.is_some());
        assert!(status.last_error.is_none());
    }

    // A second POST updates (no new adds) and the search service sees the new indexer.
    let r = send(&app, "POST", "/api/prowlarr-sync?apikey=adminkey").await;
    let body = r.json();
    assert_eq!(body["added"], 0);
    assert_eq!(body["updated"], 0);
    assert_eq!(body["removed"], 0);
}

#[tokio::test]
async fn prowlarr_sync_records_failures_without_touching_indexers() {
    // Nothing listens on this port: the fetch fails and the error is persisted.
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = probe.local_addr().unwrap();
    drop(probe);
    let (app, config) = build_app(&format!("http://{dead}"), &[]).await;

    let r = send(&app, "POST", "/api/prowlarr-sync?apikey=adminkey").await;
    assert_eq!(r.code, StatusCode::OK);
    let body = r.json();
    assert_eq!(body["status"], true);
    assert!(
        body["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{body}"
    );
    assert_eq!(body["added"], 0);
    assert_eq!(body["lastSuccessAt"], Value::Null);
    assert_eq!(body["indexerConfigJson"], Value::Null);

    let s = {
        let store = config.read().await;
        let s = store.settings().clone();
        drop(store);
        s
    };
    let status = s.prowlarr.sync_status.as_ref().expect("status");
    assert!(status.last_error.is_some());
    assert!(status.last_attempt_at > 0);
    assert_eq!(s.indexers.indexers.len(), 0);
}

#[tokio::test]
async fn prowlarr_sync_refuses_to_persist_when_indexers_are_env_managed() {
    let (addr, _keys) = prowlarr_server(REMOTE_INDEXERS).await;
    let (app, config) = build_app(
        &format!("http://{addr}"),
        &[("INFINIDYSK__INDEXERS__INDEXERS", "[]")],
    )
    .await;

    let r = send(&app, "POST", "/api/prowlarr-sync?apikey=adminkey").await;
    assert_eq!(r.code, StatusCode::OK);
    let body = r.json();
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("INFINIDYSK__INDEXERS__INDEXERS")),
        "{body}"
    );
    assert_eq!(body["indexersEnvironmentManaged"], true);
    let n = config.read().await.settings().indexers.indexers.len();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn prowlarr_sync_reports_unconfigured_without_network() {
    let path = std::env::temp_dir().join(format!("infinidysk-prowlarr-{}.sqlite", ItemId::new()));
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
    ];
    let config = Arc::new(RwLock::new(
        ConfigStore::load(db.clone(), env).await.unwrap(),
    ));
    let auth = Arc::new(Auth::load(db, Arc::clone(&config)).await.unwrap());
    let search_state = search::build(Arc::clone(&config), Arc::clone(&auth)).await;
    let sync = prowlarr_sync::build(Arc::clone(&config), search_state, auth);
    let app = prowlarr_sync::router(sync);

    let r = send(&app, "POST", "/api/prowlarr-sync?apikey=adminkey").await;
    assert_eq!(r.code, StatusCode::OK);
    let body = r.json();
    assert_eq!(body["configured"], false);
    assert_eq!(
        body["error"].as_str().unwrap(),
        "Prowlarr URL and API key are required."
    );
}
