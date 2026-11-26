//! The Newznab indexer client, ported from `backend/Clients/Indexers/NewznabClient.cs`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::time::Duration;

use jiff::Timestamp;
use reqwest::header::USER_AGENT;
use reqwest::{Client, Response, Url};
use tokio_util::sync::CancellationToken;

use infinidysk_search::result::SearchResult;

use crate::Error;
use crate::xml::{self, Element};

const DEFAULT_USER_AGENT: &str = "NzbDav";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Newznab API client for one indexer. The HTTP client is rebuilt per request so a
/// changed process-wide SOCKS5 proxy (published by config load/save) takes effect
/// immediately, mirroring the v1 `ProxyHttpClientPool` semantics.
pub struct NewznabClient {
    api_url: Url,
    api_key: String,
    user_agent: String,
    timeout: Duration,
    max_response_bytes: u64,
    skip_tls_verification: bool,
}

impl NewznabClient {
    pub fn new(base_url: &str, api_key: &str, max_response_bytes: u64) -> Result<Self, Error> {
        Self::with_options(
            base_url,
            api_key,
            max_response_bytes,
            DEFAULT_USER_AGENT,
            DEFAULT_TIMEOUT,
            false,
        )
    }

    pub fn with_options(
        base_url: &str,
        api_key: &str,
        max_response_bytes: u64,
        user_agent: &str,
        timeout: Duration,
        skip_tls_verification: bool,
    ) -> Result<Self, Error> {
        if max_response_bytes == 0 {
            return Err(Error::InvalidMaxResponseBytes);
        }
        Ok(Self {
            api_url: normalize_api_url(base_url)?,
            api_key: api_key.to_owned(),
            user_agent: user_agent.to_owned(),
            timeout: if timeout.is_zero() {
                DEFAULT_TIMEOUT
            } else {
                timeout
            },
            max_response_bytes,
            skip_tls_verification,
        })
    }

    /// `t=caps`: true when the indexer answers 2xx with a `<caps>` document.
    pub async fn test(&self, ct: &CancellationToken) -> Result<bool, Error> {
        let params = [("t", "caps"), ("apikey", self.api_key.as_str())];
        self.with_timeout(ct, async {
            let url = self.build_url(&params);
            let http = self.http_client()?;
            let resp = self.get(&http, &url, ct).await?;
            if !resp.status().is_success() {
                return Ok(false);
            }
            let body = read_bounded(resp, self.max_response_bytes).await?;
            let root = xml::parse(&body)?;
            Ok(root.has_descendant_named_ci("caps"))
        })
        .await
    }

    /// `t=search` with `q` and `limit`.
    pub async fn search(
        &self,
        query: &str,
        limit: u32,
        ct: &CancellationToken,
    ) -> Result<Vec<SearchResult>, Error> {
        let limit = limit.to_string();
        let params = [("t", "search"), ("q", query), ("limit", limit.as_str())];
        self.query(&params, ct).await
    }

    /// Runs an arbitrary Newznab query; `apikey` and `extended=1` are prepended.
    pub async fn query(
        &self,
        params: &[(&str, &str)],
        ct: &CancellationToken,
    ) -> Result<Vec<SearchResult>, Error> {
        let mut extra: Vec<(&str, &str)> =
            vec![("apikey", self.api_key.as_str()), ("extended", "1")];
        extra.extend_from_slice(params);
        self.with_timeout(ct, async {
            let url = self.build_url(&extra);
            let http = self.http_client()?;
            let resp = self.get(&http, &url, ct).await?;
            let resp = resp.error_for_status().map_err(|e| match e.status() {
                Some(s) => Error::Status(s),
                None => Error::Transport,
            })?;
            let body = read_bounded(resp, self.max_response_bytes).await?;
            let root = xml::parse(&body)?;
            if root.ns.is_none() && root.local == "error" {
                let code = root.attr("code");
                let desc = root
                    .attr("description")
                    .unwrap_or("Indexer returned an error.");
                return Err(Error::Indexer(match code {
                    Some(c) => format!("[{c}] {desc}"),
                    None => desc.to_owned(),
                }));
            }
            let Some(channel) = root.child("channel") else {
                return Ok(Vec::new());
            };
            Ok(channel.children_named("item").map(parse_item).collect())
        })
        .await
    }

