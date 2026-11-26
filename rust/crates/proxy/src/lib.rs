//! Process-wide outbound SOCKS5 proxy routing, ported from the C# v1 fork
//! (`backend/Utils/OutboundProxy.cs`, `backend/Utils/ProxyBypassMatcher.cs` and the SOCKS5
//! tunnel in `libs/UsenetSharp/Clients/UsenetClient.Socks5.cs`).
//!
//! All outbound connections (NNTP providers, HTTP clients) consult [`global`] per
//! connection, so configuration changes apply to new connections without rebuilding cached
//! clients. Local destinations — loopback addresses and single-label host names such as
//! Docker service names — always connect directly, as do the hosts listed in the bypass
//! setting.

mod bypass;
mod socks5;
mod url;

use std::net::IpAddr;
use std::sync::{LazyLock, PoisonError, RwLock};

pub use bypass::BypassMatcher;
pub use socks5::{Credentials, Error as Socks5Error, connect as socks5_connect};
pub use url::Socks5Url;

/// The outbound proxy configuration: an optional SOCKS5 proxy plus its bypass exceptions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutboundProxy {
    socks5: Option<Socks5Url>,
    bypass: BypassMatcher,
}

impl OutboundProxy {
    /// Builds from the raw `socks5-proxy` / `socks5-proxy-bypass` setting values. Empty or
    /// invalid values clear the proxy / the exceptions.
    #[must_use]
    pub fn new(socks5_raw: Option<&str>, bypass_raw: Option<&str>) -> Self {
        Self {
            socks5: Socks5Url::parse(socks5_raw),
            bypass: BypassMatcher::parse(bypass_raw),
        }
    }

    /// True when a SOCKS5 proxy URL is currently configured.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.socks5.is_some()
    }

    /// The proxy to route `host` through, or `None` when no proxy is configured or the
    /// destination is local or excepted and must connect directly.
    #[must_use]
    pub fn proxy_for(&self, host: &str) -> Option<&Socks5Url> {
        let socks5 = self.socks5.as_ref()?;
        if self.is_bypassed(host) {
            None
        } else {
            Some(socks5)
        }
    }

    /// True for destinations that must never be routed through the outbound proxy: local
    /// destinations (see [`is_local_host`]) plus every host matched by the configured bypass
    /// exceptions.
    #[must_use]
    pub fn is_bypassed(&self, host: &str) -> bool {
        is_local_host(host) || self.bypass.matches(host)
    }
}

/// True for destinations that must never be routed through the outbound proxy: loopback
/// addresses and single-label host names (Docker service names such as `gluetun`).
#[must_use]
pub fn is_local_host(host: &str) -> bool {
    if let Ok(address) = host.parse::<IpAddr>() {
        return address.is_loopback();
    }
    // A colon means an IPv6 literal that failed to parse; never treat it as a name.
    if host.contains(':') {
        return false;
    }
    !host.contains('.')
}

/// Process-wide proxy state. Read per connection, so saving settings applies to new
/// connections; the config layer publishes here on boot and on every save.
static GLOBAL: LazyLock<RwLock<OutboundProxy>> =
    LazyLock::new(|| RwLock::new(OutboundProxy::default()));

/// Replaces the process-wide proxy state.
pub fn set_global(proxy: OutboundProxy) {
    *GLOBAL.write().unwrap_or_else(PoisonError::into_inner) = proxy;
}

/// A snapshot of the process-wide proxy state.
#[must_use]
pub fn global() -> OutboundProxy {
    GLOBAL
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_local_host_classifies_destinations() {
        // Ported from OutboundProxyTests.IsLocalDestination / IsBypassedHost.
        for host in ["localhost", "127.0.0.1", "::1", "gluetun"] {
            assert!(is_local_host(host), "{host} is local");
        }
        for host in [
            "api.example.com",
            "1.2.3.4",
            "sub.gluetun.internal",
            "fe80::1",
            "192.168.1.5",
        ] {
            assert!(!is_local_host(host), "{host} is not local");
        }
    }

    #[test]
    fn proxy_for_routes_external_and_skips_bypassed() {
        let proxy = OutboundProxy::new(
            Some("socks5://gluetun:1080"),
            Some("api.internal, .lan, 192.168.0.0/16, 10.1.2.3"),
        );
        assert!(proxy.is_configured());
        assert_eq!(
            proxy.proxy_for("news.example.com").map(Socks5Url::port),
            Some(1080)
        );
        // Local destinations and configured exceptions stay direct.
        for host in [
            "gluetun",
            "localhost",
            "127.0.0.1",
            "::1",
            "sonarr.media.lan",
            "api.internal",
            "sub.api.internal",
            "192.168.1.20",
            "10.1.2.3",
        ] {
            assert!(proxy.proxy_for(host).is_none(), "{host} stays direct");
        }
        for host in ["api.example.com", "10.9.9.9", "internal.evil.com"] {
            assert!(proxy.proxy_for(host).is_some(), "{host} goes via proxy");
        }
    }

    #[test]
    fn unconfigured_or_invalid_never_proxies() {
        for raw in [None, Some(""), Some("   "), Some("http://gluetun:1080")] {
            let proxy = OutboundProxy::new(raw, None);
            assert!(!proxy.is_configured());
            assert!(proxy.proxy_for("news.example.com").is_none());
        }
    }
}
