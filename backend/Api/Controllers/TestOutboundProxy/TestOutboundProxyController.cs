using System.Net;
using System.Net.Sockets;
using Microsoft.AspNetCore.Mvc;
using NzbWebDAV.Api.Errors;
using NzbWebDAV.Utils;

namespace NzbWebDAV.Api.Controllers.TestOutboundProxy;

/// <summary>
/// Verifies a submitted SOCKS5 proxy URL. The probe performs the RFC 1928
/// method negotiation (offering only the no-auth method) but never sends a
/// CONNECT request, so verifying has no side effects on the proxy.
/// </summary>
[ApiController]
[Route("api/test-outbound-proxy")]
public class TestOutboundProxyController : BaseApiController
{
    public const int ProbeTimeoutSeconds = 5;

    private const byte Socks5Version = 0x05;
    private const byte Socks5AuthNone = 0x00;
    private const byte Socks5NoAcceptableMethods = 0xFF;

    protected override async Task<IActionResult> HandleRequest()
    {
        var rawProxyUrl = HttpContext.Request.Form["proxy-url"].FirstOrDefault();
        var proxyUri = ValidateProxyUrl(rawProxyUrl);
        var response = await TestProxyAsync(proxyUri, HttpContext.RequestAborted).ConfigureAwait(false);
        return Ok(response);
    }

    /// <summary>
    /// Validates the submitted proxy URL with the same rules the
    /// <c>general.socks5-proxy</c> setting enforces.
    /// </summary>
    public static Uri ValidateProxyUrl(string? rawProxyUrl)
    {
        var uri = OutboundProxy.TryParseSocks5Url(rawProxyUrl);
        var errors = new ValidationErrors();
        if (uri is null)
            errors.Add("proxy-url", "A valid SOCKS5 proxy URL like socks5://gluetun:1080 is required");
        errors.ThrowIfAny();
        return uri!;
    }

    /// <summary>
    /// Connects to the proxy and performs the SOCKS5 method negotiation without
    /// opening a tunneled connection. Returns a response describing whether the
    /// proxy is usable by the app.
    /// </summary>
    public static async Task<TestOutboundProxyResponse> TestProxyAsync(
        Uri proxyUri,
        CancellationToken cancellationToken)
    {
        using var timeoutCts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeoutCts.CancelAfter(TimeSpan.FromSeconds(ProbeTimeoutSeconds));
        try
        {
            using var tcpClient = new TcpClient { NoDelay = true };
            await tcpClient.ConnectAsync(proxyUri.Host, proxyUri.Port, timeoutCts.Token)
                .ConfigureAwait(false);
            using var stream = tcpClient.GetStream();

            // Offer only the no-auth method: the general.socks5-proxy setting does
            // not carry credentials, so a proxy that demands authentication
            // cannot be used and must be reported as such.
            var greeting = new byte[] { Socks5Version, 0x01, Socks5AuthNone };
            await stream.WriteAsync(greeting, timeoutCts.Token).ConfigureAwait(false);

            var methodReply = new byte[2];
            await ReadExactlyAsync(stream, methodReply, timeoutCts.Token).ConfigureAwait(false);
            return EvaluateMethodReply(methodReply);
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            return Failure($"The connection attempt timed out after {ProbeTimeoutSeconds} seconds.");
        }
        catch (SocketException e)
        {
            return Failure($"Could not connect to {proxyUri.Host}:{proxyUri.Port}: {e.SocketErrorCode}.");
        }
        catch (IOException)
        {
            return Failure("The proxy closed the connection during the handshake.");
        }
    }

    private static TestOutboundProxyResponse EvaluateMethodReply(byte[] methodReply)
    {
        if (methodReply[0] != Socks5Version)
            return Failure(
                $"The server replied with protocol version {methodReply[0]}; it may not be a SOCKS5 proxy.");
        if (methodReply[1] == Socks5NoAcceptableMethods)
            return Failure(
                "The proxy rejected all offered authentication methods; it may require username/password authentication.");
        if (methodReply[1] != Socks5AuthNone)
            return Failure($"The proxy selected unsupported authentication method 0x{methodReply[1]:X2}.");
        return new TestOutboundProxyResponse { Status = true, Connected = true };
    }

    private static TestOutboundProxyResponse Failure(string reason) =>
        new() { Status = true, Connected = false, Reason = reason };

    private static async Task ReadExactlyAsync(Stream stream, byte[] buffer, CancellationToken cancellationToken)
    {
        var offset = 0;
        while (offset < buffer.Length)
        {
            var read = await stream.ReadAsync(buffer.AsMemory(offset), cancellationToken)
                .ConfigureAwait(false);
            if (read == 0)
                throw new EndOfStreamException();
            offset += read;
        }
    }
}
