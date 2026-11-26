//! The SOCKS5 CONNECT tunnel used for NNTP connections, ported from
//! `libs/UsenetSharp/Clients/UsenetClient.Socks5.cs` (RFC 1928, with optional RFC 1929
//! username/password sub-negotiation).

use std::io;
use std::net::IpAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const VERSION: u8 = 0x05;
const AUTH_NONE: u8 = 0x00;
const AUTH_USER_PASS: u8 = 0x02;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
const REPLY_SUCCEEDED: u8 = 0x00;
const NO_ACCEPTABLE_METHODS: u8 = 0xFF;
/// Optional RFC 1929 username/password credentials. `Debug` never prints the password.
#[derive(Clone, Default)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Credentials {
    /// True when username/password authentication should be offered.
    #[must_use]
    pub fn any(&self) -> bool {
        !self.username.is_empty() || !self.password.is_empty()
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// Why the SOCKS5 tunnel to the destination could not be established: the proxy rejected
/// the handshake, the authentication, or the CONNECT request.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("socks5 proxy i/o failed")]
    Io(#[from] io::Error),
    #[error("the socks5 proxy closed the connection during the handshake")]
    Closed,
    #[error("the socks5 proxy replied with an unexpected protocol version {0}")]
    Version(u8),
    #[error("the socks5 proxy rejected all offered authentication methods")]
    NoAcceptableMethods,
    #[error("the socks5 proxy selected unsupported authentication method 0x{0:02x}")]
    UnsupportedMethod(u8),
    #[error(
        "the socks5 proxy requires username/password authentication but no credentials are configured"
    )]
    CredentialsRequired,
    #[error("socks5 proxy credentials exceed the RFC 1929 limit of 255 bytes")]
    CredentialsTooLong,
    #[error("the socks5 proxy sent an unexpected auth reply version {0}")]
    AuthVersion(u8),
    #[error("the socks5 proxy rejected the configured username/password credentials")]
    AuthRejected,
    #[error("the socks5 proxy refused the connection to {host}:{port} (reply code {code})")]
    Refused { host: String, port: u16, code: u8 },
    #[error("the socks5 proxy sent an unknown address type 0x{0:02x}")]
    UnknownAddressType(u8),
    #[error("the destination host name is too long for socks5 (max 255 bytes, got {0})")]
    HostTooLong(usize),
}

/// Performs the SOCKS5 CONNECT handshake over `stream`, which is already connected to the
/// proxy, and returns it positioned at the first byte of the tunneled session.
///
/// Host names are sent as domain addresses so DNS resolution happens on the proxy
/// (socks5h semantics); TLS layered on top of the returned stream stays end-to-end between
/// client and destination. The whole handshake must finish within `timeout`.
pub async fn connect(
    mut stream: TcpStream,
    host: &str,
    port: u16,
    credentials: Option<&Credentials>,
    timeout: Duration,
) -> Result<TcpStream, Error> {
    tokio::time::timeout(timeout, handshake(&mut stream, host, port, credentials))
        .await
        .map_err(|_| Error::Io(io::Error::new(io::ErrorKind::TimedOut, "socks5 handshake")))??;
    Ok(stream)
}

