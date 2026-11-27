//! Characterization tests for the Newznab client, ported from
//! `tests/NzbWebDAV.Tests/Clients/NewznabClientTests.cs` (v1 fork), against a loopback
//! HTTP server. The `declared-below-limit/actual-over` case is covered differently: hyper
//! enforces the declared Content-Length at the framing layer, so a server sending more
//! bytes than declared never delivers them to the body (see
//! `declared_length_is_enforced_by_the_http_framing`); stream-count rejection is covered
//! by the chunked/undeclared cases.

mod common;

use common::{Reply, serve};

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use infinidysk_indexer::{Error, NewznabClient};

const API_KEY: &str = "DO_NOT_LOG_INDEXER_KEY";
const BODY_MARKER: &str = "DO_NOT_LOG_BODY_MARKER";

fn search_xml(title: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/"><channel><item><title>{title}</title><guid>g1</guid><link>http://nzb.example/a.nzb</link><enclosure url="http://nzb.example/a.nzb" length="10"/></item></channel></rss>"#
    )
    .into_bytes()
}

fn caps_xml() -> Vec<u8> {
    br#"<?xml version="1.0"?><caps><server version="1.0" title="t"/></caps>"#.to_vec()
}

fn pad_utf8(body: &[u8], target: usize) -> Vec<u8> {
    let mut out = body.to_vec();
    out.resize(target, b' ');
    out
}

fn client_for(addr: SocketAddr, max: u64) -> NewznabClient {
    NewznabClient::new(&format!("http://{addr}/api"), API_KEY, max).expect("client")
}

#[tokio::test]
async fn query_valid_body_at_or_below_limit_is_accepted() {
    // Ported from QueryAsync_ValidBody_AtOrBelowLimit_IsAccepted (headroom 1 and 0).
    for headroom in [1usize, 0] {
        let body = search_xml("Example Release");
        let limit = body.len() + headroom;
        let (addr, requests) = serve(vec![Reply::ok(body)]).await;
        let client = client_for(addr, limit as u64);

        let items = client
            .search("query", 100, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Example Release");
        assert_eq!(requests.count(), 1);
        let req = requests.first();
        assert!(req.target.starts_with("/api?"), "{}", req.target);
        for param in [
            "apikey=DO_NOT_LOG_INDEXER_KEY",
            "extended=1",
            "t=search",
            "q=query",
            "limit=100",
        ] {
            assert!(req.target.contains(param), "{} missing {param}", req.target);
        }
        assert_eq!(req.user_agent.as_deref(), Some("NzbDav"));
    }
}

#[tokio::test]
async fn one_byte_over_limit_rejects_without_partial_result() {
    // Ported from QueryAsync_OneByteOverLimit_RejectsWithoutPartialResult.
    let body = search_xml(BODY_MARKER);
    let limit = body.len() - 1;
    let (addr, _requests) = serve(vec![Reply::ok(body)]).await;
    let client = client_for(addr, limit as u64);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    let Error::TooLarge { max_bytes, .. } = &err else {
        panic!("expected TooLarge, got {err:?}");
    };
    assert_eq!(*max_bytes, limit as u64);
    assert!(!err.to_string().contains(API_KEY));
    assert!(!format!("{err:?}").contains(API_KEY));
    assert!(!err.to_string().contains(BODY_MARKER));
    assert!(!format!("{err:?}").contains(BODY_MARKER));
}

#[tokio::test]
async fn test_valid_caps_at_limit_succeeds() {
    // Ported from TestAsync_ValidCaps_AtLimit_Succeeds.
    let body = caps_xml();
    let (addr, _requests) = serve(vec![Reply::ok(body.clone())]).await;
    let client = client_for(addr, body.len() as u64);

    assert!(client.test(&CancellationToken::new()).await.unwrap());
}

#[tokio::test]
async fn test_one_byte_over_limit_rejects() {
    // Ported from TestAsync_OneByteOverLimit_Rejects.
    let body = caps_xml();
    let (addr, _requests) = serve(vec![Reply::ok(body.clone())]).await;
    let client = client_for(addr, (body.len() - 1) as u64);

    let err = client.test(&CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, Error::TooLarge { .. }), "{err:?}");
}

