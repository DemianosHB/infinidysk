//! Prowlarr pull-sync background service and status route, ported from
//! `backend/Services/ProwlarrSyncService.cs` and `backend/Api/Controllers/ProwlarrSync`
//! (v1 fork): periodic (and on-config-change) pull of the Prowlarr indexer list, merged
//! into the indexer/profile settings via `infinidysk_indexer::sync::merge`, with the last
//! outcome persisted in `prowlarr.sync_status`.
//!
//! Deviations from v1: the `OnConfigChanged` trigger is observed on the 30 s tick (plus a
//! `Notify` on every settings save), so a connection change can take up to one tick to
//! start a sync; the sync gate is a `tokio::sync::Semaphore`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use jiff::Timestamp;
use serde::Serialize;
use tokio::sync::{RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use infinidysk_indexer::prowlarr::ProwlarrClient;
use infinidysk_indexer::{Error as IndexerError, sync};

use crate::auth::Auth;
use crate::config::{ConfigStore, MASK, ProwlarrSyncStatus, Settings};
use crate::search::{Search, api_key_allowed, search_settings};

/// v1 `StartupDelay`.
const STARTUP_DELAY: Duration = Duration::from_secs(15);
/// v1 `TickInterval`.
const TICK_INTERVAL: Duration = Duration::from_secs(30);

const INDEXERS_INSTANCES: [&str; 2] = ["indexers", "indexers"];
const PROFILES_INSTANCES: [&str; 2] = ["profiles", "profiles"];

/// Shared state for the Prowlarr sync loop and the status route.
pub struct ProwlarrSync {
    config: Arc<RwLock<ConfigStore>>,
    search: Arc<Search>,
    auth: Arc<Auth>,
    gate: Semaphore,
    /// Connection tuple last seen by the loop, for the v1 `OnConfigChanged` trigger.
    last_connection: Mutex<Option<(String, String, bool, u32)>>,
}

#[must_use]
pub fn build(
    config: Arc<RwLock<ConfigStore>>,
    search: Arc<Search>,
    auth: Arc<Auth>,
) -> Arc<ProwlarrSync> {
    Arc::new(ProwlarrSync {
        config,
        search,
        auth,
        gate: Semaphore::new(1),
        last_connection: Mutex::new(None),
    })
}

pub fn router(state: Arc<ProwlarrSync>) -> Router {
    Router::new()
        .route("/api/prowlarr-sync", get(snapshot).post(sync_now))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            require_api_key,
        ))
        .with_state(state)
}

/// v1 `ApiKeyValidator` (same rule as the search routes).
async fn require_api_key(
    State(state): State<Arc<ProwlarrSync>>,
    req: Request,
    next: Next,
) -> Response {
    let allowed = api_key_allowed(&state.config, &state.auth, &req).await;
    if allowed {
        return next.run(req).await;
    }
    (StatusCode::UNAUTHORIZED, "API Key Required").into_response()
}

/// v1 `ProwlarrSyncResponse` (+ `BaseApiResponse`).
#[derive(Debug, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the v1 response fields 1:1"
)]
#[serde(rename_all = "camelCase")]
pub struct SyncResponse {
    status: bool,
    error: Option<String>,
    configured: bool,
    sync_enabled: bool,
    indexers_environment_managed: bool,
    profiles_environment_managed: bool,
    last_attempt_at: Option<i64>,
    last_success_at: Option<i64>,
    remote_indexer_count: i32,
    added: i32,
    updated: i32,
    removed: i32,
    skipped: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    indexer_config_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_config_json: Option<String>,
}

fn build_response(
    settings: &Settings,
    status: &ProwlarrSyncStatus,
    current_error: Option<String>,
) -> SyncResponse {
    SyncResponse {
        status: true,
        error: current_error.or_else(|| status.last_error.clone()),
        configured: !settings.prowlarr.url.trim().is_empty()
            && !settings.prowlarr.api_key.0.is_empty(),
        sync_enabled: settings.prowlarr.sync_enabled,
        indexers_environment_managed: false, // filled by the caller with env_managed()
        profiles_environment_managed: false,
        last_attempt_at: (status.last_attempt_at > 0).then_some(status.last_attempt_at),
        last_success_at: status.last_success_at,
        remote_indexer_count: status.remote_indexer_count,
        added: status.added,
        updated: status.updated,
        removed: status.removed,
        skipped: status.skipped,
        indexer_config_json: None,
        profile_config_json: None,
    }
}