async fn handshake(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    credentials: Option<&Credentials>,
) -> Result<(), Error> {
    // --- Method negotiation (RFC 1928 section 3) ---
    let methods: &[u8] = match credentials {
        Some(c) if c.any() => &[AUTH_NONE, AUTH_USER_PASS],
        _ => &[AUTH_NONE],
    };
    let mut greeting = Vec::with_capacity(2 + methods.len());
    // At most two methods are ever offered.
    greeting.extend_from_slice(&[VERSION, u8::try_from(methods.len()).unwrap_or(u8::MAX)]);
    greeting.extend_from_slice(methods);
    write_all(stream, &greeting).await?;

    let mut method_reply = [0u8; 2];
    read_exact(stream, &mut method_reply).await?;
    if method_reply[0] != VERSION {
        return Err(Error::Version(method_reply[0]));
    }
    let method = method_reply[1];
    if method == NO_ACCEPTABLE_METHODS {
        return Err(Error::NoAcceptableMethods);
    }
    if method != AUTH_NONE && method != AUTH_USER_PASS {
        return Err(Error::UnsupportedMethod(method));
    }

    // --- Optional username/password sub-negotiation (RFC 1929) ---
    if method == AUTH_USER_PASS {
        let credentials = credentials.ok_or(Error::CredentialsRequired)?;
        let user = credentials.username.as_bytes();
        let password = credentials.password.as_bytes();
        let user_len = u8::try_from(user.len()).map_err(|_| Error::CredentialsTooLong)?;
        let password_len = u8::try_from(password.len()).map_err(|_| Error::CredentialsTooLong)?;
        let mut auth_request = Vec::with_capacity(3 + user.len() + password.len());
        auth_request.extend_from_slice(&[0x01, user_len]);
        auth_request.extend_from_slice(user);
        auth_request.extend_from_slice(&[password_len]);
        auth_request.extend_from_slice(password);
        write_all(stream, &auth_request).await?;

        let mut auth_reply = [0u8; 2];
        read_exact(stream, &mut auth_reply).await?;
        if auth_reply[0] != 0x01 {
            return Err(Error::AuthVersion(auth_reply[0]));
        }
        if auth_reply[1] != 0x00 {
            return Err(Error::AuthRejected);
        }
    }

    // --- CONNECT request ---
    // RFC 1928 caps a domain address at 255 bytes.
    let mut request = Vec::with_capacity(4 + 1 + 255 + 2);
    request.extend_from_slice(&[VERSION, CMD_CONNECT, 0x00]);
    if let Ok(address) = host.parse::<IpAddr>() {
        match address {
            IpAddr::V4(v4) => {
                request.push(ATYP_IPV4);
                request.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                request.push(ATYP_IPV6);
                request.extend_from_slice(&v6.octets());
            }
        }
    } else {
        let host_bytes = host.as_bytes();
        let host_len =
            u8::try_from(host_bytes.len()).map_err(|_| Error::HostTooLong(host_bytes.len()))?;
        request.push(ATYP_DOMAIN);
        request.push(host_len);
        request.extend_from_slice(host_bytes);
    }
    request.extend_from_slice(&port.to_be_bytes());
    write_all(stream, &request).await?;

    // --- CONNECT reply; drain BND.ADDR and BND.PORT so the first byte of the tunneled
    //     session is not corrupted. ---
    let mut reply_header = [0u8; 4];
    read_exact(stream, &mut reply_header).await?;
    if reply_header[0] != VERSION {
        return Err(Error::Version(reply_header[0]));
    }
    if reply_header[1] != REPLY_SUCCEEDED {
        return Err(Error::Refused {
            host: host.to_owned(),
            port,
            code: reply_header[1],
        });
    }
    let bound_len = match reply_header[3] {
        ATYP_IPV4 => 4,
        ATYP_IPV6 => 16,
        ATYP_DOMAIN => {
            let mut length = [0u8; 1];
            read_exact(stream, &mut length).await?;
            usize::from(length[0])
        }
        other => return Err(Error::UnknownAddressType(other)),
    };
    skip_exact(stream, bound_len).await?;
    skip_exact(stream, 2).await?;
    Ok(())
}

async fn write_all(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), Error> {
    stream.write_all(bytes).await.map_err(Error::Io)
}

async fn read_exact(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<(), Error> {
    stream
        .read_exact(buffer)
        .await
        .map(|_| ())
        .map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
            ) {
                Error::Closed
            } else {
                Error::Io(error)
            }
        })
}

