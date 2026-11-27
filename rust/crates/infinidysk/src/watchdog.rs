//! v1-fork play watchdog read/clear routes, ported from
//! `backend/Api/Controllers/GetWatchdogEntries` and `ClearWatchdogEntries`.
//! API-key gated like v1's `BaseApiController` (the watchdog page reads them).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::RwLock;

use infinidysk_db::Db;
use infinidysk_db::watchdog::WatchdogOutcome;

use crate::auth::Auth;
use crate::config::ConfigStore;
use crate::search;

/// Shared state for the watchdog routes.
pub struct Watchdog {
    pub config: Arc<RwLock<ConfigStore>>,
    pub auth: Arc<Auth>,
    pub db: Db,
}

/// Builds the watchdog route state.
#[must_use]
pub fn build(db: Db, config: Arc<RwLock<ConfigStore>>, auth: Arc<Auth>) -> Arc<Watchdog> {
    Arc::new(Watchdog { config, auth, db })
}

/// Router for the v1 watchdog admin routes.
pub fn router(state: Arc<Watchdog>) -> Router {
    Router::new()
        .route("/api/get-watchdog-entries", get(get_entries))
        .route("/api/clear-watchdog-entries", post(clear_entries))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            require_api_key,
        ))
        .with_state(state)
}

async fn require_api_key(State(state): State<Arc<Watchdog>>, req: Request, next: Next) -> Response {
    let allowed = search::api_key_allowed(
        &state.config,
        &state.auth,
        req.headers(),
        req.extensions(),
        req.uri().query(),
    )
    .await;
    if allowed {
        return next.run(req).await;
    }
    (StatusCode::UNAUTHORIZED, "API Key Required").into_response()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EntryDto {
    click_id: String,
    attempted_at_unix: i64,
    content_type: String,
    requested_title: String,
    candidate_title: String,
    indexer_name: String,
    size: i64,
    rank_index: i32,
    outcome: &'static str,
    fail_reason: Option<String>,
    duration_ms: i64,
    is_winner: bool,
    provider_host: Option<String>,
    provider_nickname: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GetEntriesResponse {
    status: bool,
    entries: Vec<EntryDto>,
}

/// v1 `GetWatchdogEntriesController`: recent entries, newest first; provider hosts are
/// resolved to nicknames from the current config (v1 `Nickname`, here the provider `name`).
async fn get_entries(
    State(state): State<Arc<Watchdog>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .map_or(200, |n| n.clamp(1, 500));
    let nicknames_by_host: HashMap<String, String> = {
        let store = state.config.read().await;
        store
            .settings()
            .providers
            .iter()
            .filter(|p| !p.name.trim().is_empty())
            .map(|p| (p.hostname.to_lowercase(), p.name.clone()))
            .collect()
    };
    let Ok(recent) = state.db.watchdog_list(limit).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": false, "error": "database error"})),
        )
            .into_response();
    };
    let entries = recent
        .into_iter()
        .map(|a| EntryDto {
            click_id: a.click_id,
            attempted_at_unix: a.attempted_at,
            content_type: a.content_type,
            requested_title: a.requested_title,
            candidate_title: a.candidate_title,
            indexer_name: a.indexer_name,
            size: a.size,
            rank_index: a.rank_index,
            outcome: WatchdogOutcome::from_i16(a.result).label(),
            fail_reason: a.fail_reason,
            duration_ms: a.duration_ms,
            is_winner: a.is_winner,
            provider_nickname: resolve_nickname(a.provider_host.as_deref(), &nicknames_by_host),
            provider_host: a.provider_host,
        })
        .collect();
    Json(GetEntriesResponse {
        status: true,
        entries,
    })
    .into_response()
}

/// v1 `ResolveNickname`: `provider_host` may be `host` or `host (60%), other (40%)`;
/// each host is replaced by its nickname, or kept when none is configured.
fn resolve_nickname(
    provider_host: Option<&str>,
    nicknames_by_host: &HashMap<String, String>,
) -> Option<String> {
    let host = provider_host?.trim();
    if host.is_empty() || nicknames_by_host.is_empty() {
        return None;
    }
    let mut any_matched = false;
    let rendered: Vec<String> = host
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|part| {
            let (bare, suffix) = match part.rfind(" (") {
                Some(i)
                    if part.ends_with(')')
                        && part[i + 2..].trim_end_matches(')').ends_with('%') =>
                {
                    (&part[..i], &part[i..])
                }
                _ => (part, ""),
            };
            if let Some(nick) = nicknames_by_host.get(&bare.to_lowercase())
                && !nick.trim().is_empty()
            {
                any_matched = true;
                return format!("{nick}{suffix}");
            }
            part.to_owned()
        })
        .collect();
    any_matched.then(|| rendered.join(", "))
}

/// v1 `ClearWatchdogEntriesController`: delete all entries, report the count.
async fn clear_entries(State(state): State<Arc<Watchdog>>) -> Response {
    match state.db.watchdog_clear().await {
        Ok(deleted) => {
            Json(serde_json::json!({"status": true, "deleted": deleted})).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": false, "error": "database error"})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nicknames() -> HashMap<String, String> {
        HashMap::from([
            ("news.eweka.nl".to_owned(), "Eweka".to_owned()),
            ("news.frugal.com".to_owned(), "Frugal".to_owned()),
        ])
    }

    #[test]
    fn nickname_resolution_replaces_hosts_and_keeps_percent_suffix() {
        let n = nicknames();
        assert_eq!(
            resolve_nickname(Some("news.eweka.nl (60%), news.frugal.com (40%)"), &n),
            Some("Eweka (60%), Frugal (40%)".to_owned())
        );
        // Case-insensitive on host, like v1.
        assert_eq!(
            resolve_nickname(Some("NEWS.EWEKA.NL"), &n),
            Some("Eweka".to_owned())
        );
        // Unmatched parts stay; null only when nothing matched.
        assert_eq!(
            resolve_nickname(Some("news.eweka.nl, other.example"), &n),
            Some("Eweka, other.example".to_owned())
        );
        assert_eq!(resolve_nickname(Some("other.example"), &n), None);
        assert_eq!(resolve_nickname(None, &n), None);
        assert_eq!(resolve_nickname(Some("  "), &n), None);
        assert_eq!(
            resolve_nickname(Some("news.eweka.nl"), &HashMap::new()),
            None
        );
    }
}
