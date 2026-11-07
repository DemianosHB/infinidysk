using System.Collections.Concurrent;
using System.Net;
using System.Net.Sockets;
using System.Text;
using UsenetSharp.Clients;
using UsenetSharp.Exceptions;

namespace UsenetSharpTest.Clients;

/// <summary>
/// ConnectAsync tests against a loopback-only fake SOCKS5 server. The fake
/// records protocol violations and the destination it observed so tests can
/// assert on the exact bytes the client put on the wire.
/// </summary>
public class Socks5ConnectTests
{
    private sealed class FakeSocks5Server : IDisposable
    {
        private readonly TcpListener _listener;
        private readonly Func<NetworkStream, FakeSocks5Server, Task> _handler;

        public readonly ConcurrentQueue<string> Violations = new();
        public readonly ConcurrentQueue<string> Observed = new();

        private FakeSocks5Server(Func<NetworkStream, FakeSocks5Server, Task> handler)
        {
            _listener = new TcpListener(IPAddress.Loopback, 0);
            _listener.Start(1);
            Port = ((IPEndPoint)_listener.LocalEndpoint).Port;
            _handler = handler;
        }

        public int Port { get; }

        public Task StartAsync() => Task.Run(async () =>
        {
            using var client = await _listener.AcceptTcpClientAsync();
            _listener.Stop();
            using var stream = client.GetStream();
            await _handler(stream, this);
            // Keep reading until the client closes so no RST races with the
            // tunnel teardown; ignore any trailing bytes (e.g. QUIT).
            var drain = new byte[64];
            while (await stream.ReadAsync(drain) > 0)
            {
            }
        });

        public void Require(bool condition, string violation)
        {
            if (!condition)
                Violations.Enqueue(violation);
        }

        public static async Task<byte[]> ReadExactlyAsync(NetworkStream stream, int count)
        {
            var buffer = new byte[count];
            var offset = 0;
            while (offset < count)
            {
                var read = await stream.ReadAsync(buffer.AsMemory(offset));
                if (read == 0)
                    throw new EndOfStreamException($"fake SOCKS5 server needed {count} bytes, got {offset}");
                offset += read;
            }
            return buffer;
        }

        public static async Task SendReply(NetworkStream stream, byte rep, byte atyp, ReadOnlySpan<byte> boundAddress)
        {
            var reply = new byte[6 + boundAddress.Length];
            reply[0] = 5;
            reply[1] = rep;
            reply[2] = 0;
            reply[3] = atyp;
            boundAddress.CopyTo(reply.AsSpan(4));
            reply[4 + boundAddress.Length] = 0;
            reply[5 + boundAddress.Length] = 0;
            await stream.WriteAsync(reply);
        }

        public static FakeSocks5Server Create(Func<NetworkStream, FakeSocks5Server, Task> handler) =>
            new(handler);

        public void Dispose() => _listener.Stop();
    }

    private static async Task<UsenetClient> CreateConnectedClientAsync(
        FakeSocks5Server server,
        string destinationHost,
        int destinationPort,
        string? proxyUser = null,
        string? proxyPassword = null)
    {
        var client = new UsenetClient(new UsenetClientOptions
        {
            Socks5Proxy = new Socks5ProxyEndpoint(IPAddress.Loopback.ToString(), server.Port, proxyUser, proxyPassword),
        });
        await client.ConnectAsync(destinationHost, destinationPort, useSsl: false, CancellationToken.None);
        return client;
    }

    private static async Task RunAsync(
        Func<NetworkStream, FakeSocks5Server, Task> handler,
        Func<FakeSocks5Server, Task> test)
    {
        using var server = FakeSocks5Server.Create(handler);
        var serverTask = server.StartAsync();
        await test(server);
        var completed = await Task.WhenAny(serverTask, Task.Delay(TimeSpan.FromSeconds(5)));
        Assert.That(completed, Is.SameAs(serverTask), "fake SOCKS5 server did not finish");
        Assert.That(server.Violations, Is.Empty, () => string.Join("; ", server.Violations));
    }

