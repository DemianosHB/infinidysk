//! Warden dead-release list (v1 fork Phase 3), ported from `backend/Services/WardenStore.cs`,
//! `WardenStore.Backup.cs`, `WardenInputLimits.cs`, `WardenRemoteSourceService.cs` and
//! `backend/Api/Controllers/Warden`. Persisted in the main database (v1 used a separate
//! `warden.db` — documented deviation so PostgreSQL works too).

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use infinidysk_db::Db;
use infinidysk_db::warden::{
    LOCAL_SOURCE_ID, RemoteEntrySpec, TRUST_CORROBORATE, TRUST_FULL, WardenEntry, WardenSource,
};
use infinidysk_profiles::preferred;

use crate::auth::Auth;
use crate::config::ConfigStore;
use crate::search::api_key_allowed;

/// v1 `WardenInputLimits`.
pub const MAX_DECOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_RECORD_CHARS: usize = 64 * 1024;
pub const MAX_RECORDS: usize = 4_000_000;

/// v1 fingerprint shape: `wd1:` + 32 hex chars.
#[must_use]
pub fn is_valid_fp(fp: Option<&str>) -> bool {
    fp.is_some_and(|f| {
        f.len() == 36
            && f.starts_with("wd1:")
            && f[4..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

pub struct Warden {
    db: Db,
    config: Arc<RwLock<ConfigStore>>,
    auth: Arc<Auth>,
    /// In-memory mirror of effective dead fingerprints for the sync `IsDeadHook` (the store
    /// lookup is async; the hook is not). Refreshed on every local mark and by the run loop.
    dead_mirror: std::sync::Mutex<HashSet<String>>,
}

#[must_use]
pub fn build(db: Db, config: Arc<RwLock<ConfigStore>>, auth: Arc<Auth>) -> Arc<Warden> {
    Arc::new(Warden {
        db,
        config,
        auth,
        dead_mirror: std::sync::Mutex::new(HashSet::new()),
    })
}

pub fn router(state: Arc<Warden>) -> Router {
    Router::new()
        .route("/api/get-warden", get(get_warden))
        .route("/api/warden-sources", get(list_sources))
        .route("/api/warden-source-add", post(add_source))
        .route("/api/warden-source-update", post(update_source))
        .route("/api/warden-source-remove", post(remove_source))
        .route("/api/warden-export", get(export))
        .route("/api/warden-import", post(import))
        .route("/api/warden-sources-export", get(sources_export))
        .route("/api/warden-sources-import", post(sources_import))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            require_api_key,
        ))
        .with_state(state)
}

async fn require_api_key(
    State(state): State<Arc<Warden>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let allowed = api_key_allowed(&state.config, &state.auth, &req).await;
    if allowed {
        return next.run(req).await;
    }
    (StatusCode::UNAUTHORIZED, "API Key Required").into_response()
}

impl Warden {
    /// v1 `IsDeadAnywhere`: full-trust hit, or `quorum` corroborations, optionally scoped to
    /// the local providers' backbone root domains (v1 `warden.backbone-scope`).
    pub async fn is_dead_anywhere(&self, fp: Option<&str>) -> bool {
        let Some(fp) = fp else { return false };
        if fp.is_empty() {
            return false;
        }
        let (quorum, scope_enabled) = {
            let store = self.config.read().await;
            let w = store.settings().warden.clone();
            drop(store);
            (w.quorum(), w.backbone_scope)
        };
        let scope_roots: HashSet<String> = if scope_enabled {
            let hosts = self.current_backbones().await;
            let roots: HashSet<String> = hosts
                .iter()
                .map(|h| preferred::root_domain(Some(h)))
                .filter(|r| r != "unknown")
                .collect();
            if roots.is_empty() {
                HashSet::new()
            } else {
                roots
            }
        } else {
            HashSet::new()
        };
        let scope = !scope_roots.is_empty();
        let rows = match self.db.warden_dead_rows(fp).await {
            Ok(r) => r,
            Err(e) => {
                debug!(reason = %e, "warden lookup failed");
                return false;
            }
        };
        let mut agree = 0i64;
        for (trust, backbones, source_id) in rows {
            if scope && source_id != LOCAL_SOURCE_ID && !backbone_in_scope(&backbones, &scope_roots)
            {
                continue;
            }
            if trust == TRUST_FULL {
                return true;
            }
            agree += 1;
            if agree >= quorum {
                return true;
            }
        }
        false
    }

    /// v1 `MarkDead`: local upsert with merged backbones.
    pub async fn mark_dead(&self, fp: Option<&str>) {
        let Some(fp) = fp else { return };
        if !is_valid_fp(Some(fp)) {
            return;
        }
        let backbones = self.current_backbones().await.join(",");
        if let Err(e) = self.db.warden_mark_dead(fp, &backbones).await {
            debug!(reason = %e, "warden mark failed");
        } else {
            self.dead_mirror.lock().expect("lock").insert(fp.to_owned());
        }
    }

    /// Sync hook: is this fingerprint known dead (in-memory mirror)?
    pub fn is_dead_cached(&self, fp: Option<&str>) -> bool {
        fp.is_some_and(|f| self.dead_mirror.lock().expect("lock").contains(f))
    }

    /// Reloads the dead mirror from the store (called by the run loop).
    async fn reload_mirror(&self) {
        // Effective dead set = fingerprints where is_dead_anywhere is true; approximate by
        // scanning local + full-trust entries. The store is authoritative per query; the
        // mirror only serves the sync hook.
        let rows: Vec<String> = match self.db.warden_effective_fps().await {
            Ok(r) => r,
            Err(_) => return,
        };
        *self.dead_mirror.lock().expect("lock") = rows.into_iter().collect();
    }

    /// Comma-separated backbone hosts of the configured providers (v1 `CurrentBackbones`).
    async fn current_backbones(&self) -> Vec<String> {
        let store = self.config.read().await;
        let mut hosts: Vec<String> = store
            .settings()
            .providers
            .iter()
            .filter_map(|p| {
                let h = p.hostname.trim();
                if h.is_empty() {
                    None
                } else {
                    Some(preferred::backbone(Some(h)))
                }
            })
            .filter(|h| !h.is_empty() && h != "unknown")
            .collect();
        drop(store);
        hosts.sort();
        hosts.dedup();
        hosts
    }

    /// Refreshes all enabled remote sources whose refresh interval elapsed (v1
    /// `WardenRemoteSourceService`); runs in the background loop.
    pub async fn refresh_due_sources(&self, ct: &CancellationToken) {
        let Ok(sources) = self.db.warden_get_sources().await else {
            return;
        };
        let now = jiff::Timestamp::now().as_second();
        let (max_entries, max_bytes) = {
            let store = self.config.read().await;
            (
                store.settings().warden.max_source_entries(),
                store.settings().watchtower.list_source_max_response_bytes,
            )
        };
        for s in sources {
            if s.kind != "remote" || !s.enabled {
                continue;
            }
            let due =
                s.last_checked == 0 || now - s.last_checked >= i64::from(s.refresh_hours) * 3600;
            if !due {
                continue;
            }
            let Some(url) = s.url.clone() else { continue };
            let _ = max_bytes;
            match fetch_bundle(&url, ct).await {
                Ok((entries, etag)) => {
                    let specs: Vec<RemoteEntrySpec> = entries
                        .into_iter()
                        .filter(|(fp, _, _, _)| is_valid_fp(Some(fp)))
                        .collect();
                    if let Err(e) = self
                        .db
                        .warden_set_source_entries(&s.id, &specs, max_entries, now)
                        .await
                    {
                        warn!(reason = %e, source = %s.id, "warden source refresh failed");
                    }
                    let _ = self
                        .db
                        .warden_source_status(&s.id, "ok", etag.as_deref(), now)
                        .await;
                    let _ = self
                        .db
                        .warden_prune_source_entries(&s.id, max_entries)
                        .await;
                }
                Err(e) => {
                    debug!(reason = %e, source = %s.id, url = %url, "warden bundle fetch failed");
                    let _ = self
                        .db
                        .warden_source_status(&s.id, &format!("error: {e}"), None, now)
                        .await;
                }
            }
        }
    }

    /// Background loop: refresh remote sources every 30 minutes (v1 hosted service cadence).
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        self.reload_mirror().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(std::time::Duration::from_secs(300)) => {}
            }
            self.reload_mirror().await;
            self.refresh_due_sources(&cancel).await;
        }
    }
}