#[tokio::test]
async fn declared_length_above_limit_rejects_before_stream_read() {
    // Ported from QueryAsync_DeclaredLengthAboveLimit_RejectsBeforeStreamRead.
    let limit = 64usize;
    let reply = Reply {
        content_length: Some(limit + 1),
        send_body: false,
        ..Reply::ok(Vec::new())
    };
    let (addr, _requests) = serve(vec![reply]).await;
    let client = client_for(addr, limit as u64);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    let Error::TooLarge {
        max_bytes,
        declared_bytes,
    } = &err
    else {
        panic!("expected TooLarge, got {err:?}");
    };
    assert_eq!(*max_bytes, limit as u64);
    assert_eq!(*declared_bytes, Some((limit + 1) as u64));
}

#[tokio::test]
async fn declared_at_limit_actual_shorter_parses() {
    // Ported from QueryAsync_DeclaredAtLimit_ActualShorter_Parses (early close reads as EOF).
    let body = search_xml("Example Release");
    let limit = body.len() + 32;
    let reply = Reply {
        content_length: Some(limit),
        ..Reply::ok(body)
    };
    let (addr, _requests) = serve(vec![reply]).await;
    let client = client_for(addr, limit as u64);

    let items = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(items[0].title, "Example Release");
}

#[tokio::test]
async fn bytes_beyond_declared_length_are_cut_by_http_framing_not_buffered() {
    // v1's DeclaredBelowLimit_ActualOver_RejectsByStreamCount relied on .NET handing the raw
    // stream to the bounded reader. hyper enforces the declared Content-Length at the
    // framing layer, so bytes beyond it are never delivered to the body — the client can
    // never buffer more than the declared length here, and over-limit stream-count
    // rejection is covered by the chunked/undeclared cases. Documented deviation.
    let body = search_xml("Example Release");
    let limit = body.len();
    let over = pad_utf8(&body, limit + 1);
    let reply = Reply {
        content_length: Some(limit - 1),
        ..Reply::ok(over)
    };
    let (addr, _requests) = serve(vec![reply]).await;
    let client = client_for(addr, limit as u64);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        !matches!(err, Error::TooLarge { .. }),
        "framing cut the body, no over-limit buffering: {err:?}"
    );
}

#[tokio::test]
async fn chunked_undeclared_exact_limit_succeeds() {
    // Ported from QueryAsync_ChunkedUndeclared_ExactLimitSucceeds.
    let body = search_xml("Example Release");
    let limit = body.len();
    let reply = Reply {
        content_length: None,
        chunked: true,
        ..Reply::ok(body)
    };
    let (addr, _requests) = serve(vec![reply]).await;
    let client = client_for(addr, limit as u64);

    let items = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(items[0].title, "Example Release");
}

#[tokio::test]
async fn non_eof_oversize_rejects_before_eager_buffering() {
    // Ported from QueryAsync_NonEofOversize_RejectsBeforeEagerBuffering: the oversize body is
    // sent as one prefix and the connection then stays open — the client must reject as soon
    // as the limit is exceeded, without waiting for EOF.
    let body = search_xml(BODY_MARKER);
    let limit = body.len();
    let over = pad_utf8(&body, limit + 1);
    let reply = Reply {
        content_length: None,
        block: true,
        ..Reply::ok(over)
    };
    let (addr, _requests) = serve(vec![reply]).await;
    let client = client_for(addr, limit as u64);

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        client.search("query", 100, &CancellationToken::new()),
    )
    .await
    .expect("must reject without waiting for EOF");

    let err = result.unwrap_err();
    let Error::TooLarge { max_bytes, .. } = &err else {
        panic!("expected TooLarge, got {err:?}");
    };
    assert_eq!(*max_bytes, limit as u64);
}

