//! Parsing of the `socks5-proxy` setting value, ported from
//! `OutboundProxy.TryParseSocks5Url`.

use std::fmt;

/// A parsed, normalized `socks5://host:port` proxy URL.
///
/// `socks5h://` input is accepted and normalized to `socks5`: both mean remote DNS here, the
/// host name is forwarded to the proxy for resolution. Credentials, query and fragment are
/// rejected; an explicit port is required.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Socks5Url {
    host: String,
    port: u16,
}

impl Socks5Url {
    /// Parses a SOCKS5 proxy URL, or `None` when the value is empty or invalid.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Option<Self> {
        let raw = raw?.trim();
        if raw.is_empty() || raw.chars().any(char::is_whitespace) {
            return None;
        }
        let (scheme, rest) = raw.split_once("://")?;
        if !scheme.eq_ignore_ascii_case("socks5") && !scheme.eq_ignore_ascii_case("socks5h") {
            return None;
        }
        // Credentials, query and fragment are rejected; a path after the authority is
        // ignored, like System.Uri treats it.
        if rest.contains(['?', '#']) {
            return None;
        }
        let authority = rest.split('/').next()?;
        if authority.contains('@') {
            return None;
        }
        let (host, port) = split_host_port(authority)?;
        if host.is_empty() {
            return None;
        }
        Some(Self {
            host: host.to_owned(),
            port,
        })
    }

    /// The proxy host name or IP literal, without brackets around an IPv6 literal.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The explicit proxy port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Display for Socks5Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "socks5://[{}]:{}", self.host, self.port)
        } else {
            write!(f, "socks5://{}:{}", self.host, self.port)
        }
    }
}

/// Splits `host:port`, with an optional bracketed IPv6 literal, and requires an explicit
/// port in 1..=65535.
fn split_host_port(authority: &str) -> Option<(&str, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?;
        let port = parse_port(port)?;
        return Some((host, port));
    }
    let (host, port) = authority.rsplit_once(':')?;
    // An unbracketed IPv6 literal has several colons; it is not a valid `host:port`.
    if host.contains(':') {
        return None;
    }
    Some((host, parse_port(port)?))
}

fn parse_port(port: &str) -> Option<u16> {
    let port: u16 = port.parse().ok()?;
    (port >= 1).then_some(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_or_whitespace_is_none() {
        // Ported from OutboundProxyTests.TryParseSocks5Url_EmptyOrWhitespace_ReturnsNull.
        for raw in [None, Some(""), Some("   ")] {
            assert!(Socks5Url::parse(raw).is_none());
        }
    }

    #[test]
    fn parse_invalid_is_none() {
        // Ported from OutboundProxyTests.TryParseSocks5Url_Invalid_ReturnsNull.
        for raw in [
            "not a url",
            "http://gluetun:1080",
            "https://gluetun:1080",
            "socks5://gluetun",                // no explicit port
            "socks5://gluetun:0",              // port 0
            "socks5://gluetun:70000",          // port out of range
            "socks5://gluetun:",               // empty port
            "socks5://:1080",                  // empty host
            "socks5://user:pass@gluetun:1080", // credentials unsupported
            "socks5://gluetun:1080?timeout=5",
            "socks5://gluetun:1080#section",
        ] {
            assert!(
                Socks5Url::parse(Some(raw)).is_none(),
                "expected invalid: {raw}"
            );
        }
    }

    #[test]
    fn parse_valid_socks5() {
        let url = Socks5Url::parse(Some("socks5://gluetun:1080")).unwrap();
        assert_eq!(url.host(), "gluetun");
        assert_eq!(url.port(), 1080);
        assert_eq!(url.to_string(), "socks5://gluetun:1080");
    }

    #[test]
    fn parse_socks5h_normalizes_to_socks5() {
        let url = Socks5Url::parse(Some(" socks5h://gluetun:9040 ")).unwrap();
        assert_eq!(url.host(), "gluetun");
        assert_eq!(url.port(), 9040);
        assert_eq!(url.to_string(), "socks5://gluetun:9040");
    }

    #[test]
    fn parse_scheme_is_case_insensitive() {
        let url = Socks5Url::parse(Some("SOCKS5H://gluetun:1080")).unwrap();
        assert_eq!(url.port(), 1080);
    }

    #[test]
    fn parse_ipv6_literal() {
        let url = Socks5Url::parse(Some("socks5://[::1]:1080")).unwrap();
        assert_eq!(url.host(), "::1");
        assert_eq!(url.port(), 1080);
        assert_eq!(url.to_string(), "socks5://[::1]:1080");
        // Unbracketed IPv6 is not a valid host:port authority.
        assert!(Socks5Url::parse(Some("socks5://::1:1080")).is_none());
    }

    #[test]
    fn parse_ipv4_literal() {
        let url = Socks5Url::parse(Some("socks5://127.0.0.1:1080")).unwrap();
        assert_eq!(url.host(), "127.0.0.1");
        assert_eq!(url.port(), 1080);
    }
}