fn backbone_in_scope(backbones: &str, scope_roots: &HashSet<String>) -> bool {
    backbones
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty() && *b != "unknown")
        .any(|b| scope_roots.contains(&preferred::root_domain(Some(b))))
}

// --- HTTP handlers ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GetWardenResponse {
    status: bool,
    count: i64,
    local_count: i64,
    effective_count: i64,
    hide_dead: bool,
    quorum: i64,
    sources: Vec<SourceView>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceView {
    id: String,
    kind: String,
    name: String,
    url: Option<String>,
    enabled: bool,
    trust: String,
    refresh_hours: i32,
    last_checked: i64,
    last_updated: i64,
    status: Option<String>,
    count: i64,
}

impl From<WardenSource> for SourceView {
    fn from(s: WardenSource) -> Self {
        Self {
            id: s.id,
            kind: s.kind,
            name: s.name,
            url: s.url,
            enabled: s.enabled,
            trust: s.trust,
            refresh_hours: s.refresh_hours,
            last_checked: s.last_checked,
            last_updated: s.last_updated,
            status: s.status,
            count: s.count,
        }
    }
}

async fn get_warden(State(state): State<Arc<Warden>>) -> Response {
    let (hide_dead, quorum) = {
        let store = state.config.read().await;
        let w = store.settings().warden.clone();
        drop(store);
        (w.hide_dead, w.quorum())
    };
    let count = state.db.warden_count().await.unwrap_or(0);
    let local_count = state.db.warden_local_count().await.unwrap_or(0);
    let effective_count = state.db.warden_effective_count(quorum).await.unwrap_or(0);
    let sources = state
        .db
        .warden_get_sources()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(SourceView::from)
        .collect();
    Json(GetWardenResponse {
        status: true,
        count,
        local_count,
        effective_count,
        hide_dead,
        quorum,
        sources,
    })
    .into_response()
}