fn snapshot_of(store: &ConfigStore, current_error: Option<String>) -> SyncResponse {
    let s = store.settings();
    let mut r = build_response(
        s,
        &s.prowlarr.sync_status.clone().unwrap_or_default(),
        current_error,
    );
    r.indexers_environment_managed = store.env_managed(INDEXERS_INSTANCES);
    r.profiles_environment_managed = store.env_managed(PROFILES_INSTANCES);
    r
}

/// `GET /api/prowlarr-sync`: the last pull-sync status, without network access (v1 GET).
async fn snapshot(State(state): State<Arc<ProwlarrSync>>) -> Response {
    let store = state.config.read().await;
    Json(snapshot_of(&store, None)).into_response()
}

/// `POST /api/prowlarr-sync`: run a sync now (v1 POST); a successful sync also returns the
/// masked indexer config and the profile config so the UI can refresh.
async fn sync_now(State(state): State<Arc<ProwlarrSync>>) -> Response {
    let ct = CancellationToken::new();
    let mut response = state.sync_now(&ct).await;
    if response.error.is_none() {
        let store = state.config.read().await;
        let s = store.settings();
        let indexer_json = masked_indexers_json(s);
        let profile_json = serde_json::to_string(&s.profiles).unwrap_or_default();
        drop(store);
        response.indexer_config_json = Some(indexer_json);
        response.profile_config_json = Some(profile_json);
    }
    Json(response).into_response()
}

/// v1 `ConfigSecretMasker` for `indexers.instances`: every non-empty `apiKey` becomes MASK.
fn masked_indexers_json(s: &Settings) -> String {
    let mut value = serde_json::to_value(&s.indexers).unwrap_or(serde_json::Value::Null);
    if let Some(list) = value.get_mut("indexers").and_then(|a| a.as_array_mut()) {
        for item in list {
            if let Some(key) = item.get_mut("apiKey")
                && let Some(text) = key.as_str()
                && !text.is_empty()
            {
                *key = serde_json::Value::String(MASK.to_owned());
            }
        }
    }
    serde_json::to_string(&value).unwrap_or_default()
}

impl ProwlarrSync {
    /// v1 `SyncNowAsync`: one sync at a time behind the gate.
    pub async fn sync_now(&self, ct: &CancellationToken) -> SyncResponse {
        let Ok(_permit) = self.gate.acquire().await else {
            let store = self.config.read().await;
            return snapshot_of(&store, None);
        };
        let (url, api_key) = {
            let store = self.config.read().await;
            let s = store.settings();
            let out = (s.prowlarr.url.clone(), s.prowlarr.api_key.0.clone());
            drop(store);
            out
        };
        if url.trim().is_empty() || api_key.is_empty() {
            let store = self.config.read().await;
            return snapshot_of(
                &store,
                Some("Prowlarr URL and API key are required.".to_owned()),
            );
        }
        let indexers_env = {
            let store = self.config.read().await;
            store.env_managed(INDEXERS_INSTANCES)
        };
        if indexers_env {
            return self
                .fail(
                    "Indexer settings are managed by INFINIDYSK__INDEXERS__INDEXERS, so Prowlarr sync cannot persist changes.".to_owned(),
                    Timestamp::now().as_second(),
                    ct,
                )
                .await;
        }

        let attempted_at = Timestamp::now().as_second();
        let client = match ProwlarrClient::new(&url, &api_key) {
            Ok(c) => c,
            Err(e) => return self.fail(e.to_string(), attempted_at, ct).await,
        };
        match client.get_indexers(ct).await {
            Ok(remote) => {
                self.persist_success(&url, &api_key, &remote, attempted_at, ct)
                    .await
            }
            Err(IndexerError::Cancelled) if ct.is_cancelled() => {
                let store = self.config.read().await;
                snapshot_of(&store, None)
            }
            Err(e) => {
                warn!(reason = %e, "Prowlarr indexer sync failed");
                self.fail(e.to_string(), attempted_at, ct).await
            }
        }
    }

