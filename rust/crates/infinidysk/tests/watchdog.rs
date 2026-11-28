//! The v1-fork watchdog read/clear routes (`/api/get-watchdog-entries`,
//! `/api/clear-watchdog-entries`), API-key gated like v1's `BaseApiController`.
#![allow(clippy::unwrap_used)] // Test helpers outside `#[test]` fns; panics are the assertions.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use infinidysk::auth::Auth;
use infinidysk::config::ConfigStore;
use infinidysk::watchdog;
use infinidysk_core::ItemId;
use infinidysk_db::Db;
use infinidysk_db::watchdog::{NewWatchdogEntry, WatchdogOutcome};
use serde_json::Value;
use tokio::sync::RwLock;
use tower::ServiceExt;

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let code = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn setup() -> (Router, Db) {
    let path = std::env::temp_dir().join(format!("infinidysk-watchdog-{}.sqlite", ItemId::new()));
    let db = Db::connect(&format!("sqlite:{}", path.display()))
        .await
        .unwrap();
    let env = [
        ("INFINIDYSK__AUTH__API_KEY".to_owned(), "adminkey".to_owned()),
        ("INFINIDYSK__AUTH__SESSION_KEY".to_owned(), "signing".to_owned()),
        (
            "INFINIDYSK__PROVIDERS".to_owned(),
            r#"[{"name":"Eweka","hostname":"news.eweka.nl","port":563,"tls":true,"username":"u","password":"p","maxConnections":5,"pipelineDepth":1,"priority":0,"enabled":true}]"#
                .to_owned(),
        ),
    ];
    let config = Arc::new(RwLock::new(
        ConfigStore::load(db.clone(), env).await.unwrap(),
    ));
    let auth = Arc::new(Auth::load(db.clone(), Arc::clone(&config)).await.unwrap());
    let state = watchdog::build(db.clone(), config, auth);
    (watchdog::router(state), db)
}

fn entry(provider_host: Option<&str>) -> NewWatchdogEntry {
    NewWatchdogEntry {
        click_id: "click-1".to_owned(),
        attempted_at: 1_800_000_000_000,
        content_type: "movie".to_owned(),
        requested_title: "Dune".to_owned(),
        candidate_title: "Dune.2024.2160p.WEB-DL".to_owned(),
        indexer_name: "a".to_owned(),
        size: 5000,
        rank_index: 0,
        result: WatchdogOutcome::QueueCompleted,
        fail_reason: None,
        duration_ms: 12,
        is_winner: true,
        provider_host: provider_host.map(str::to_owned),
        queue_item_id: Some("job-1".to_owned()),
        content_group_key: Some("movie:tt1160419".to_owned()),
    }
}

#[tokio::test]
async fn entries_require_the_api_key_and_resolve_nicknames() {
    let (app, db) = setup().await;

    let (code, _) = send(
        &app,
        Request::get("/api/get-watchdog-entries")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);

    let (code, body) = send(
        &app,
        Request::get("/api/get-watchdog-entries?limit=10")
            .header("x-api-key", "adminkey")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["entries"].as_array().unwrap().len(), 0);

    db.watchdog_record(&entry(Some("news.eweka.nl (80%), other.example (20%)")))
        .await
        .unwrap();
    let (code, body) = send(
        &app,
        Request::get("/api/get-watchdog-entries?limit=abc")
            .header("x-api-key", "adminkey")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["outcome"], "QueueCompleted");
    assert_eq!(entries[0]["isWinner"], true);
    assert_eq!(entries[0]["attemptedAtUnix"], 1_800_000_000);
    assert_eq!(
        entries[0]["providerNickname"],
        "Eweka (80%), other.example (20%)"
    );
    assert_eq!(
        entries[0]["providerHost"],
        "news.eweka.nl (80%), other.example (20%)"
    );

    // limit is clamped to 1..=500 (v1 behavior; "abc" falls back to 200).
    let (code, body) = send(
        &app,
        Request::get("/api/get-watchdog-entries?limit=9999")
            .header("x-api-key", "adminkey")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["entries"].as_array().unwrap().len(), 1);

    let (code, _) = send(
        &app,
        Request::post("/api/clear-watchdog-entries")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, body) = send(
        &app,
        Request::post("/api/clear-watchdog-entries")
            .header("x-api-key", "adminkey")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["deleted"], 1);

    let (_, body) = send(
        &app,
        Request::get("/api/get-watchdog-entries")
            .header("x-api-key", "adminkey")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body["entries"].as_array().unwrap().len(), 0);

    db.close().await;
}
