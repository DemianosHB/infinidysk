using System.Net;
using NzbWebDAV.Tests.TestUtils;
using NzbWebDAV.Utils;

namespace NzbWebDAV.Tests.Utils;

[Collection(nameof(GlobalOutboundProxyCollection))]
public class OutboundProxyTests
{
    // OutboundProxy is process-wide static state; always restore it so other
    // tests (and the environment-proxy fallback) are unaffected.
    private sealed class ProxyState : IDisposable
    {
        public ProxyState(string? value, string? bypass = null)
        {
            OutboundProxy.Update(value, bypass);
        }

        public void Dispose() => OutboundProxy.Update(null);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("   ")]
    public void TryParseSocks5Url_EmptyOrWhitespace_ReturnsNull(string? raw)
    {
        Assert.Null(OutboundProxy.TryParseSocks5Url(raw));
    }

    [Theory]
    [InlineData("not a url")]
    [InlineData("http://gluetun:1080")]
    [InlineData("https://gluetun:1080")]
    [InlineData("socks5://gluetun")] // no explicit port
    [InlineData("socks5://gluetun:0")]
    [InlineData("socks5://gluetun:70000")]
    [InlineData("socks5://user:pass@gluetun:1080")] // credentials unsupported
    [InlineData("socks5://gluetun:1080?timeout=5")]
    [InlineData("socks5://gluetun:1080#section")]
    public void TryParseSocks5Url_Invalid_ReturnsNull(string raw)
    {
        Assert.Null(OutboundProxy.TryParseSocks5Url(raw));
    }

    [Fact]
    public void TryParseSocks5Url_ValidSocks5_ReturnsUri()
    {
        var uri = OutboundProxy.TryParseSocks5Url("socks5://gluetun:1080");

        Assert.NotNull(uri);
        Assert.Equal("socks5", uri.Scheme);
        Assert.Equal("gluetun", uri.Host);
        Assert.Equal(1080, uri.Port);
    }

    [Fact]
    public void TryParseSocks5Url_Socks5H_NormalizesToSocks5()
    {
        var uri = OutboundProxy.TryParseSocks5Url(" socks5h://gluetun:9040 ");

        Assert.NotNull(uri);
        Assert.Equal("socks5", uri.Scheme);
        Assert.Equal("gluetun", uri.Host);
        Assert.Equal(9040, uri.Port);
    }

    [Fact]
    public void Update_SetsAndClearsConfiguredProxy()
    {
        using (new ProxyState("socks5://gluetun:1080"))
        {
            Assert.True(OutboundProxy.IsConfigured);
            Assert.NotNull(OutboundProxy.Socks5Uri);
            Assert.Equal("gluetun", OutboundProxy.Socks5Uri!.Host);
            Assert.Equal(1080, OutboundProxy.Socks5Uri.Port);
        }

        Assert.False(OutboundProxy.IsConfigured);
        Assert.Null(OutboundProxy.Socks5Uri);
    }

    [Theory]
    [InlineData("http://localhost:8080/api", true)]
    [InlineData("http://127.0.0.1:8080/api", true)]
    [InlineData("http://[::1]:8080/api", true)]
    [InlineData("http://gluetun:8080/api", true)] // single-label Docker name
    [InlineData("http://api.example.com/api", false)]
    [InlineData("http://1.2.3.4:8080/api", false)]
    [InlineData("http://sub.gluetun.internal:8080", false)]
    public void IsLocalDestination_ClassifiesDestinations(string url, bool expectedLocal)
    {
        Assert.Equal(expectedLocal, OutboundProxy.IsLocalDestination(new Uri(url)));
    }

    [Fact]
    public void GetProxy_Configured_ExternalDestination_ReturnsProxyUri()
    {
        using var state = new ProxyState("socks5://gluetun:1080");
        var destination = new Uri("https://api.example.com/v1");

        Assert.Same(OutboundProxy.Socks5Uri, OutboundProxy.WebProxy.GetProxy(destination));
        Assert.Same(OutboundProxy.Socks5Uri, OutboundProxy.StrictWebProxy.GetProxy(destination));
    }

