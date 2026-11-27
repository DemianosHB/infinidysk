//! Newznab/Prowlarr indexer HTTP client, ported from the C# v1 fork
//! (`backend/Clients/Indexers/NewznabClient.cs`).
//!
//! Behavior contract (from the v1 fork):
//! - API URL normalisation: the base URL path always ends in `/api` (case-insensitive),
//!   any existing query string is preserved and extended with the request parameters.
//! - `query` adds `apikey` and `extended=1` before the caller's parameters; `search`
//!   sends `t=search&q=…&limit=…`.
//! - One retry on the first transport-level failure; HTTP error statuses are not retried.
//! - Response bodies are bounded by `max_response_bytes`: a declared Content-Length above
//!   the limit rejects before the body is read; otherwise the stream is counted and
//!   rejects as soon as the limit is exceeded (before EOF).
//! - XML is parsed with DTD processing prohibited (a DOCTYPE is a format error and no
//!   external resolver is ever touched); malformed XML is a controlled format error.
//! - A root `<error>` element is a protocol error (`[code] description`).
//! - Error messages never contain the request URL, the API key, or body excerpts.
//! - HTTP goes direct by default; when the Phase-1 process-wide SOCKS5 proxy is configured
//!   and the indexer host is not bypassed, requests are tunneled through it as
//!   `socks5h://` (remote DNS at the proxy), mirroring the v1 `ProxyHttpClientPool`.

pub mod client;
pub mod filter;
pub mod http;
pub mod models;
pub mod prowlarr;
pub mod sync;
mod xml;

pub use client::NewznabClient;
pub use prowlarr::{ProwlarrClient, ProwlarrIndexer, ProwlarrSystemStatus};

/// Errors of indexer requests. Messages are controlled and never embed the request URL,
/// the API key, or response body excerpts (the URL query carries the API key). Safe
/// response metadata (HTTP status, sanitized content-type, byte count, parse offset)
/// may be included so failures stay diagnosable from logs and the UI.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("The indexer base URL is not a valid absolute URL.")]
    InvalidBaseUrl,
    #[error("max_response_bytes must be positive.")]
    InvalidMaxResponseBytes,
    #[error("Indexer request timed out after {0}s.")]
    Timeout(u64),
    #[error("Indexer request was cancelled.")]
    Cancelled,
    /// Transport-level failure after the single retry. Deliberately generic: the
    /// underlying error's debug output contains the request URL (with the API key).
    #[error("Indexer request failed.")]
    Transport,
    #[error("Failed to build the HTTP client.")]
    HttpClient,
    #[error("Indexer returned HTTP {0}.")]
    Status(reqwest::StatusCode),
    #[error("Remote response exceeded the configured {max_bytes}-byte limit.")]
    TooLarge {
        max_bytes: u64,
        declared_bytes: Option<u64>,
    },
    /// Malformed XML or a prohibited DOCTYPE. Carries only safe response metadata.
    #[error(
        "Indexer returned invalid XML (HTTP {status}, content-type {content_type}, {body_bytes} bytes, XML error at byte offset {offset})."
    )]
    Format {
        status: u16,
        content_type: String,
        body_bytes: usize,
        offset: u64,
    },
    /// Newznab `<error code="…" description="…"/>` root element: `"[code] description"`.
    #[error("{0}")]
    Indexer(String),
    // --- Prowlarr client ---
    #[error(
        "Prowlarr URL must be an absolute http(s) URL without credentials, query, or fragment."
    )]
    ProwlarrInvalidBaseUrl,
    #[error("Prowlarr rejected the API key.")]
    ProwlarrUnauthorized,
    #[error("Prowlarr returned HTTP {0}.")]
    ProwlarrStatus(reqwest::StatusCode),
    #[error("Prowlarr returned an indexer without a valid ID or name.")]
    ProwlarrInvalidIndexer,
    #[error("Prowlarr returned an empty response.")]
    ProwlarrEmpty,
    #[error("Prowlarr returned invalid JSON.")]
    ProwlarrInvalidJson,
    #[error("Prowlarr {operation} request to {base_url} timed out after {secs}s.")]
    ProwlarrTimeout {
        operation: &'static str,
        base_url: String,
        secs: u64,
    },
}

impl Error {
    /// Builds [`Error::Format`] from safe response metadata only: never the request
    /// URL, the API key, or body content.
    pub fn format(status: u16, content_type: &str, body_bytes: usize, offset: u64) -> Self {
        Self::Format {
            status,
            content_type: content_type.to_owned(),
            body_bytes,
            offset,
        }
    }
}
