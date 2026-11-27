//! Loopback HTTP server helpers shared by the indexer integration tests.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub target: String,
    pub user_agent: Option<String>,
}

#[derive(Clone)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "scripted-reply flags for the loopback test server"
)]
pub struct Reply {
    pub status: &'static str,
    pub content_length: Option<usize>,
    pub content_type: Option<String>,
    pub chunked: bool,
    pub body: Vec<u8>,
    pub send_body: bool,
    pub block: bool,
    pub drop: bool,
    /// Never answer at all (v1 HangHandler): the client must hit its own timeout.
    pub hang: bool,
}

impl Reply {
    pub fn ok(body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_length: Some(body.len()),
            content_type: None,
            chunked: false,
            body,
            send_body: true,
            block: false,
            drop: false,
            hang: false,
        }
    }
}

#[derive(Clone, Default)]
pub struct Requests(pub Arc<Mutex<Vec<RecordedRequest>>>);

impl Requests {
    pub fn count(&self) -> usize {
        self.0.lock().expect("requests lock").len()
    }

    pub fn first(&self) -> RecordedRequest {
        self.0.lock().expect("requests lock")[0].clone()
    }
}

/// Serves scripted replies on loopback, one per accepted connection (last reply repeats).
pub async fn serve(replies: Vec<Reply>) -> (SocketAddr, Requests) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let requests = Requests::default();
    let reqs = requests.clone();
    tokio::spawn(async move {
        let mut idx = 0usize;
        while let Ok((mut sock, _)) = listener.accept().await {
            let reply = replies[idx.min(replies.len() - 1)].clone();
            idx += 1;
            let reqs = reqs.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                let head_end = loop {
                    let Ok(n) = sock.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = find(&buf, b"\r\n\r\n") {
                        break p + 4;
                    }
                    if buf.len() > 65536 {
                        return;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
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
                reqs.0
                    .lock()
                    .expect("requests lock")
                    .push(RecordedRequest { target, user_agent });
                if reply.drop {
                    return;
                }
                if reply.hang {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    return;
                }
                let mut out = format!("HTTP/1.1 {}\r\n", reply.status);
                if let Some(cl) = reply.content_length {
                    let _ = write!(out, "content-length: {cl}\r\n");
                }
                if let Some(ct) = &reply.content_type {
                    let _ = write!(out, "content-type: {ct}\r\n");
                }
                if reply.chunked {
                    out.push_str("transfer-encoding: chunked\r\n");
                }
                out.push_str("connection: close\r\n\r\n");
                if sock.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
                if reply.send_body {
                    if reply.chunked {
                        let _ = sock
                            .write_all(format!("{:x}\r\n", reply.body.len()).as_bytes())
                            .await;
                        let _ = sock.write_all(&reply.body).await;
                        let _ = sock.write_all(b"\r\n0\r\n\r\n").await;
                    } else {
                        let _ = sock.write_all(&reply.body).await;
                    }
                }
                if reply.block {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });
        }
    });
    (addr, requests)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