    [Theory]
    [InlineData("http://gluetun:1080")]
    [InlineData("http://localhost:3000")]
    [InlineData("http://127.0.0.1:3000")]
    public void GetProxy_Configured_LocalDestination_StaysDirect(string url)
    {
        using var state = new ProxyState("socks5://gluetun:1080");
        var destination = new Uri(url);

        Assert.Equal(destination, OutboundProxy.WebProxy.GetProxy(destination));
        Assert.Equal(destination, OutboundProxy.StrictWebProxy.GetProxy(destination));
    }

    [Fact]
    public void GetProxy_Unconfigured_StrictProxyConnectsDirectly()
    {
        using var state = new ProxyState(null);
        var destination = new Uri("https://api.example.com/v1");

        Assert.Equal(destination, OutboundProxy.StrictWebProxy.GetProxy(destination));
    }

    [Fact]
    public void GetProxy_IsNeverBypassed()
    {
        using var state = new ProxyState("socks5://gluetun:1080");

        Assert.False(OutboundProxy.WebProxy.IsBypassed(new Uri("https://api.example.com")));
        Assert.False(OutboundProxy.StrictWebProxy.IsBypassed(new Uri("http://localhost:3000")));
    }

    [Fact]
    public void CreateHandler_UsesOutboundProxy()
    {
        using var state = new ProxyState("socks5://gluetun:1080");
        using var handler = OutboundProxy.CreateHandler();

        Assert.True(handler.UseProxy);
        Assert.Same(OutboundProxy.WebProxy, handler.Proxy);
    }

    [Fact]
    public void Update_SetsAndClearsBypass()
    {
        using (new ProxyState("socks5://gluetun:1080", "sonarr.lan, 10.0.0.0/8"))
        {
            Assert.True(OutboundProxy.Bypass.Matches("sonarr.lan"));
            Assert.True(OutboundProxy.Bypass.Matches("10.1.2.3"));
            Assert.False(OutboundProxy.Bypass.Matches("api.example.com"));
        }

        Assert.True(OutboundProxy.Bypass.IsEmpty);
    }

    [Theory]
    [InlineData("http://sonarr.media.lan:8989", true)] // domain suffix .lan
    [InlineData("http://api.internal:8989", true)] // exact host entry
    [InlineData("http://sub.api.internal:8989", true)] // host entry covers subdomains
    [InlineData("http://192.168.1.20:8989", true)] // CIDR 192.168.0.0/16
    [InlineData("http://10.1.2.3:8989", true)] // IP literal entry
    [InlineData("http://gluetun:1080", true)] // single-label stays local
    [InlineData("http://api.example.com", false)]
    [InlineData("http://10.9.9.9:8989", false)]
    [InlineData("http://internal.evil.com", false)] // suffix/host entries must not overmatch
    public void IsBypassed_ConfiguredExceptions_MatchDestinations(string url, bool expected)
    {
        using var state = new ProxyState("socks5://gluetun:1080", "api.internal, .lan, 192.168.0.0/16, 10.1.2.3");
        Assert.Equal(expected, OutboundProxy.IsBypassed(new Uri(url)));
    }

    [Fact]
    public void GetProxy_Configured_ExceptedDestination_StaysDirect()
    {
        using var state = new ProxyState("socks5://gluetun:1080", "*.lan, 192.168.0.0/16");
        var excepted = new Uri("http://sonarr.media.lan:8989");
        var external = new Uri("https://api.example.com/v1");

        Assert.Equal(excepted, OutboundProxy.WebProxy.GetProxy(excepted));
        Assert.Equal(excepted, OutboundProxy.StrictWebProxy.GetProxy(excepted));
        Assert.Same(OutboundProxy.Socks5Uri, OutboundProxy.WebProxy.GetProxy(external));
        Assert.Same(OutboundProxy.Socks5Uri, OutboundProxy.StrictWebProxy.GetProxy(external));
    }

    [Theory]
    [InlineData("news.internal", true)] // exception suffix
    [InlineData("localhost", true)] // loopback name
    [InlineData("127.0.0.1", true)] // loopback address
    [InlineData("::1", true)]
    [InlineData("gluetun", true)] // single-label Docker name
    [InlineData("fe80::1", false)] // link-local is not loopback and not excepted
    [InlineData("192.168.1.5", false)]
    [InlineData("news.example.com", false)]
    public void IsBypassedHost_CombinesLocalRulesAndExceptions(string host, bool expected)
    {
        using var state = new ProxyState("socks5://gluetun:1080", ".internal");
        Assert.Equal(expected, OutboundProxy.IsBypassedHost(host));
    }
}