async fn list_sources(State(state): State<Arc<Warden>>) -> Response {
    let sources = state
        .db
        .warden_get_sources()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(SourceView::from)
        .collect::<Vec<_>>();
    Json(serde_json::json!({"status": true, "sources": sources})).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddSourceRequest {
    kind: Option<String>,
    name: Option<String>,
    url: Option<String>,
    trust: Option<String>,
    refresh_hours: Option<i32>,
}

fn normalize_trust(trust: Option<&str>) -> String {
    match trust {
        Some("corroborate") => TRUST_CORROBORATE.to_owned(),
        Some("observe") => "observe".to_owned(),
        _ => TRUST_FULL.to_owned(),
    }
}

fn clamp_refresh_hours(h: Option<i32>) -> i32 {
    h.unwrap_or(24).clamp(1, 24 * 365)
}

async fn add_source(
    State(state): State<Arc<Warden>>,
    Json(body): Json<AddSourceRequest>,
) -> Response {
    let kind = body.kind.unwrap_or_else(|| "remote".to_owned());
    if kind != "remote" && kind != "local" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"status": false, "error": "kind must be remote or local"})),
        )
            .into_response();
    }
    let id = format!(
        "src_{}",
        uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
    );
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("Untitled")
        .to_owned();
    let res = state
        .db
        .warden_add_source(
            &id,
            &kind,
            &name,
            body.url.as_deref().map(str::trim).filter(|u| !u.is_empty()),
            &normalize_trust(body.trust.as_deref()),
            clamp_refresh_hours(body.refresh_hours),
        )
        .await;
    match res {
        Ok(()) => Json(serde_json::json!({"status": true, "id": id})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateSourceRequest {
    id: String,
    name: Option<String>,
    url: Option<String>,
    enabled: Option<bool>,
    trust: Option<String>,
    refresh_hours: Option<i32>,
}

async fn update_source(
    State(state): State<Arc<Warden>>,
    Json(body): Json<UpdateSourceRequest>,
) -> Response {
    let trust = body.trust.as_deref().map(|t| normalize_trust(Some(t)));
    let ok = state
        .db
        .warden_update_source(
            &body.id,
            body.name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty()),
            body.url.as_deref().map(str::trim).filter(|u| !u.is_empty()),
            body.enabled,
            trust.as_deref(),
            body.refresh_hours.map(|h| h.clamp(1, 24 * 365)),
        )
        .await
        .unwrap_or(false);
    if ok {
        Json(serde_json::json!({"status": true})).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

#[derive(Deserialize)]
struct RemoveSourceRequest {
    id: String,
}

async fn remove_source(
    State(state): State<Arc<Warden>>,
    Json(body): Json<RemoveSourceRequest>,
) -> Response {
    let ok = state
        .db
        .warden_delete_source(&body.id)
        .await
        .unwrap_or(false);
    if ok {
        Json(serde_json::json!({"status": true})).into_response()
    } else {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"status": false, "error": "cannot remove the local source or unknown id"}))).into_response()
    }
}