#[tokio::test]
async fn multibyte_utf8_title_is_bounded_by_bytes() {
    // Ported from QueryAsync_MultibyteUtf8Title_IsBoundedByBytes.
    let title = "Café 🏴";
    let body = search_xml(title);
    assert!(title.chars().count() < body.len());

    let (addr, _requests) = serve(vec![Reply::ok(body.clone())]).await;
    let client = client_for(addr, body.len() as u64);
    let items = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(items[0].title, title);

    let (addr2, _requests2) = serve(vec![Reply::ok(body.clone())]).await;
    let client2 = client_for(addr2, (body.len() - 1) as u64);
    let err = client2
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::TooLarge { .. }), "{err:?}");
}

#[tokio::test]
async fn caller_cancellation_during_body_read_is_cancellation() {
    // Ported from QueryAsync_CallerCancellationDuringBodyRead_IsCancellation.
    let (addr, requests) = serve(vec![Reply {
        content_length: Some(4096),
        send_body: false,
        block: true,
        ..Reply::ok(Vec::new())
    }])
    .await;
    let ct = CancellationToken::new();

    let task_ct = ct.clone();
    let task = tokio::spawn(async move {
        let client = client_for(addr, 4096);
        client.search("query", 100, &task_ct).await
    });
    // wait until the server recorded the request (i.e. the body read is pending)
    for _ in 0..100 {
        if requests.count() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(requests.count(), 1);
    ct.cancel();

    let err = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("cancelled task must finish")
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
}

#[tokio::test]
async fn already_cancelled_does_not_parse_or_send() {
    // Ported from QueryAsync_AlreadyCancelled_DoesNotParse.
    let (addr, requests) = serve(vec![Reply::ok(search_xml("Example Release"))]).await;
    let client = client_for(addr, 4096);
    let ct = CancellationToken::new();
    ct.cancel();

    let err = client.search("query", 100, &ct).await.unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert_eq!(requests.count(), 0);
}

#[tokio::test]
async fn malformed_xml_is_controlled_format_error() {
    // Ported from QueryAsync_MalformedXml_IsControlledFormatError.
    let xml =
        br#"<?xml version="1.0"?><rss><channel><item><title>x</item></channel></rss>"#.to_vec();
    let (addr, _requests) = serve(vec![Reply::ok(xml)]).await;
    let client = client_for(addr, 4096);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    let Error::Format { .. } = &err else {
        panic!("expected Format, got {err:?}");
    };
    let msg = err.to_string();
    assert!(
        msg.starts_with("Indexer returned invalid XML (HTTP 200, content-type none, "),
        "{msg}"
    );
    assert!(msg.contains("XML error at byte offset "), "{msg}");
    assert!(!msg.contains("<item>"));
    assert!(!format!("{err:?}").contains(API_KEY));
    assert!(!msg.contains(BODY_MARKER));
}

#[tokio::test]
async fn html_response_reports_content_type_without_leaking_body() {
    // The Prowlarr failure mode: a 200 answer that is the web UI/SPA shell or a login
    // page instead of Newznab XML. The content-type makes that visible in logs/UI.
    let html = format!("<!DOCTYPE html><html><body>{BODY_MARKER}</body></html>").into_bytes();
    let reply = Reply {
        content_type: Some("text/html".to_owned()),
        ..Reply::ok(html)
    };
    let (addr, _requests) = serve(vec![reply]).await;
    let client = client_for(addr, 4096);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    let Error::Format { content_type, .. } = &err else {
        panic!("expected Format, got {err:?}");
    };
    assert_eq!(content_type, "text/html");
    let msg = err.to_string();
    assert!(
        msg.starts_with("Indexer returned invalid XML (HTTP 200, content-type text/html, "),
        "{msg}"
    );
    assert!(!msg.contains(BODY_MARKER));
    assert!(!msg.contains(API_KEY));
    assert!(!format!("{err:?}").contains(API_KEY));
}

#[tokio::test]
async fn content_type_reflecting_key_is_sanitized_to_unknown() {
    let reply = Reply {
        content_type: Some(format!("text/{API_KEY}")),
        ..Reply::ok(b"not xml at all".to_vec())
    };
    let (addr, _requests) = serve(vec![reply]).await;
    let client = client_for(addr, 4096);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    let Error::Format { content_type, .. } = &err else {
        panic!("expected Format, got {err:?}");
    };
    assert_eq!(content_type, "unknown");
    assert!(!err.to_string().contains(API_KEY));
    assert!(!format!("{err:?}").contains(API_KEY));
}

#[tokio::test]
async fn char_refs_and_predefined_entities_resolve_like_dotnet() {
    // quick-xml splits every `&...;` in text into a GeneralRef event; numeric character
    // references (common in RSS titles) and predefined entities must resolve, as
    // .NET XmlReader does, instead of failing the whole feed.
    let xml = br#"<?xml version="1.0"?><rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/"><channel><item><title>Tom &#x27; Jerry &amp; Co &#39;</title><guid>g1</guid><link>http://nzb.example/a.nzb</link><enclosure url="http://nzb.example/dl?x=1&amp;y=2&#x26;z=3" length="10"/></item></channel></rss>"#.to_vec();
    let (addr, _requests) = serve(vec![Reply::ok(xml)]).await;
    let client = client_for(addr, 4096);

    let items = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap();

    let item = items.first().expect("one item");
    assert_eq!(item.title, "Tom ' Jerry & Co '");
    assert_eq!(item.nzb_url, "http://nzb.example/dl?x=1&y=2&z=3");
}

#[tokio::test]
async fn prowlarr_style_feed_with_amp_links_parses() {
    // The full Prowlarr feed shape: atom:link in the channel, a prowlarrindexer element,
    // and '&amp;' inside the download link and enclosure URL — the case that broke
    // parsing before character references/predefined entities were resolved.
    let xml = concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        r#"<rss version="1.0" xmlns:atom="http://www.w3.org/2005/Atom" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">"#,
        r#"<channel><atom:link rel="self" type="application/rss+xml" /><title>Prowlarr</title>"#,
        r#"<item><title>Example Release</title><description>Example Release</description>"#,
        r#"<guid>https://indexer.example/details/abc123</guid>"#,
        r#"<prowlarrindexer id="1" type="private">ExampleIndexer</prowlarrindexer>"#,
        r#"<pubDate>Fri, 27 Nov 2026 18:08:28 +0000</pubDate><size>8235519</size>"#,
        r#"<link>http://localhost:9696/1/download?apikey=k&amp;link=aHh4&amp;file=Example</link>"#,
        r#"<category>7010</category>"#,
        r#"<enclosure url="http://localhost:9696/1/download?apikey=k&amp;link=aHh4&amp;file=Example" length="8235519" type="application/x-nzb" />"#,
        r#"<newznab:attr name="grabs" value="9" /></item>"#,
        r#"</channel></rss>"#
    );
    let (addr, _requests) = serve(vec![Reply::ok(xml.as_bytes().to_vec())]).await;
    let client = client_for(addr, 1 << 20);

    let items = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap();

    let item = items.first().expect("one item");
    assert_eq!(item.title, "Example Release");
    assert_eq!(
        item.nzb_url,
        "http://localhost:9696/1/download?apikey=k&link=aHh4&file=Example"
    );
    assert_eq!(item.size, 8_235_519);
    assert_eq!(item.grabs, Some(9));
}