async fn skip_exact(stream: &mut TcpStream, mut count: usize) -> Result<(), Error> {
    let mut buffer = [0u8; 32];
    while count > 0 {
        let len = count.min(buffer.len());
        let read = stream.read(&mut buffer[..len]).await.map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
            ) {
                Error::Closed
            } else {
                Error::Io(error)
            }
        })?;
        if read == 0 {
            return Err(Error::Closed);
        }
        count -= read;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// A fake SOCKS5 server on loopback; the handler sees the connected stream.
    struct FakeServer {
        addr: std::net::SocketAddr,
        handle: tokio::task::JoinHandle<()>,
    }

    impl FakeServer {
        async fn start<F, Fut>(handler: F) -> Self
        where
            F: FnOnce(TcpStream) -> Fut + Send + 'static,
            Fut: std::future::Future<Output = std::io::Result<()>> + Send + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                handler(stream).await.unwrap();
            });
            Self { addr, handle }
        }
    }

    async fn connect_to(server: &FakeServer) -> TcpStream {
        TcpStream::connect(server.addr).await.unwrap()
    }

    async fn read_exactly(stream: &mut TcpStream, len: usize) -> Vec<u8> {
        let mut buffer = vec![0u8; len];
        stream.read_exact(&mut buffer).await.unwrap();
        buffer
    }

    async fn send_reply(stream: &mut TcpStream, code: u8, atyp: u8, addr: &[u8]) {
        let mut reply = vec![VERSION, code, 0x00, atyp];
        if atyp == ATYP_DOMAIN {
            reply.push(u8::try_from(addr.len()).unwrap());
        }
        reply.extend_from_slice(addr);
        reply.extend_from_slice(&[0x00, 0x00]);
        stream.write_all(&reply).await.unwrap();
    }

    #[tokio::test]
    async fn domain_destination_is_sent_as_domain_and_tunnel_is_ready() {
        // Ported from Socks5ConnectTests.ConnectAsync_Socks5Proxy_SendsDomainDestinationAndReadsGreeting.
        let server = FakeServer::start(|mut stream| async move {
            let greeting = read_exactly(&mut stream, 3).await;
            assert_eq!(greeting, [VERSION, 0x01, AUTH_NONE]);
            stream.write_all(&[VERSION, AUTH_NONE]).await?;
            let header = read_exactly(&mut stream, 4).await;
            assert_eq!(header[..3], [VERSION, CMD_CONNECT, 0x00]);
            assert_eq!(header[3], ATYP_DOMAIN);
            let length = usize::from(read_exactly(&mut stream, 1).await[0]);
            let host = read_exactly(&mut stream, length).await;
            assert_eq!(host, b"nntp.example.com");
            let port = read_exactly(&mut stream, 2).await;
            assert_eq!(u16::from_be_bytes([port[0], port[1]]), 119);
            send_reply(&mut stream, 0x00, ATYP_DOMAIN, b"proxy.local").await;
            // The tunnel is positioned at the first byte of the destination session.
            stream.write_all(b"200 hello\r\n").await?;
            Ok(())
        })
        .await;

        let stream = connect_to(&server).await;
        let mut tunneled = connect(
            stream,
            "nntp.example.com",
            119,
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let mut line = Vec::new();
        tunneled.read_to_end(&mut line).await.unwrap();
        assert_eq!(line, b"200 hello\r\n");
        server.handle.await.unwrap();
    }

    #[tokio::test]
    async fn ipv4_literal_destination_uses_ipv4_address_type() {
        // Ported from Socks5ConnectTests.ConnectAsync_Socks5Proxy_Ipv4LiteralDestination_UsesIPv4AddressType.
        let server = FakeServer::start(|mut stream| async move {
            let greeting = read_exactly(&mut stream, 3).await;
            assert_eq!(greeting, [VERSION, 0x01, AUTH_NONE]);
            stream.write_all(&[VERSION, AUTH_NONE]).await?;
            let header = read_exactly(&mut stream, 4).await;
            assert_eq!(header[3], ATYP_IPV4);
            let address = read_exactly(&mut stream, 4).await;
            assert_eq!(address, [127, 0, 0, 1]);
            let port = read_exactly(&mut stream, 2).await;
            assert_eq!(u16::from_be_bytes([port[0], port[1]]), 119);
            send_reply(&mut stream, 0x00, ATYP_IPV4, &[127, 0, 0, 1]).await;
            Ok(())
        })
        .await;

        let stream = connect_to(&server).await;
        connect(stream, "127.0.0.1", 119, None, Duration::from_secs(5))
            .await
            .unwrap();
        server.handle.await.unwrap();
    }

    #[tokio::test]
    async fn method_rejected_throws() {
        // Ported from Socks5ConnectTests.ConnectAsync_Socks5Proxy_MethodRejected_ThrowsSocks5ProxyException.
        let server = FakeServer::start(|mut stream| async move {
            let greeting = read_exactly(&mut stream, 3).await;
            assert_eq!(greeting, [VERSION, 0x01, AUTH_NONE]);
            stream.write_all(&[VERSION, NO_ACCEPTABLE_METHODS]).await?;
            Ok(())
        })
        .await;

        let stream = connect_to(&server).await;
        let error = connect(
            stream,
            "nntp.example.com",
            119,
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Error::NoAcceptableMethods), "{error}");
        server.handle.await.unwrap();
    }

    #[tokio::test]
    async fn refused_reply_carries_the_reply_code() {
        // Ported from Socks5ConnectTests.ConnectAsync_Socks5Proxy_Refused_ThrowsSocks5ProxyExceptionWithReplyCode.
        let server = FakeServer::start(|mut stream| async move {
            let _greeting = read_exactly(&mut stream, 3).await;
            stream.write_all(&[VERSION, AUTH_NONE]).await?;
            let _header = read_exactly(&mut stream, 4).await;
            // ATYP_DOMAIN for a name: skip length + host + port.
            let length = usize::from(read_exactly(&mut stream, 1).await[0]);
            let _host = read_exactly(&mut stream, length).await;
            let _port = read_exactly(&mut stream, 2).await;
            send_reply(&mut stream, 0x05, ATYP_IPV4, &[0, 0, 0, 0]).await; // 0x05: connection refused
            Ok(())
        })
        .await;

        let stream = connect_to(&server).await;
        let error = connect(
            stream,
            "nntp.example.com",
            119,
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        match error {
            Error::Refused { host, port, code } => {
                assert_eq!(host, "nntp.example.com");
                assert_eq!(port, 119);
                assert_eq!(code, 0x05);
            }
            other => panic!("expected Refused, got {other}"),
        }
        server.handle.await.unwrap();
    }

    #[tokio::test]
    async fn closed_during_handshake_is_a_clean_error() {
        let server = FakeServer::start(|mut stream| async move {
            let _greeting = read_exactly(&mut stream, 3).await;
            drop(stream); // close without a method reply
            Ok(())
        })
        .await;

        let stream = connect_to(&server).await;
        let error = connect(
            stream,
            "nntp.example.com",
            119,
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Error::Closed), "{error}");
        server.handle.await.unwrap();
    }

    #[tokio::test]
    async fn username_password_auth_is_offered_and_answered() {
        let server = FakeServer::start(|mut stream| async move {
            let greeting = read_exactly(&mut stream, 4).await;
            assert_eq!(greeting, [VERSION, 0x02, AUTH_NONE, AUTH_USER_PASS]);
            stream.write_all(&[VERSION, AUTH_USER_PASS]).await?;
            let header = read_exactly(&mut stream, 2).await;
            assert_eq!(header[0], 0x01); // sub-negotiation version
            let user_len = usize::from(header[1]);
            let user = read_exactly(&mut stream, user_len).await;
            assert_eq!(user, b"user");
            let pass_len = usize::from(read_exactly(&mut stream, 1).await[0]);
            let pass = read_exactly(&mut stream, pass_len).await;
            assert_eq!(pass, b"pass");
            stream.write_all(&[0x01, 0x00]).await?; // auth success
            let header = read_exactly(&mut stream, 4).await;
            assert_eq!(header[..3], [VERSION, CMD_CONNECT, 0x00]);
            assert_eq!(header[3], ATYP_DOMAIN);
            let length = usize::from(read_exactly(&mut stream, 1).await[0]);
            let host = read_exactly(&mut stream, length).await;
            assert_eq!(host, b"nntp.example.com");
            let _port = read_exactly(&mut stream, 2).await;
            send_reply(&mut stream, 0x00, ATYP_IPV4, &[127, 0, 0, 1]).await;
            stream.write_all(b"tunneled").await?;
            Ok(())
        })
        .await;

        let credentials = Credentials {
            username: "user".to_owned(),
            password: "pass".to_owned(),
        };
        let stream = connect_to(&server).await;
        let mut tunneled = connect(
            stream,
            "nntp.example.com",
            119,
            Some(&credentials),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let mut data = Vec::new();
        tunneled.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, b"tunneled");
        server.handle.await.unwrap();
    }
}