    [Test]
    public async Task ConnectAsync_Socks5Proxy_SendsDomainDestinationAndReadsGreeting()
    {
        await RunAsync(
            async (stream, server) =>
            {
                var greeting = await FakeSocks5Server.ReadExactlyAsync(stream, 3);
                server.Require(greeting[0] == 5, $"expected SOCKS5 version, got {greeting[0]}");
                server.Require(greeting[1] == 1, $"expected 1 offered method, got {greeting[1]}");
                server.Require(greeting[2] == 0x00, "expected no-auth method");
                await stream.WriteAsync((byte[])[(byte)5, (byte)0x00]);

                var header = await FakeSocks5Server.ReadExactlyAsync(stream, 4);
                server.Require(header[0] == 5, "expected request version 5");
                server.Require(header[1] == 0x01, "expected CONNECT command");
                server.Require(header[3] == 0x03, $"expected domain ATYP, got 0x{header[3]:X2}");
                var length = (await FakeSocks5Server.ReadExactlyAsync(stream, 1))[0];
                var host = Encoding.ASCII.GetString(await FakeSocks5Server.ReadExactlyAsync(stream, length));
                var port = await FakeSocks5Server.ReadExactlyAsync(stream, 2);
                server.Require(host == "news.example.test", $"expected host news.example.test, got {host}");
                server.Require(((port[0] << 8) | port[1]) == 119, $"expected port 119, got {(port[0] << 8) | port[1]}");
                server.Observed.Enqueue($"{host}:{(port[0] << 8) | port[1]}");

                // Reply with a domain BND.ADDR so the drain path covers ATYP=3.
                await FakeSocks5Server.SendReply(stream, 0x00, 0x03, Encoding.ASCII.GetBytes("proxy.local"));
                await stream.WriteAsync(Encoding.ASCII.GetBytes("200 fake ready\r\n"));
            },
            async server =>
            {
                using var client = await CreateConnectedClientAsync(server, "news.example.test", 119);
                Assert.That(server.Observed, Does.Contain("news.example.test:119"));
            });
    }

    [Test]
    public async Task ConnectAsync_Socks5Proxy_Ipv4LiteralDestination_UsesIPv4AddressType()
    {
        await RunAsync(
            async (stream, server) =>
            {
                var greeting = await FakeSocks5Server.ReadExactlyAsync(stream, 3);
                server.Require(greeting[2] == 0x00, "expected no-auth method");
                await stream.WriteAsync((byte[])[(byte)5, (byte)0x00]);

                var header = await FakeSocks5Server.ReadExactlyAsync(stream, 4);
                server.Require(header[3] == 0x01, $"expected IPv4 ATYP, got 0x{header[3]:X2}");
                var address = await FakeSocks5Server.ReadExactlyAsync(stream, 4);
                server.Require(address.SequenceEqual(new byte[] { 127, 0, 0, 1 }),
                    $"expected 127.0.0.1, got {string.Join('.', address)}");
                await FakeSocks5Server.ReadExactlyAsync(stream, 2);
                await FakeSocks5Server.SendReply(stream, 0x00, 0x01, new byte[] { 127, 0, 0, 1 });
                await stream.WriteAsync(Encoding.ASCII.GetBytes("200 fake ready\r\n"));
            },
            async server =>
            {
                using var client = await CreateConnectedClientAsync(server, "127.0.0.1", 563);
            });
    }

    [Test]
    public async Task ConnectAsync_Socks5Proxy_MethodRejected_ThrowsSocks5ProxyException()
    {
        await RunAsync(
            async (stream, server) =>
            {
                await FakeSocks5Server.ReadExactlyAsync(stream, 3);
                await stream.WriteAsync((byte[])[(byte)5, (byte)0xFF]);
            },
            async server =>
            {
                var client = new UsenetClient(new UsenetClientOptions
                {
                    Socks5Proxy = new Socks5ProxyEndpoint(IPAddress.Loopback.ToString(), server.Port),
                });
                try
                {
                    var exception = Assert.ThrowsAsync<Socks5ProxyException>(async () =>
                        await client.ConnectAsync("news.example.test", 119, useSsl: false, CancellationToken.None))!;
                    Assert.That(exception.Message, Does.Contain("rejected all offered authentication methods"));
                }
                finally
                {
                    client.Dispose();
                }
            });
    }

    [Test]
    public async Task ConnectAsync_Socks5Proxy_Refused_ThrowsSocks5ProxyExceptionWithReplyCode()
    {
        await RunAsync(
            async (stream, server) =>
            {
                var greeting = await FakeSocks5Server.ReadExactlyAsync(stream, 3);
                server.Require(greeting[2] == 0x00, "expected no-auth method");
                await stream.WriteAsync((byte[])[(byte)5, (byte)0x00]);
                var header = await FakeSocks5Server.ReadExactlyAsync(stream, 4);
                if (header[3] == 0x03)
                {
                    var length = (await FakeSocks5Server.ReadExactlyAsync(stream, 1))[0];
                    await FakeSocks5Server.ReadExactlyAsync(stream, length + 2);
                }
                else
                {
                    await FakeSocks5Server.ReadExactlyAsync(stream, header[3] == 0x01 ? 6 : 18);
                }
                await FakeSocks5Server.SendReply(stream, 0x05, 0x01, new byte[] { 0, 0, 0, 0 });
            },
            async server =>
            {
                var client = new UsenetClient(new UsenetClientOptions
                {
                    Socks5Proxy = new Socks5ProxyEndpoint(IPAddress.Loopback.ToString(), server.Port),
                });
                try
                {
                    var exception = Assert.ThrowsAsync<Socks5ProxyException>(async () =>
                        await client.ConnectAsync("news.example.test", 119, useSsl: false, CancellationToken.None))!;
                    Assert.That(exception.Message, Does.Contain("refused the connection"));
                    Assert.That(exception.Message, Does.Contain("reply code 5"));
                }
                finally
                {
                    client.Dispose();
                }
            });
    }