#[tokio::test]
async fn undeclared_entity_is_format_error_without_leaking_name() {
    // With DTDs prohibited, a named non-predefined entity is not well-formed; .NET
    // rejects it too. The error must not contain the entity/body text.
    let xml = format!(
        r#"<?xml version="1.0"?><rss><channel><item><title>&{BODY_MARKER};</title></item></channel></rss>"#
    )
    .into_bytes();
    let (addr, _requests) = serve(vec![Reply::ok(xml)]).await;
    let client = client_for(addr, 4096);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Format { .. }), "{err:?}");
    assert!(!err.to_string().contains(BODY_MARKER));
    assert!(!format!("{err:?}").contains(BODY_MARKER));
}

#[tokio::test]
async fn illegal_char_ref_is_format_error() {
    // &#x0B; (vertical tab) is not a legal XML 1.0 character; .NET rejects it as well.
    let xml = br#"<?xml version="1.0"?><rss><channel><item><title>a&#x0B;b</title></item></channel></rss>"#
        .to_vec();
    let (addr, _requests) = serve(vec![Reply::ok(xml)]).await;
    let client = client_for(addr, 4096);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Format { .. }), "{err:?}");
}

#[tokio::test]
async fn newznab_error_element_is_protocol_error_not_format() {
    // Ported from QueryAsync_NewznabErrorElement_IsProtocolErrorNotFormat.
    let xml =
        br#"<?xml version="1.0"?><error code="100" description="Missing parameter"/>"#.to_vec();
    let (addr, _requests) = serve(vec![Reply::ok(xml)]).await;
    let client = client_for(addr, 4096);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    let Error::Indexer(msg) = &err else {
        panic!("expected Indexer, got {err:?}");
    };
    assert!(msg.contains("[100]"), "{msg}");
    assert!(msg.contains("Missing parameter"), "{msg}");
}