    /// The v1 `WithTimeoutAsync`: caller cancellation wins over the timeout budget.
    async fn with_timeout<T, F: Future<Output = Result<T, Error>>>(
        &self,
        ct: &CancellationToken,
        work: F,
    ) -> Result<T, Error> {
        if ct.is_cancelled() {
            return Err(Error::Cancelled);
        }
        tokio::select! {
            biased;
            () = ct.cancelled() => Err(Error::Cancelled),
            () = tokio::time::sleep(self.timeout) => Err(Error::Timeout(self.timeout.as_secs())),
            r = work => r,
        }
    }

    /// One shared HTTP client per request, honoring the process-wide proxy for the
    /// indexer host: bypassed/local destinations stay direct (`no_proxy` also disables
    /// environment proxies, like the v1 strict direct handler).
    fn http_client(&self) -> Result<Client, Error> {
        let mut builder = Client::builder().connect_timeout(CONNECT_TIMEOUT);
        if self.skip_tls_verification {
            // Intentional opt-in escape hatch for self-signed indexers, ported from v1's
            // per-indexer `IndexerConfig.SkipTlsVerification`; off by default.
            // codeql[rs/disabled-certificate-check]
            builder = builder.danger_accept_invalid_certs(true);
        }
        let proxy = infinidysk_proxy::global();
        let use_proxy = self
            .api_url
            .host_str()
            .and_then(|host| proxy.proxy_for(host));
        if let Some(socks) = use_proxy {
            let host = socks.host();
            let authority = if host.contains(':') {
                format!("[{host}]")
            } else {
                host.to_owned()
            };
            let url = format!("socks5h://{authority}:{}", socks.port());
            let proxy = reqwest::Proxy::all(url).map_err(|_| Error::InvalidBaseUrl)?;
            builder = builder.proxy(proxy);
        } else {
            builder = builder.no_proxy();
        }
        builder.build().map_err(|_| Error::HttpClient)
    }

