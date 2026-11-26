//! Characterization tests for the Prowlarr client, ported from
//! `tests/NzbWebDAV.Tests/Clients/ProwlarrClientTests.cs`, against a loopback HTTP server.

mod common;

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use common::{Reply, serve};
use infinidysk_indexer::{Error, ProwlarrClient};

const KEY: &str = "super-secret-key";

#[test]
fn build_indexer_api_url_preserves_prowlarr_url_base() {
    // Ported from BuildIndexerApiUrl_PreservesProwlarrUrlBase.
    assert_eq!(
        ProwlarrClient::build_indexer_api_url("http://prowlarr:9696/prowlarr/", 42),
        "http://prowlarr:9696/prowlarr/42/api"
    );
}

#[test]
fn normalize_base_url_rejects_unsafe_or_unsupported_urls() {
    // Ported from NormalizeBaseUrl_RejectsUnsafeOrUnsupportedUrls.
    for url in [
        "not-a-url",
        "ftp://prowlarr:9696",
        "https://user:secret@prowlarr:9696",
        "http://prowlarr:9696/?x=1",
        "http://prowlarr:9696#frag",
        "http://user@prowlarr:9696",
    ] {
        assert!(
            matches!(
                ProwlarrClient::normalize_base_url(url),
                Err(Error::ProwlarrInvalidBaseUrl)
            ),
            "{url}"
        );
    }
    assert_eq!(
        ProwlarrClient::normalize_base_url("http://prowlarr:9696/prowlarr/").unwrap(),
        "http://prowlarr:9696/prowlarr"
    );
}

#[tokio::test]
async fn get_indexers_uses_api_key_and_parses_fields() {
    // Ported from GetIndexers_UsesApiKeyAndParsesSyncFields.
    let body = br#"[
      {"id":7,"name":"Usenet","enable":true,"supportsSearch":true,"protocol":"usenet"},
      {"id":8,"name":"Torrent","enable":true,"supportsSearch":true,"protocol":"torrent"}
    ]"#
    .to_vec();
    let (addr, requests) = serve(vec![Reply::ok(body)]).await;
    let client =
        ProwlarrClient::new(&format!("http://{addr}/prowlarr/"), "prowlarr-secret").unwrap();

    let indexers = client
        .get_indexers(&CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(indexers.iter().map(|x| x.id).collect::<Vec<_>>(), [7, 8]);
    assert_eq!(indexers[0].name.as_deref(), Some("Usenet"));
    assert!(indexers[0].supports_search);
    assert_eq!(indexers[1].protocol.as_deref(), Some("torrent"));

    let req = requests.first();
    let expected_ua = format!("NzbDAV/{}", env!("CARGO_PKG_VERSION"));
    assert_eq!(req.target, "/prowlarr/api/v1/indexer");
    assert_eq!(req.user_agent.as_deref(), Some(expected_ua.as_str()));
}

#[tokio::test]
async fn unauthorized_does_not_leak_api_key() {
    // Ported from GetIndexers_UnauthorizedDoesNotLeakApiKey.
    let reply = Reply {
        status: "401 Unauthorized",
        ..Reply::ok(Vec::new())
    };
    let (addr, _requests) = serve(vec![reply]).await;
    let client = ProwlarrClient::new(&format!("http://{addr}"), KEY).unwrap();

    let err = client
        .get_indexers(&CancellationToken::new())
        .await
        .unwrap_err();

    assert!(matches!(err, Error::ProwlarrUnauthorized), "{err:?}");
    assert_eq!(err.to_string(), "Prowlarr rejected the API key.");
    assert!(!format!("{err:?}").contains(KEY));
}

#[tokio::test]
async fn other_status_maps_to_http_code() {
    let reply = Reply {
        status: "500 Internal Server Error",
        ..Reply::ok(Vec::new())
    };
    let (addr, _requests) = serve(vec![reply]).await;
    let client = ProwlarrClient::new(&format!("http://{addr}"), KEY).unwrap();

    let err = client
        .get_indexers(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::ProwlarrStatus(s) if s.as_u16() == 500),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Prowlarr returned HTTP 500 Internal Server Error."
    );
}