#[tokio::test]
async fn doctype_is_format_error_without_resolver_access() {
    // Ported from QueryAsync_Doctype_IsFormatErrorWithoutResolverAccess.
    let xml = br#"<?xml version="1.0"?><!DOCTYPE caps SYSTEM "http://127.0.0.1:1/should-not-resolve"><caps></caps>"#.to_vec();
    let (addr, _requests) = serve(vec![Reply::ok(xml)]).await;
    let client = client_for(addr, 4096);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Format { .. }), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.starts_with("Indexer returned invalid XML (HTTP 200, content-type none, "),
        "{msg}"
    );
    assert!(!msg.contains("127.0.0.1"));
}

#[tokio::test]
async fn non_success_status_does_not_retry() {
    // Ported from QueryAsync_NonSuccessStatus_DoesNotRetry.
    let reply = Reply {
        status: "502 Bad Gateway",
        ..Reply::ok(Vec::new())
    };
    let (addr, requests) = serve(vec![reply]).await;
    let client = client_for(addr, 4096);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    assert!(
        matches!(err, Error::Status(s) if s.as_u16() == 502),
        "{err:?}"
    );
    assert_eq!(requests.count(), 1);
}

#[tokio::test]
async fn get_retries_once_on_first_transport_failure() {
    // Ported from GetAsync_RetriesOnceOnFirstTransportFailure.
    let body = search_xml("Example Release");
    let drop = Reply {
        drop: true,
        ..Reply::ok(Vec::new())
    };
    let (addr, requests) = serve(vec![drop, Reply::ok(body)]).await;
    let client = client_for(addr, 4096);

    let items = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(items[0].title, "Example Release");
    assert_eq!(requests.count(), 2);
}

#[tokio::test]
async fn oversize_does_not_embed_api_key_or_body() {
    // Ported from QueryAsync_Oversize_DoesNotEmbedApiKeyOrBody.
    let (addr, requests) = serve(vec![Reply::ok(search_xml(BODY_MARKER))]).await;
    let client = client_for(addr, 8);

    let err = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap_err();

    assert!(matches!(err, Error::TooLarge { .. }), "{err:?}");
    assert!(!format!("{err:?}").contains(API_KEY));
    assert!(!err.to_string().contains(BODY_MARKER));
    assert_eq!(requests.count(), 1);
}