    /// v1 `PersistSuccessAsync`: merge under the config write lock, re-checking that the
    /// connection did not change meanwhile, then save and push the new settings to search.
    async fn persist_success(
        &self,
        url: &str,
        api_key: &str,
        remote: &[infinidysk_indexer::ProwlarrIndexer],
        attempted_at: i64,
        ct: &CancellationToken,
    ) -> SyncResponse {
        let mut store = self.config.write().await;
        let settings = store.settings().clone();
        if settings.prowlarr.url != url || settings.prowlarr.api_key.0 != api_key {
            drop(store);
            return self
                .fail(
                    "Prowlarr settings changed while synchronization was in progress; retrying is required.".to_owned(),
                    attempted_at,
                    ct,
                )
                .await;
        }
        let mut indexer_config = settings.indexers.clone();
        let mut profile_config = settings.profiles.clone().normalized();
        let merge = sync::merge(
            &mut indexer_config,
            &mut profile_config,
            remote,
            url,
            api_key,
        );
        if merge.profiles_changed && store.env_managed(PROFILES_INSTANCES) {
            drop(store);
            return self
                .fail(
                    "Prowlarr renamed or removed an indexer referenced by search profiles, but profiles are managed by INFINIDYSK__PROFILES__PROFILES.".to_owned(),
                    attempted_at,
                    ct,
                )
                .await;
        }
        let count = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
        let status = ProwlarrSyncStatus {
            last_attempt_at: attempted_at,
            last_success_at: Some(attempted_at),
            last_error: None,
            remote_indexer_count: count(merge.remote_indexer_count),
            added: count(merge.added),
            updated: count(merge.updated),
            removed: count(merge.removed),
            skipped: count(merge.skipped),
        };
        let mut new_settings = settings.clone();
        new_settings.indexers = indexer_config;
        new_settings.profiles = profile_config;
        new_settings.prowlarr.sync_status = Some(status.clone());
        if let Err(e) = store.save(new_settings).await {
            drop(store);
            return self.fail(format!("{e:#}"), attempted_at, ct).await;
        }
        let applied = store.settings().clone();
        drop(store);
        self.search
            .service
            .apply_settings(search_settings(&applied))
            .await;
        if !merge.skipped_details.is_empty() {
            warn!(
                count = merge.skipped_details.len(),
                details = %merge.skipped_details.join("; "),
                "Prowlarr indexer sync skipped indexer(s)"
            );
        }
        info!(
            added = merge.added,
            updated = merge.updated,
            removed = merge.removed,
            skipped = merge.skipped,
            "Prowlarr indexer sync completed"
        );
        let store = self.config.read().await;
        snapshot_of(&store, None)
    }

    /// v1 `FailAsync`: persist the failed attempt (attempt time + error, zeroed counters).
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the write guard is used by snapshot_of at the end"
    )]
    async fn fail(
        &self,
        error: String,
        attempted_at: i64,
        _ct: &CancellationToken,
    ) -> SyncResponse {
        let mut store = self.config.write().await;
        let mut settings = store.settings().clone();
        let mut status = settings.prowlarr.sync_status.clone().unwrap_or_default();
        status.last_attempt_at = attempted_at;
        status.last_error = Some(error.clone());
        status.added = 0;
        status.updated = 0;
        status.removed = 0;
        status.skipped = 0;
        settings.prowlarr.sync_status = Some(status);
        if let Err(e) = store.save(settings).await {
            warn!(reason = %format!("{e:#}"), "could not persist the Prowlarr sync failure status");
        }
        snapshot_of(&store, Some(error))
    }

    /// v1 `ExecuteAsync`: startup delay, then sync on config change or when the interval
    /// since the last attempt is due, waking at least every `TICK_INTERVAL`.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(STARTUP_DELAY) => {}
        }
        loop {
            let notified = {
                let store = self.config.read().await;
                store.changed()
            };
            let should = {
                let (connection, status, interval, enabled, url, key) = {
                    let store = self.config.read().await;
                    let s = store.settings();
                    let out = (
                        (
                            s.prowlarr.url.clone(),
                            s.prowlarr.api_key.0.clone(),
                            s.prowlarr.sync_enabled,
                            s.prowlarr.sync_interval_minutes,
                        ),
                        s.prowlarr.sync_status.clone().unwrap_or_default(),
                        s.prowlarr.sync_interval_minutes,
                        s.prowlarr.sync_enabled,
                        s.prowlarr.url.clone(),
                        s.prowlarr.api_key.0.clone(),
                    );
                    drop(store);
                    out
                };
                let mut last = self.last_connection.lock().expect("lock");
                let changed = *last != Some(connection.clone());
                if changed {
                    *last = Some(connection);
                }
                drop(last);
                let due = status.last_attempt_at <= 0
                    || Timestamp::now().as_second()
                        >= status.last_attempt_at + i64::from(interval) * 60;
                let enabled_with_connection = enabled && !url.trim().is_empty() && !key.is_empty();
                (changed && enabled) || (due && enabled_with_connection)
            };
            if should {
                self.sync_now(&cancel).await;
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(TICK_INTERVAL) => {}
                () = notified.notified() => {}
            }
        }
    }
}
