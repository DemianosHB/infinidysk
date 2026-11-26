//! Loopback Newznab server for the search-execution integration tests: replies are
//! computed per request from the target query string, and every request is recorded.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub target: String,
    pub user_agent: Option<String>,
}

#[derive(Clone, Default)]
pub struct Requests(pub Arc<Mutex<Vec<RecordedRequest>>>);

impl Requests {
    pub fn count(&self) -> usize {
        self.0.lock().expect("requests lock").len()
    }

    pub fn all(&self) -> Vec<RecordedRequest> {
        self.0.lock().expect("requests lock").clone()
    }

    pub fn targets(&self) -> Vec<String> {
        self.all().iter().map(|r| r.target.clone()).collect()
    }
}

pub type Handler = Arc<dyn Fn(&str) -> Vec<u8> + Send + Sync>;

/// Serves `handler(target) -> body` (always 200, content-length) on loopback.
pub async fn serve_newznab(handler: Handler) -> (SocketAddr, Requests) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let requests = Requests::default();
    let reqs = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let reqs = reqs.clone();
            let handler = handler.clone();
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
                    if buf.len() > 65536 {
                        return;
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let request_line = head.lines().next().unwrap_or("");
                let target = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_owned();
                let user_agent = head.lines().find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
                        .map(|(_, v)| v.trim().to_owned())
                });
                reqs.0.lock().expect("requests lock").push(RecordedRequest {
                    target: target.clone(),
                    user_agent,
                });
                let body = handler(&target);
                let out = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                if sock.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
                let _ = sock.write_all(&body).await;
            });
        }
    });
    (addr, requests)
}

/// Builds a Newznab RSS search response with the given items.
pub fn rss(items: &[(String, String, i64)]) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/"><channel>"#,
    );
    for (i, (title, nzb_url, size)) in items.iter().enumerate() {
        let _ = write!(
            xml,
            r#"<item><title>{title}</title><guid>guid-{i}</guid><link>{nzb_url}</link><pubDate>Fri, 09 Jan 2026 12:00:00 +0000</pubDate><enclosure url="{nzb_url}" length="{size}"/><newznab:attr name="grabs" value="{i}"/></item>"#
        );
    }
    xml.push_str("</channel></rss>");
    xml.into_bytes()
}

/// Converts string-literal items into owned tuples.
pub fn lit(items: &[(&str, &str, i64)]) -> Vec<(String, String, i64)> {
    items
        .iter()
        .map(|(t, u, s)| (t.to_string(), u.to_string(), *s))
        .collect()
}

/// Builds `count` distinct items with the given title prefix.
pub fn items(prefix: &str, count: usize) -> Vec<(String, String, i64)> {
    (0..count)
        .map(|i| {
            (
                format!("{prefix}.{i}.1080p.WEB-DL"),
                format!("http://nzb.example/{prefix}-{i}.nzb"),
                1_000_000 + i64::try_from(i).expect("small i"),
            )
        })
        .collect()
}

pub fn query_param(target: &str, key: &str) -> Option<String> {
    let q = target.split_once('?')?.1;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