#[tokio::test]
async fn full_item_fields_are_parsed_like_v1() {
    // Characterization of ParseItem: newznab attrs (case-insensitive names, first non-empty
    // value wins), enclosure length over size attr, link fallback, pubDate/usenetdate,
    // negative counts clamp to 0, unparseable numbers become None/0, element fallbacks.
    let xml = br#"<?xml version="1.0"?><rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/"><channel>
<item>
  <title>Rich Release</title><guid>g-42</guid>
  <link>http://nzb.example/fallback.nzb</link>
  <pubDate>Fri, 02 Jan 2026 15:04:05 +0000</pubDate>
  <enclosure url="http://nzb.example/a.nzb" length="12345"/>
  <newznab:attr name="grabs" value="42"/>
  <newznab:attr name="Grabs" value="7"/>
  <newznab:attr name="comments" value="-3"/>
  <newznab:attr name="password" value="notanum"/>
  <newznab:attr name="files" value="12"/>
  <newznab:attr name="group" value="alt.binaries.test"/>
  <newznab:attr name="language" value="German"/>
  <newznab:attr name="subs" value="en,de"/>
  <newznab:attr name="infohash" value="abc123"/>
  <newznab:attr name="usenetdate" value="Tue, 06 Jan 2026 10:00:00 +0100"/>
  <newznab:attr name="poster" value="bot@example"/>
  <newznab:attr name="sourceIndexerName" value="MyIndexer"/>
</item>
<item>
  <title>Fallback Source</title><guid>g-2</guid>
  <enclosure length="99"/>
  <link>http://nzb.example/b.nzb</link>
  <jackettindexer>JackettSource</jackettindexer>
  <newznab:attr name="size" value="777"/>
</item>
<item>
  <title>Bad Size</title><guid>g-3</guid>
  <newznab:attr name="size" value="not-a-number"/>
</item>
<item>
  <title>Size Attr Fallback</title><guid>g-4</guid>
  <enclosure/>
  <newznab:attr name="size" value="555"/>
</item>
</channel></rss>"#
    .to_vec();
    let (addr, _requests) = serve(vec![Reply::ok(xml)]).await;
    let client = client_for(addr, 1 << 20);

    let items = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(items.len(), 4);

    let rich = &items[0];
    assert_eq!(rich.title, "Rich Release");
    assert_eq!(rich.guid, "g-42");
    assert_eq!(rich.nzb_url, "http://nzb.example/a.nzb");
    assert_eq!(rich.size, 12345, "enclosure length wins over size attr");
    assert_eq!(rich.posted.unwrap().to_string(), "2026-01-02T15:04:05Z");
    assert_eq!(
        rich.usenet_date.unwrap().to_string(),
        "2026-01-06T09:00:00Z"
    );
    assert_eq!(rich.grabs, Some(42), "first non-empty attr value wins");
    assert_eq!(rich.comments, Some(0), "negative clamps to 0");
    assert_eq!(rich.password, None, "unparseable count is None");
    assert_eq!(rich.files, Some(12));
    assert_eq!(rich.group.as_deref(), Some("alt.binaries.test"));
    assert_eq!(rich.poster.as_deref(), Some("bot@example"));
    assert_eq!(rich.source_indexer_name.as_deref(), Some("MyIndexer"));
    assert_eq!(rich.language.as_deref(), Some("German"));
    assert_eq!(rich.subs.as_deref(), Some("en,de"));
    assert_eq!(rich.info_hash.as_deref(), Some("abc123"));

    let fallback = &items[1];
    assert_eq!(
        fallback.nzb_url, "http://nzb.example/b.nzb",
        "link fallback"
    );
    assert_eq!(fallback.size, 99, "enclosure length wins when present");
    assert_eq!(
        fallback.source_indexer_name.as_deref(),
        Some("JackettSource"),
        "element fallback"
    );

    let bad = &items[2];
    assert_eq!(bad.size, 0, "unparseable size becomes 0");
    assert_eq!(bad.nzb_url, "");

    let attr_size = &items[3];
    assert_eq!(
        attr_size.size, 555,
        "size attr when enclosure has no length"
    );
    assert_eq!(attr_size.nzb_url, "", "enclosure without url, no link");
}

#[tokio::test]
async fn base_url_query_is_preserved_and_api_path_normalized() {
    // NormalizeApiUri/BuildUrl: existing query kept, params appended, /api appended unless
    // already present (case-insensitive).
    let (addr, requests) = serve(vec![Reply::ok(search_xml("Example Release"))]).await;
    let client = NewznabClient::new(&format!("http://{addr}/base?foo=bar"), API_KEY, 4096).unwrap();
    client
        .search("a b&c", 5, &CancellationToken::new())
        .await
        .unwrap();
    let target = requests.first().target;
    assert!(target.starts_with("/base/api?foo=bar&apikey="), "{target}");
    assert!(target.contains("extended=1"), "{target}");
    assert!(target.contains("q=a%20b%26c"), "{target}");
    assert!(target.contains("limit=5"), "{target}");
}

static PROXY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct ProxyGuard;

impl ProxyGuard {
    fn set(url: &str) -> Self {
        infinidysk_proxy::set_global(infinidysk_proxy::OutboundProxy::new(Some(url), None));
        Self
    }
}

