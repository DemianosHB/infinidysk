using System.Net;
using NzbWebDAV.Utils;

namespace NzbWebDAV.Tests.Utils;

public class OutboundProxyTests
{
    // OutboundProxy is process-wide static state; always restore it so other
    // tests (and the environment-proxy fallback) are unaffected.
    private sealed class ProxyState : IDisposable
    {
        public ProxyState(string? value)
        {
            OutboundProxy.Update(value);
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
}
