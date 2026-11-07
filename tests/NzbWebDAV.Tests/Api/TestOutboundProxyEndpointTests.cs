using System.Net;
using System.Net.Sockets;
using NzbWebDAV.Api.Controllers.TestOutboundProxy;
using NzbWebDAV.Api.Errors;

namespace NzbWebDAV.Tests.Api;

/// <summary>
/// Tests for the outbound proxy test endpoint helpers. The fake proxy answers
/// the method negotiation with configurable bytes so the probe's verdicts can
/// be asserted without a real SOCKS5 server.
/// </summary>
public class TestOutboundProxyEndpointTests
{
    [Fact]
    public void ValidateProxyUrl_ValidSocks5Url_ReturnsParsedUri()
    {
        var uri = TestOutboundProxyController.ValidateProxyUrl("socks5://gluetun:1080");

        Assert.Equal("gluetun", uri.Host);
        Assert.Equal(1080, uri.Port);
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("not a url")]
    [InlineData("http://gluetun:1080")]
    [InlineData("socks5://gluetun")]
    [InlineData("socks5://user:pass@gluetun:1080")]
    public void ValidateProxyUrl_InvalidUrl_ThrowsValidationError(string? rawProxyUrl)
    {
        var exception = Assert.Throws<ApiValidationException>(
            () => TestOutboundProxyController.ValidateProxyUrl(rawProxyUrl));

        Assert.True(exception.Errors.ContainsKey("proxy-url"));
    }

    [Fact]
    public async Task TestProxyAsync_ReachableProxy_ReportsConnected()
    {
        using var server = FakeSocks5Server.Start([(byte)5, (byte)0x00]);

        var response = await TestOutboundProxyController.TestProxyAsync(
            LoopbackProxyUri(server.Port), CancellationToken.None);

        Assert.True(response.Connected);
        Assert.Null(response.Reason);
        await server.WaitForHandshakeAsync();
    }

    [Fact]
    public async Task TestProxyAsync_NonSocks5Reply_ReportsFailure()
    {
        using var server = FakeSocks5Server.Start([(byte)4, (byte)0x00]);

        var response = await TestOutboundProxyController.TestProxyAsync(
            LoopbackProxyUri(server.Port), CancellationToken.None);

        Assert.False(response.Connected);
        Assert.Contains("protocol version", response.Reason);
    }

    [Fact]
    public async Task TestProxyAsync_NoAcceptableMethods_ReportsFailure()
    {
        using var server = FakeSocks5Server.Start([(byte)5, (byte)0xFF]);

        var response = await TestOutboundProxyController.TestProxyAsync(
            LoopbackProxyUri(server.Port), CancellationToken.None);

        Assert.False(response.Connected);
        Assert.Contains("rejected all offered authentication methods", response.Reason);
    }

    [Fact]
    public async Task TestProxyAsync_UnsupportedMethod_ReportsFailure()
    {
        using var server = FakeSocks5Server.Start([(byte)5, (byte)0x02]);

        var response = await TestOutboundProxyController.TestProxyAsync(
            LoopbackProxyUri(server.Port), CancellationToken.None);

        Assert.False(response.Connected);
        Assert.Contains("unsupported authentication method", response.Reason);
    }

    [Fact]
    public async Task TestProxyAsync_ConnectionRefused_ReportsFailure()
    {
        var port = FakeSocks5Server.GetUnusedPort();

        var response = await TestOutboundProxyController.TestProxyAsync(
            LoopbackProxyUri(port), CancellationToken.None);

        Assert.False(response.Connected);
        Assert.Contains("Could not connect", response.Reason);
    }

    private static Uri LoopbackProxyUri(int port) => new($"socks5://{IPAddress.Loopback}:{port}");

    /// <summary>
    /// Loopback-only fake SOCKS5 server: reads the client's method negotiation
    /// greeting and replies with fixed bytes, then drains until the client
    /// closes the connection.
    /// </summary>
    private sealed class FakeSocks5Server : IDisposable
    {
        private readonly TcpListener _listener;
        private readonly Task _serverTask;

        private FakeSocks5Server(TcpListener listener, Task serverTask)
        {
            _listener = listener;
            _serverTask = serverTask;
        }

        public int Port => ((IPEndPoint)_listener.LocalEndpoint).Port;

        public static FakeSocks5Server Start(byte[] reply)
        {
            var listener = new TcpListener(IPAddress.Loopback, 0);
            listener.Start(1);
            var serverTask = Task.Run(async () =>
            {
                using var client = await listener.AcceptTcpClientAsync();
                using var stream = client.GetStream();
                var greeting = new byte[3];
                var offset = 0;
                while (offset < greeting.Length)
                {
                    var read = await stream.ReadAsync(greeting.AsMemory(offset));
                    if (read == 0)
                        return;
                    offset += read;
                }
                await stream.WriteAsync(reply);
                var drain = new byte[64];
                while (await stream.ReadAsync(drain) > 0)
                {
                }
            });
            return new FakeSocks5Server(listener, serverTask);
        }

        public static int GetUnusedPort()
        {
            var listener = new TcpListener(IPAddress.Loopback, 0);
            listener.Start(1);
            var port = ((IPEndPoint)listener.LocalEndpoint).Port;
            listener.Stop();
            return port;
        }

        /// <summary>Waits briefly for the fake server to finish its handshake exchange.</summary>
        public async Task WaitForHandshakeAsync() =>
            await Task.WhenAny(_serverTask, Task.Delay(TimeSpan.FromSeconds(5)));

        public void Dispose() => _listener.Stop();
    }
}