#[tokio::test]
async fn rejects_invalid_remote_indexer_shape() {
    // Ported from GetIndexers_RejectsInvalidRemoteIndexerShape.
    let (addr, _requests) = serve(vec![Reply::ok(br#"[{"id":0,"name":""}]"#.to_vec())]).await;
    let client = ProwlarrClient::new(&format!("http://{addr}"), "key").unwrap();

    let err = client
        .get_indexers(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ProwlarrInvalidIndexer), "{err:?}");
}

#[tokio::test]
async fn null_json_is_empty_response() {
    // v1: JsonSerializer.DeserializeAsync returns null for a JSON null -> "empty response".
    let (addr, _requests) = serve(vec![Reply::ok(b"null".to_vec())]).await;
    let client = ProwlarrClient::new(&format!("http://{addr}"), "key").unwrap();
    let err = client
        .get_status(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ProwlarrEmpty), "{err:?}");
}

#[tokio::test]
async fn empty_body_is_invalid_json() {
    // v1: an empty stream makes System.Text.Json throw JsonException -> "invalid JSON".
    let (addr, _requests) = serve(vec![Reply::ok(Vec::new())]).await;
    let client = ProwlarrClient::new(&format!("http://{addr}"), "key").unwrap();
    let err = client
        .get_status(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ProwlarrInvalidJson), "{err:?}");
}

#[tokio::test]
async fn invalid_json_is_controlled_error() {
    let (addr, _requests) = serve(vec![Reply::ok(b"{not json".to_vec())]).await;
    let client = ProwlarrClient::new(&format!("http://{addr}"), "key").unwrap();
    let err = client
        .get_status(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ProwlarrInvalidJson), "{err:?}");
    assert_eq!(err.to_string(), "Prowlarr returned invalid JSON.");
    assert!(!err.to_string().contains(KEY));
}

#[tokio::test]
async fn http_client_timeout_surfaces_as_timeout_not_leaking_key() {
    // Ported from HttpClientTimeout_SurfacesAsTimedOutRequest (without the v1
    // ArrHttpTransport routing suffix; the base URL carries no credentials).
    let (addr, _requests) = serve(vec![Reply {
        block: true,
        send_body: false,
        ..Reply::ok(Vec::new())
    }])
    .await;
    let client = ProwlarrClient::new(&format!("http://{addr}"), KEY)
        .unwrap()
        .with_request_timeout(Duration::from_millis(100));

    let err = client
        .get_indexers(&CancellationToken::new())
        .await
        .unwrap_err();

    let Error::ProwlarrTimeout {
        operation,
        base_url,
        secs,
    } = &err
    else {
        panic!("expected ProwlarrTimeout, got {err:?}");
    };
    assert_eq!(*operation, "indexer list");
    assert!(base_url.contains(&addr.port().to_string()));
    assert_eq!(*secs, 0, "100ms rounds down to 0s like v1's 0.1s display");
    assert!(!format!("{err:?}").contains(KEY));
}

#[tokio::test]
async fn caller_cancellation_is_not_reported_as_timeout() {
    // Ported from CallerCancellation_IsNotReportedAsTimeout.
    let (addr, _requests) = serve(vec![Reply {
        block: true,
        send_body: false,
        ..Reply::ok(Vec::new())
    }])
    .await;
    let client = ProwlarrClient::new(&format!("http://{addr}"), "key").unwrap();
    let ct = CancellationToken::new();
    ct.cancel();

    let err = client.get_status(&ct).await.unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
}

#[tokio::test]
async fn x_api_key_header_is_sent() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // raw capture: read the request head and assert the header directly
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        loop {
            let n = sock.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf).into_owned();
        let head_lower = head.to_lowercase();
        assert!(head_lower.contains("x-api-key: prowlarr-secret"), "{head}");
        assert!(head_lower.contains("accept: application/json"), "{head}");
        sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n[]")
            .await
            .unwrap();
    });
    let client = ProwlarrClient::new(&format!("http://{addr}"), "prowlarr-secret").unwrap();
    let indexers = client
        .get_indexers(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(indexers.len(), 0);
}
