using System.Net;

namespace NzbWebDAV.Utils;

/// <summary>
/// Process-wide outbound SOCKS5 proxy configured through the
/// <c>general.socks5-proxy</c> setting (<c>socks5://host:port</c>).
/// </summary>
/// <remarks>
/// All outbound HTTP clients and NNTP connections consult this state. Local
/// destinations (loopback and single-label Docker service names) always connect
/// directly so a proxy on the same Docker network remains reachable, and so do the
/// destinations listed in <c>general.socks5-proxy-bypass</c>. Handlers read the
/// proxy per connection, so configuration changes apply to new connections
/// without rebuilding cached clients.
/// </remarks>
public static class OutboundProxy
{
    private static volatile Uri? _socks5Uri;
    private static volatile ProxyBypassMatcher _bypass = ProxyBypassMatcher.Empty;

    /// <summary>True when a SOCKS5 proxy URL is currently configured.</summary>
    public static bool IsConfigured => _socks5Uri is not null;

    /// <summary>The normalized <c>socks5://host:port</c> proxy URI, or null when unset.</summary>
    public static Uri? Socks5Uri => _socks5Uri;

    /// <summary>The currently configured proxy exceptions; empty when unset.</summary>
    public static ProxyBypassMatcher Bypass => _bypass;

    /// <summary>
    /// A proxy for callers whose historical default was the environment proxy
    /// policy (HTTP_PROXY / HTTPS_PROXY / NO_PROXY). Unconfigured destinations keep
    /// delegating to <see cref="HttpClient.DefaultProxy"/>.
    /// </summary>
    public static readonly IWebProxy WebProxy = new OutboundWebProxy(fallbackToEnvironment: true);

    /// <summary>
    /// A proxy for callers whose historical default was a strict direct
    /// connection. Unconfigured destinations connect directly regardless of any
    /// proxy environment variables.
    /// </summary>
    public static readonly IWebProxy StrictWebProxy = new OutboundWebProxy(fallbackToEnvironment: false);

    /// <summary>
    /// Creates a <see cref="SocketsHttpHandler"/> that routes non-bypassed
    /// destinations through the configured SOCKS5 proxy and falls back to the
    /// environment proxy policy when unconfigured.
    /// </summary>
    public static SocketsHttpHandler CreateHandler() => new()
    {
        Proxy = WebProxy,
        UseProxy = true,
    };

    /// <summary>
    /// Applies new <c>general.socks5-proxy</c> and <c>general.socks5-proxy-bypass</c>
    /// config values. Empty or invalid values clear the proxy / the exceptions.
    /// </summary>
    public static void Update(string? configValue, string? bypassValue = null)
    {
        _socks5Uri = TryParseSocks5Url(configValue);
        _bypass = ProxyBypassMatcher.Parse(bypassValue);
    }

    /// <summary>
    /// Parses a SOCKS5 proxy URL. Accepts <c>socks5://host:port</c> and
    /// <c>socks5h://host:port</c> (both mean remote DNS here; the host name is
    /// forwarded for resolution by the proxy) and requires an explicit port.
    /// Credentials, query, and fragments are rejected.
    /// </summary>
    /// <returns>The normalized socks5 URI, or null when the value is empty or invalid.</returns>
    public static Uri? TryParseSocks5Url(string? raw)
    {
        if (string.IsNullOrWhiteSpace(raw))
            return null;
        if (!Uri.TryCreate(raw.Trim(), UriKind.Absolute, out var uri))
            return null;
        if (uri.Scheme != "socks5" && uri.Scheme != "socks5h")
            return null;
        if (string.IsNullOrWhiteSpace(uri.Host) || uri.Port is < 1 or > 65535)
            return null;
        if (!string.IsNullOrEmpty(uri.Query) || !string.IsNullOrEmpty(uri.Fragment))
            return null;
        if (!string.IsNullOrEmpty(uri.UserInfo))
            return null;
        // socks5 has no registered default port, so Uri.Port is -1 when no explicit
        // port is present and the check above already rejected it. Normalize
        // socks5h -> socks5 (both mean remote DNS here).
        if (uri.Scheme == "socks5h")
            return new UriBuilder(uri) { Scheme = "socks5", Port = uri.Port }.Uri;
        return uri;
    }

    /// <summary>
    /// True for destinations that must never be routed through the outbound
    /// proxy: loopback addresses and single-label host names (Docker service
    /// names such as <c>gluetun</c>).
    /// </summary>
    public static bool IsLocalDestination(Uri destination) =>
        destination.IsLoopback ||
        (destination.HostNameType == UriHostNameType.Dns && !destination.IdnHost.Contains('.', StringComparison.Ordinal));

    /// <summary>
    /// True for destinations that must never be routed through the outbound proxy:
    /// local destinations (see <see cref="IsLocalDestination"/>) plus every host
    /// matched by the configured <c>general.socks5-proxy-bypass</c> exceptions.
    /// </summary>
    public static bool IsBypassed(Uri destination) =>
        IsLocalDestination(destination) ||
        _bypass.Matches(destination.HostNameType == UriHostNameType.Dns ? destination.IdnHost : destination.Host);

    /// <summary>
    /// Host-based variant of <see cref="IsBypassed(Uri)"/> for callers that only have
    /// a bare host name, such as NNTP provider hosts.
    /// </summary>
    public static bool IsBypassedHost(string host) =>
        IsLocalHost(host) || _bypass.Matches(host);

    private static bool IsLocalHost(string host)
    {
        if (IPAddress.TryParse(host, out var address))
            return IPAddress.IsLoopback(address);
        // A colon means an IPv6 literal that failed to parse; never treat it as a name.
        if (host.Contains(':', StringComparison.Ordinal))
            return false;
        return !host.Contains('.', StringComparison.Ordinal);
    }

    private sealed class OutboundWebProxy(bool fallbackToEnvironment) : IWebProxy
    {
        // Always false: routing decisions are made in GetProxy so local
        // destinations can stay direct even while the proxy is configured.
        public bool IsBypassed(Uri host) => false;

        public Uri GetProxy(Uri destination)
        {
            var socks5 = _socks5Uri;
            if (socks5 is not null && !IsBypassed(destination))
                return socks5;
            if (fallbackToEnvironment && socks5 is null)
                return HttpClient.DefaultProxy.GetProxy(destination) ?? destination;
            return destination;
        }

        public ICredentials? Credentials { get; set; }
    }
}