    /// GET with the configured User-Agent; retries exactly once on the first
    /// transport-level failure (v1: `HttpRequestException`/`IOException` on attempt 0).
    async fn get(
        &self,
        http: &Client,
        url: &str,
        ct: &CancellationToken,
    ) -> Result<Response, Error> {
        let mut attempt = 0u8;
        loop {
            let result = http
                .get(url)
                .header(USER_AGENT, &self.user_agent)
                .send()
                .await;
            match result {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if ct.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    if attempt == 0 && e.status().is_none() {
                        attempt = 1;
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

    fn build_url(&self, extra: &[(&str, &str)]) -> String {
        let existing = self.api_url.query().unwrap_or("");
        let mut parts: Vec<String> = Vec::new();
        if !existing.is_empty() {
            parts.push(existing.to_owned());
        }
        for (k, v) in extra {
            parts.push(format!(
                "{}={}",
                escape_data_string(k),
                escape_data_string(v)
            ));
        }
        let mut url = self.api_url.clone();
        url.set_query(None);
        url.set_fragment(None);
        format!("{}?{}", url, parts.join("&"))
    }
}

/// The v1 `NormalizeApiUri`: the path always ends in `/api` (case-insensitive); an empty
/// path becomes `/api`. The query string is preserved.
fn normalize_api_url(base_url: &str) -> Result<Url, Error> {
    let mut url = Url::parse(base_url).map_err(|_| Error::InvalidBaseUrl)?;
    let trimmed = url.path().trim_end_matches('/');
    let path = if trimmed.is_empty() {
        "/api".to_owned()
    } else if trimmed.len() >= 4 && trimmed[trimmed.len() - 4..].eq_ignore_ascii_case("/api") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/api")
    };
    url.set_path(&path);
    Ok(url)
}

/// RFC 3986 percent-encoding, matching .NET `Uri.EscapeDataString` (unreserved set kept).
fn escape_data_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Bounded body read: a declared Content-Length above `max` rejects before the stream is
/// read; otherwise bytes are counted and the read rejects as soon as `max` is exceeded
/// (without waiting for EOF).
async fn read_bounded(mut resp: Response, max: u64) -> Result<Vec<u8>, Error> {
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
            // than declared is not an error. Garbage still fails later at XML parse.
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

fn parse_item(item: &Element) -> SearchResult {
    let mut attrs: HashMap<String, String> = HashMap::new();
    for (name, value) in item.newznab_attrs() {
        let key = name.to_ascii_lowercase();
        if !value.is_empty() && !attrs.contains_key(&key) {
            attrs.insert(key, value.to_owned());
        }
    }
    let get_attr = |attrs: &HashMap<String, String>, name: &str| -> Option<String> {
        attrs
            .get(&name.to_ascii_lowercase())
            .filter(|v| !v.is_empty())
            .cloned()
    };

    let enclosure = item.child("enclosure");
    let size_attr = get_attr(&attrs, "size");
    let size_str = enclosure
        .and_then(|e| e.attr("length"))
        .or(size_attr.as_deref());
    let size = size_str.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);

    let nzb_url = enclosure
        .and_then(|e| e.attr("url"))
        .or_else(|| item.child("link").map(|l| l.text.as_str()))
        .unwrap_or_default()
        .to_owned();

    let posted = item.child("pubDate").and_then(|p| parse_timestamp(&p.text));
    let usenet_date = get_attr(&attrs, "usenetdate").and_then(|v| parse_timestamp(&v));

    let source_indexer_name = [
        "sourceIndexerName",
        "hydraIndexerName",
        "indexer",
        "provider",
    ]
    .iter()
    .find_map(|name| get_attr(&attrs, name))
    .or_else(|| {
        ["jackettindexer", "source", "indexer", "provider"]
            .iter()
            .find_map(|local| item.child_text_ci(local))
    });

    SearchResult {
        title: item
            .child("title")
            .map(|t| t.text.clone())
            .unwrap_or_default(),
        guid: item
            .child("guid")
            .map(|g| g.text.clone())
            .unwrap_or_default(),
        nzb_url,
        size,
        posted,
        usenet_date,
        grabs: parse_non_neg_int(get_attr(&attrs, "grabs").as_deref()),
        comments: parse_non_neg_int(get_attr(&attrs, "comments").as_deref()),
        password: parse_non_neg_int(get_attr(&attrs, "password").as_deref()),
        files: parse_non_neg_int(get_attr(&attrs, "files").as_deref()),
        group: get_attr(&attrs, "group"),
        poster: get_attr(&attrs, "poster"),
        source_indexer_name,
        language: get_attr(&attrs, "language"),
        subs: get_attr(&attrs, "subs"),
        info_hash: get_attr(&attrs, "infohash"),
    }
}

/// v1 `ParseNonNegInt`: missing/unparseable → `None`, negative → `Some(0)`.
fn parse_non_neg_int(raw: Option<&str>) -> Option<i32> {
    let raw = raw?;
    if raw.is_empty() {
        return None;
    }
    let n = raw.parse::<i32>().ok()?;
    Some(n.max(0))
}

/// Parses a `pubDate`/`usenetdate` value. v1 uses the lenient `DateTimeOffset.TryParse`;
/// we accept RFC 2822 (the NNTP/RSS standard) and leave anything else as `None`. The
/// leading weekday is optional in RFC 2822 and some indexers send a wrong one, so it is
/// stripped before parsing (jiff validates it, .NET did not).
fn parse_timestamp(raw: &str) -> Option<Timestamp> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let without_weekday = match raw.split_once(',') {
        Some((day, rest)) if day.len() == 3 && day.bytes().all(|b| b.is_ascii_alphabetic()) => {
            rest.trim_start()
        }
        _ => raw,
    };
    jiff::fmt::rfc2822::parse(without_weekday)
        .ok()
        .map(|z| z.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(base: &str) -> NewznabClient {
        NewznabClient::new(base, "KEY", 1024).unwrap()
    }

    #[test]
    fn normalize_api_url_appends_api_unless_present() {
        // Ported from the v1 NormalizeApiUri behavior.
        assert_eq!(normalize_api_url("http://h").unwrap().path(), "/api");
        assert_eq!(normalize_api_url("http://h/").unwrap().path(), "/api");
        assert_eq!(normalize_api_url("http://h/x").unwrap().path(), "/x/api");
        assert_eq!(normalize_api_url("http://h/x/").unwrap().path(), "/x/api");
        assert_eq!(normalize_api_url("http://h/api").unwrap().path(), "/api");
        assert_eq!(normalize_api_url("http://h/api/").unwrap().path(), "/api");
        assert_eq!(normalize_api_url("http://h/API").unwrap().path(), "/API");
        assert_eq!(normalize_api_url("http://h/Api").unwrap().path(), "/Api");
        // existing query is preserved on the normalized URL
        assert_eq!(
            normalize_api_url("http://h/x?foo=bar").unwrap().query(),
            Some("foo=bar")
        );
        assert!(normalize_api_url("not a url").is_err());
        assert!(normalize_api_url("/relative").is_err());
    }

    #[test]
    fn build_url_keeps_existing_query_and_escapes_params() {
        let c = client("http://h/base?foo=bar");
        let url = c.build_url(&[("apikey", "K EY"), ("q", "a&b=c")]);
        assert_eq!(url, "http://h/base/api?foo=bar&apikey=K%20EY&q=a%26b%3Dc");
    }

    #[test]
    fn escape_data_string_matches_rfc3986() {
        assert_eq!(escape_data_string("abcXYZ019-._~"), "abcXYZ019-._~");
        assert_eq!(escape_data_string("a b&c=d?e/f"), "a%20b%26c%3Dd%3Fe%2Ff");
        assert_eq!(escape_data_string("ä"), "%C3%A4");
    }

    #[test]
    fn max_response_bytes_must_be_positive() {
        assert!(matches!(
            NewznabClient::new("http://h/api", "K", 0),
            Err(Error::InvalidMaxResponseBytes)
        ));
    }

    #[test]
    fn parse_timestamp_accepts_rfc2822_and_ignores_wrong_weekday() {
        let ts = parse_timestamp("Fri, 02 Jan 2026 15:04:05 +0000").unwrap();
        assert_eq!(ts.to_string(), "2026-01-02T15:04:05Z");
        // .NET's DateTimeOffset.TryParse ignores a mismatched weekday; so do we.
        let ts = parse_timestamp("Mon, 02 Jan 2026 15:04:05 +0000").unwrap();
        assert_eq!(ts.to_string(), "2026-01-02T15:04:05Z");
        assert_eq!(
            parse_timestamp("Tue, 06 Jan 2026 10:00:00 +0100")
                .unwrap()
                .to_string(),
            "2026-01-06T09:00:00Z"
        );
        assert!(parse_timestamp("not a date").is_none());
        assert!(parse_timestamp("").is_none());
    }

    #[test]
    fn zero_timeout_falls_back_to_default() {
        let c = NewznabClient::with_options("http://h/api", "K", 10, "UA", Duration::ZERO, false)
            .unwrap();
        assert_eq!(c.timeout, DEFAULT_TIMEOUT);
    }
}
