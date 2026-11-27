//! Shared HTTP transport for indexer-family clients (Newznab, Prowlarr), extracted from
//! the v1 `ProxyHttpClientPool`/`ArrHttpTransport` behavior: strict direct connections by
//! default (`no_proxy` also disables environment proxies), tunneled through the Phase-1
//! process-wide SOCKS5 proxy as `socks5h://` (remote DNS) when configured and the
//! destination host is not bypassed/local.

use std::future::Future;
use std::time::Duration;

use reqwest::header::USER_AGENT;
use reqwest::{Client, Response, Url};
use tokio_util::sync::CancellationToken;

use crate::Error;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Builds a one-shot HTTP client for requests to `host`, honoring the process-wide proxy.
pub fn build_client(skip_tls_verification: bool, host: &str) -> Result<Client, Error> {
    let mut builder = Client::builder().connect_timeout(CONNECT_TIMEOUT);
    if skip_tls_verification {
        // Intentional opt-in escape hatch for self-signed indexers, ported from v1's
        // per-indexer `IndexerConfig.SkipTlsVerification`; off by default.
        // codeql[rs/disabled-certificate-check]
        builder = builder.danger_accept_invalid_certs(true);
    }
    let proxy = infinidysk_proxy::global();
    if let Some(socks) = proxy.proxy_for(host) {
        let authority = if socks.host().contains(':') {
            format!("[{}]", socks.host())
        } else {
            socks.host().to_owned()
        };
        let url = format!("socks5h://{authority}:{}", socks.port());
        let proxy = reqwest::Proxy::all(url).map_err(|_| Error::InvalidBaseUrl)?;
        builder = builder.proxy(proxy);
    } else {
        builder = builder.no_proxy();
    }
    builder.build().map_err(|_| Error::HttpClient)
}

/// Like [`build_client`], but routes through the explicit HTTP(S) proxy URL when given
/// (v1 `ProxyHttpClientPool` per-indexer/global `proxyUrl`); otherwise the process-wide
/// proxy policy applies.
pub fn build_client_with_proxy(
    skip_tls_verification: bool,
    host: &str,
    proxy: Option<&str>,
) -> Result<Client, Error> {
    if let Some(raw) = proxy.filter(|p| !p.trim().is_empty()) {
        let url = Url::parse(raw).map_err(|_| Error::InvalidBaseUrl)?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(Error::InvalidBaseUrl);
        }
        let mut builder = Client::builder().connect_timeout(CONNECT_TIMEOUT);
        if skip_tls_verification {
            // codeql[rs/disabled-certificate-check]
            builder = builder.danger_accept_invalid_certs(true);
        }
        let proxy = reqwest::Proxy::all(url).map_err(|_| Error::InvalidBaseUrl)?;
        builder = builder.proxy(proxy);
        return builder.build().map_err(|_| Error::HttpClient);
    }
    build_client(skip_tls_verification, host)
}

/// GET with the given User-Agent. `max_attempts` = 2 retries exactly once on the first
/// transport-level failure (v1 `NewznabClient.GetAsync`); 1 means no retry (Prowlarr).
pub async fn get(
    http: &Client,
    url: &str,
    user_agent: &str,
    ct: &CancellationToken,
    max_attempts: u8,
) -> Result<Response, Error> {
    let mut attempt = 0u8;
    loop {
        let result = http.get(url).header(USER_AGENT, user_agent).send().await;
        match result {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                if ct.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                if attempt + 1 < max_attempts && e.status().is_none() {
                    attempt += 1;
                    continue;
                }
                return Err(match e.status() {
                    Some(s) => Error::Status(s),
                    None => Error::Transport,
                });
            }
        }
    }
}

/// Sanitized Content-Type media type for error messages. A malicious server could
/// reflect the API key into a header value, so anything outside a small token
/// allowlist is reported as `unknown`.
pub fn safe_media_type(headers: &reqwest::header::HeaderMap) -> String {
    let Some(raw) = headers.get(reqwest::header::CONTENT_TYPE) else {
        return "none".to_owned();
    };
    let Ok(raw) = raw.to_str() else {
        return "unknown".to_owned();
    };
    let media = raw.split(';').next().unwrap_or("").trim();
    if media.is_empty() {
        return "none".to_owned();
    }
    if media.len() <= 100
        && media
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'+' | b'/'))
    {
        media.to_owned()
    } else {
        "unknown".to_owned()
    }
}

/// Bounded body read: a declared Content-Length above `max` rejects before the stream is
/// read; otherwise bytes are counted and the read rejects as soon as `max` is exceeded
/// (without waiting for EOF).
pub async fn read_bounded(mut resp: Response, max: u64) -> Result<Vec<u8>, Error> {
    let declared = resp.content_length();
    if let Some(d) = declared
        && d > max
    {
        return Err(Error::TooLarge {
            max_bytes: max,
            declared_bytes: Some(d),
        });
    }
    let mut buf = Vec::new();
    loop {
        let chunk = match resp.chunk().await {
            Ok(chunk) => chunk,
            // A body framing/decode error (e.g. the server closing before the declared
            // Content-Length) reads as end-of-stream, like v1's bounded read: fewer bytes
            // than declared is not an error. Garbage still fails later at parse.
            Err(e) if e.is_body() || e.is_decode() => break,
            Err(_) => return Err(Error::Transport),
        };
        let Some(chunk) = chunk else { break };
        if buf.len() as u128 + chunk.len() as u128 > u128::from(max) {
            return Err(Error::TooLarge {
                max_bytes: max,
                declared_bytes: declared,
            });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// The v1 `WithTimeoutAsync`: caller cancellation wins over the timeout budget.
pub async fn with_timeout<T, F: Future<Output = Result<T, Error>>>(
    timeout: Duration,
    ct: &CancellationToken,
    work: F,
) -> Result<T, Error> {
    if ct.is_cancelled() {
        return Err(Error::Cancelled);
    }
    tokio::select! {
        biased;
        () = ct.cancelled() => Err(Error::Cancelled),
        () = tokio::time::sleep(timeout) => Err(Error::Timeout(timeout.as_secs())),
        r = work => r,
    }
}