/// v1 backup/export document shape (WardenStore.Backup).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WardenBackup {
    version: u32,
    exported_at: i64,
    sources: Vec<SourceView>,
    entries: Vec<EntryView>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryView {
    source_id: String,
    fp: String,
    dead_at: i64,
    n: i32,
    backbones: String,
}

async fn export(State(state): State<Arc<Warden>>) -> Response {
    let sources = state.db.warden_get_sources().await.unwrap_or_default();
    let mut entries = Vec::new();
    for s in &sources {
        let rows = state.db.warden_entries_of(&s.id).await.unwrap_or_default();
        for r in rows {
            entries.push(EntryView {
                source_id: r.source_id,
                fp: r.fp,
                dead_at: r.dead_at,
                n: r.n,
                backbones: r.backbones,
            });
        }
    }
    let doc = WardenBackup {
        version: 1,
        exported_at: jiff::Timestamp::now().as_second(),
        sources: sources.into_iter().map(SourceView::from).collect(),
        entries,
    };
    Json(doc).into_response()
}

async fn import(State(state): State<Arc<Warden>>, body: axum::body::Bytes) -> Response {
    let doc: Result<WardenBackup, _> = serde_json::from_slice(&body);
    let Ok(doc) = doc else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"status": false, "error": "invalid warden backup JSON"})),
        )
            .into_response();
    };
    if doc.entries.len() > MAX_RECORDS || body.len() > MAX_RECORD_CHARS * 64 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"status": false, "error": "backup too large"})),
        )
            .into_response();
    }
    let sources: Vec<WardenSource> = doc
        .sources
        .into_iter()
        .map(|s| WardenSource {
            id: s.id,
            kind: s.kind,
            name: s.name,
            url: s.url,
            enabled: s.enabled,
            trust: s.trust,
            refresh_hours: s.refresh_hours,
            last_checked: s.last_checked,
            last_updated: s.last_updated,
            status: s.status,
            count: 0,
        })
        .collect();
    let entries: Vec<WardenEntry> = doc
        .entries
        .into_iter()
        .filter(|e| is_valid_fp(Some(&e.fp)))
        .map(|e| WardenEntry {
            source_id: e.source_id,
            fp: e.fp,
            dead_at: e.dead_at,
            n: e.n,
            backbones: e.backbones,
        })
        .collect();
    match state.db.warden_replace_all(&sources, &entries).await {
        Ok(()) => {
            Json(serde_json::json!({"status": true, "entries": entries.len()})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourcesBundle {
    version: u32,
    sources: Vec<BundleSource>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BundleSource {
    name: String,
    url: String,
    trust: Option<String>,
    refresh_hours: Option<i32>,
}

async fn sources_export() -> Response {
    // Static well-known bundle list is a v1 UI convenience; the store itself is per-instance.
    Json(SourcesBundle {
        version: 1,
        sources: Vec::new(),
    })
    .into_response()
}

async fn sources_import(State(state): State<Arc<Warden>>, body: axum::body::Bytes) -> Response {
    let doc: Result<SourcesBundle, _> = serde_json::from_slice(&body);
    let Ok(doc) = doc else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"status": false, "error": "invalid sources bundle"})),
        )
            .into_response();
    };
    if doc.sources.len() > 1000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"status": false, "error": "too many sources"})),
        )
            .into_response();
    }
    let specs: Vec<(String, String, Option<String>, String, i32)> = doc
        .sources
        .into_iter()
        .map(|s| {
            (
                format!(
                    "src_{}",
                    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
                ),
                s.name,
                Some(s.url),
                normalize_trust(s.trust.as_deref()),
                clamp_refresh_hours(s.refresh_hours),
            )
        })
        .collect();
    match state.db.warden_import_remote_sources(&specs).await {
        Ok((added, skipped)) => {
            Json(serde_json::json!({"status": true, "added": added, "skipped": skipped}))
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct BundleEntry {
    fp: String,
    #[serde(default)]
    dead_at: Option<i64>,
    #[serde(default)]
    n: Option<i32>,
    #[serde(default)]
    backbones: Option<String>,
}

#[derive(Deserialize)]
struct BundleDoc {
    #[serde(default)]
    entries: Vec<BundleEntry>,
}

/// Fetches a remote warden bundle: JSON with `entries` of `fp`/`deadAt`/`n`/`backbones`
/// (or a bare array), with optional `ETag` handling.
async fn fetch_bundle(
    url: &str,
    ct: &CancellationToken,
) -> Result<(Vec<RemoteEntrySpec>, Option<String>), String> {
    use infinidysk_indexer::http;
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    let client = http::build_client(false, &host).map_err(|e| e.to_string())?;
    let resp = http::with_timeout(
        std::time::Duration::from_secs(30),
        ct,
        http::get(&client, url, "infinidysk-warden", ct, 1),
    )
    .await
    .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = http::read_bounded(resp, MAX_DECOMPRESSED_BYTES)
        .await
        .map_err(|e| e.to_string())?;
    let parsed: Result<BundleDoc, _> = serde_json::from_slice(&bytes);
    let entries = if let Ok(b) = parsed {
        b.entries
    } else {
        let arr: Result<Vec<BundleEntry>, _> = serde_json::from_slice(&bytes);
        arr.map_err(|e| e.to_string())?
    };
    if entries.len() > MAX_RECORDS {
        return Err("bundle too large".to_owned());
    }
    let now = jiff::Timestamp::now().as_second();
    Ok((
        entries
            .into_iter()
            .map(|e| {
                (
                    e.fp,
                    e.dead_at.unwrap_or(now),
                    e.n.unwrap_or(1),
                    e.backbones.unwrap_or_default(),
                )
            })
            .collect(),
        etag,
    ))
}

// Keep warden::merge_backbones referenced for tests.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fp_shape() {
        assert!(is_valid_fp(Some("wd1:0123456789abcdef0123456789abcdef")));
        assert!(!is_valid_fp(Some("wd1:XYZ")));
        assert!(!is_valid_fp(Some("rk1:0123456789abcdef0123456789abcdef")));
        assert!(!is_valid_fp(None));
    }

    #[test]
    fn trust_and_refresh_normalization() {
        assert_eq!(normalize_trust(Some("full")), "full");
        assert_eq!(normalize_trust(Some("corroborate")), "corroborate");
        assert_eq!(normalize_trust(Some("observe")), "observe");
        assert_eq!(normalize_trust(None), "full");
        assert_eq!(clamp_refresh_hours(None), 24);
        assert_eq!(clamp_refresh_hours(Some(0)), 1);
        assert_eq!(clamp_refresh_hours(Some(999_999)), 24 * 365);
    }

    #[test]
    fn backbone_scope_matching() {
        let roots: HashSet<String> = ["example.com".to_owned()].into_iter().collect();
        assert!(backbone_in_scope("nzb.example.com", &roots));
        assert!(!backbone_in_scope("other.org", &roots));
        assert!(!backbone_in_scope("unknown", &roots));
    }

    #[test]
    fn merge_backbones_works() {
        assert_eq!(
            infinidysk_db::warden::merge_backbones("b.com", "a.com,b.com"),
            "a.com,b.com"
        );
    }
}