    [Test]
    public async Task ConnectAsync_Socks5Proxy_Credentials_AuthenticatesAndConnects()
    {
        await RunAsync(
            async (stream, server) =>
            {
                var greeting = await FakeSocks5Server.ReadExactlyAsync(stream, 4);
                server.Require(greeting[1] == 2, "expected two offered methods");
                server.Require(greeting[2] == 0x00 && greeting[3] == 0x02, "expected [no-auth, user/pass] methods");
                await stream.WriteAsync((byte[])[(byte)5, (byte)0x02]);

                var version = (await FakeSocks5Server.ReadExactlyAsync(stream, 1))[0];
                server.Require(version == 0x01, $"expected RFC 1929 version 1, got {version}");
                var userLength = (await FakeSocks5Server.ReadExactlyAsync(stream, 1))[0];
                var user = Encoding.UTF8.GetString(await FakeSocks5Server.ReadExactlyAsync(stream, userLength));
                var passwordLength = (await FakeSocks5Server.ReadExactlyAsync(stream, 1))[0];
                var password = Encoding.UTF8.GetString(await FakeSocks5Server.ReadExactlyAsync(stream, passwordLength));
                server.Require(user == "socksuser" && password == "sockspass",
                    $"expected socksuser/sockspass, got {user}/{password}");
                await stream.WriteAsync((byte[])[(byte)0x01, (byte)0x00]);

                var header = await FakeSocks5Server.ReadExactlyAsync(stream, 4);
                server.Require(header[1] == 0x01, "expected CONNECT command");
                if (header[3] == 0x03)
                {
                    var length = (await FakeSocks5Server.ReadExactlyAsync(stream, 1))[0];
                    await FakeSocks5Server.ReadExactlyAsync(stream, length + 2);
                }
                else
                {
                    await FakeSocks5Server.ReadExactlyAsync(stream, header[3] == 0x01 ? 6 : 18);
                }
                // IPv6 BND.ADDR so the drain path covers ATYP=4.
                await FakeSocks5Server.SendReply(stream, 0x00, 0x04, new byte[16]);
                await stream.WriteAsync(Encoding.ASCII.GetBytes("200 fake ready\r\n"));
            },
            async server =>
            {
                using var client = await CreateConnectedClientAsync(
                    server, "news.example.test", 563, proxyUser: "socksuser", proxyPassword: "sockspass");
            });
    }

    [Test]
    public async Task ConnectAsync_Socks5Proxy_RejectedCredentials_ThrowsSocks5ProxyException()
    {
        await RunAsync(
            async (stream, server) =>
            {
                await FakeSocks5Server.ReadExactlyAsync(stream, 4);
                await stream.WriteAsync((byte[])[(byte)5, (byte)0x02]);
                await FakeSocks5Server.ReadExactlyAsync(stream, 1); // version
                var userLength = (await FakeSocks5Server.ReadExactlyAsync(stream, 1))[0];
                await FakeSocks5Server.ReadExactlyAsync(stream, userLength);
                var passwordLength = (await FakeSocks5Server.ReadExactlyAsync(stream, 1))[0];
                await FakeSocks5Server.ReadExactlyAsync(stream, passwordLength);
                await stream.WriteAsync((byte[])[(byte)0x01, (byte)0x01]);
            },
            async server =>
            {
                var client = new UsenetClient(new UsenetClientOptions
                {
                    Socks5Proxy = new Socks5ProxyEndpoint(
                        IPAddress.Loopback.ToString(), server.Port, "socksuser", "wrong"),
                });
                try
                {
                    var exception = Assert.ThrowsAsync<Socks5ProxyException>(async () =>
                        await client.ConnectAsync("news.example.test", 563, useSsl: false, CancellationToken.None))!;
                    Assert.That(exception.Message, Does.Contain("rejected the configured username/password credentials"));
                }
                finally
                {
                    client.Dispose();
                }
            });
    }
}
