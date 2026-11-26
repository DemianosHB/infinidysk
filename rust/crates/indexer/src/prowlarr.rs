//! Prowlarr API client, ported from `backend/Clients/Prowlarr/ProwlarrClient.cs` (v1 fork).
//! JSON over the shared transport ([`crate::http`]); no retry (v1 `SendAsync` doesn't),
//! 15s request timeout, `X-Api-Key` header. Error messages never contain the API key.

use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::Error;
use crate::http;

/// Ported from `ProwlarrClient.RequestTimeout`.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Ported from `ProwlarrSystemStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProwlarrSystemStatus {
    pub version: Option<String>,
}

/// Ported from `ProwlarrIndexer` (Prowlarr API uses camelCase).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProwlarrIndexer {
    // Value-type fields default when absent, matching System.Text.Json; the shape check in
    // get_indexers then rejects id <= 0.
    #[serde(default)]
    pub id: i32,
    pub name: Option<String>,
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub supports_search: bool,
    pub protocol: Option<String>,
}

/// Ported from `ProwlarrClient`.
pub struct ProwlarrClient {
    base_url: String,
    api_key: String,
    timeout: Duration,
}

impl ProwlarrClient {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, Error> {
        Ok(Self {
            base_url: Self::normalize_base_url(base_url)?,
            api_key: api_key.to_owned(),
            timeout: REQUEST_TIMEOUT,
        })
    }

    /// Overrides the request timeout (default [`REQUEST_TIMEOUT`]); mainly for tests.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Ported from `ProwlarrClient.NormalizeBaseUrl`: absolute http(s) URL without
    /// credentials, query, or fragment; trailing slashes trimmed.
    pub fn normalize_base_url(raw: &str) -> Result<String, Error> {
        let mut url = Url::parse(raw).map_err(|_| Error::ProwlarrInvalidBaseUrl)?;
        let is_http = url.scheme() == "http" || url.scheme() == "https";
        if !is_http
            || url.query().is_some_and(|q| !q.is_empty())
            || url.fragment().is_some()
            || url.password().is_some()
            || !url.username().is_empty()
        {
            return Err(Error::ProwlarrInvalidBaseUrl);
        }
        // Rebuild without query/fragment/userinfo and trim trailing slashes.
        url.set_query(None);
        url.set_fragment(None);
        url.set_username("")
            .map_err(|()| Error::ProwlarrInvalidBaseUrl)?;
        url.set_password(None)
            .map_err(|()| Error::ProwlarrInvalidBaseUrl)?;
        let mut s = url.to_string();
        while s.ends_with('/') {
            s.pop();
        }
        Ok(s)
    }

    /// Ported from `ProwlarrClient.BuildIndexerApiUrl`: `{base}/{indexerId}/api`.
    pub fn build_indexer_api_url(base_url: &str, indexer_id: i32) -> String {
        let base = Self::normalize_base_url(base_url)
            .unwrap_or_else(|_| base_url.trim_end_matches('/').to_owned());
        format!("{base}/{indexer_id}/api")
    }

    pub async fn get_status(&self, ct: &CancellationToken) -> Result<ProwlarrSystemStatus, Error> {
        self.get_json("system/status", "system status", ct).await
    }

    pub async fn get_indexers(
        &self,
        ct: &CancellationToken,
    ) -> Result<Vec<ProwlarrIndexer>, Error> {
        let indexers: Vec<ProwlarrIndexer> = self.get_json("indexer", "indexer list", ct).await?;
        if indexers
            .iter()
            .any(|x| x.id <= 0 || x.name.as_deref().is_none_or(|n| n.trim().is_empty()))
        {
            return Err(Error::ProwlarrInvalidIndexer);
        }
        Ok(indexers)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        resource: &str,
        operation: &'static str,
        ct: &CancellationToken,
    ) -> Result<T, Error> {
        let url = format!("{}/api/v1/{resource}", self.base_url);
        let base = self.base_url.clone();
        let secs = self.timeout.as_secs();
        http::with_timeout(self.timeout, ct, async {
            let host = Url::parse(&base)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_default();
            let client = http::build_client(false, &host)?;
            let mut req = client.get(&url);
            req = req.header(reqwest::header::ACCEPT, "application/json");
            req = req.header(
                reqwest::header::USER_AGENT,
                format!("NzbDAV/{}", env!("CARGO_PKG_VERSION")),
            );
            req = req.header("X-Api-Key", &self.api_key);
            let resp = req.send().await.map_err(|e| {
                if ct.is_cancelled() {
                    Error::Cancelled
                } else {
                    match e.status() {
                        Some(s) => Error::Status(s),
                        None => Error::Transport,
                    }
                }
            })?;
            let status = resp.status();
            if !status.is_success() {
                return Err(if status.as_u16() == 401 || status.as_u16() == 403 {
                    Error::ProwlarrUnauthorized
                } else {
                    Error::ProwlarrStatus(status)
                });
            }
            let body = http::read_bounded(resp, crate::models::HARD_MAX_RESPONSE_BYTES).await?;
            let value: Option<T> =
                serde_json::from_slice(&body).map_err(|_| Error::ProwlarrInvalidJson)?;
            value.ok_or(Error::ProwlarrEmpty)
        })
        .await
        .map_err(|e| match e {
            Error::Timeout(_) => Error::ProwlarrTimeout {
                operation,
                base_url: base,
                secs,
            },
            other => other,
        })
    }
}