impl Drop for ProxyGuard {
    fn drop(&mut self) {
        infinidysk_proxy::set_global(infinidysk_proxy::OutboundProxy::default());
    }
}

/// A fake SOCKS5 proxy on loopback that records the requested host:port and tunnels
/// everything to `target` (like the nntp proxy test's fake proxy).
async fn fake_socks5_proxy(target: SocketAddr) -> (SocketAddr, Arc<Mutex<Vec<(String, u16)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let seen: Arc<Mutex<Vec<(String, u16)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let seen = seen2.clone();
            let _ = tokio::spawn(async move {
                let mut greeting = [0u8; 3];
                client.read_exact(&mut greeting).await?;
                client.write_all(&[0x05, 0x00]).await?;
                let mut header = [0u8; 4];
                client.read_exact(&mut header).await?;
                let (host, port) = match header[3] {
                    0x03 => {
                        let mut len = [0u8; 1];
                        client.read_exact(&mut len).await?;
                        let mut h = vec![0u8; usize::from(len[0])];
                        client.read_exact(&mut h).await?;
                        let mut p = [0u8; 2];
                        client.read_exact(&mut p).await?;
                        (
                            String::from_utf8(h).expect("utf8 host"),
                            u16::from_be_bytes(p),
                        )
                    }
                    0x01 => {
                        let mut h = [0u8; 4];
                        client.read_exact(&mut h).await?;
                        let mut p = [0u8; 2];
                        client.read_exact(&mut p).await?;
                        (
                            format!("{}.{}.{}.{}", h[0], h[1], h[2], h[3]),
                            u16::from_be_bytes(p),
                        )
                    }
                    _ => return Ok(()),
                };
                seen.lock().expect("seen lock").push((host, port));
                let upstream = TcpStream::connect(target).await?;
                client
                    .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await?;
                let (mut cr, mut cw) = tokio::io::split(client);
                let (mut ur, mut uw) = tokio::io::split(upstream);
                let up = tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut cr, &mut uw).await;
                });
                let down = tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut ur, &mut cw).await;
                });
                let _ = tokio::join!(up, down);
                Ok::<(), std::io::Error>(())
            })
            .await;
        }
    });
    (addr, seen)
}

#[tokio::test]
async fn query_tunnels_through_global_socks5_proxy_when_not_bypassed() {
    let _lock = PROXY_LOCK.lock().await;
    let (http_addr, requests) = serve(vec![Reply::ok(search_xml("Example Release"))]).await;
    let (proxy_addr, seen) = fake_socks5_proxy(http_addr).await;
    let _guard = ProxyGuard::set(&format!("socks5://127.0.0.1:{}", proxy_addr.port()));

    // Non-local indexer host: the request must go through the proxy as a domain (socks5h).
    let client = NewznabClient::new(
        &format!("http://indexer.example:{}/api", http_addr.port()),
        API_KEY,
        4096,
    )
    .unwrap();
    let items = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(items[0].title, "Example Release");
    assert_eq!(requests.count(), 1, "the indexer server saw the request");
    let seen = seen.lock().expect("seen lock");
    assert_eq!(
        seen.as_slice(),
        &[("indexer.example".to_owned(), http_addr.port())],
        "proxy received the host as a domain (ATYP 0x03), remote DNS"
    );
}

#[tokio::test]
async fn loopback_indexer_stays_direct_despite_global_proxy() {
    let _lock = PROXY_LOCK.lock().await;
    let (http_addr, requests) = serve(vec![Reply::ok(search_xml("Example Release"))]).await;
    let (proxy_addr, seen) = fake_socks5_proxy(http_addr).await;
    let _guard = ProxyGuard::set(&format!("socks5://127.0.0.1:{}", proxy_addr.port()));

    // Loopback destinations are always local → bypassed → direct connection.
    let client = client_for(http_addr, 4096);
    let items = client
        .search("query", 100, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(items[0].title, "Example Release");
    assert_eq!(requests.count(), 1);
    assert!(
        seen.lock().expect("seen lock").is_empty(),
        "proxy must not be used"
    );
}
