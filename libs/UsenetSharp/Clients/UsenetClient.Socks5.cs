using System.Net;
using System.Text;
using UsenetSharp.Exceptions;

namespace UsenetSharp.Clients;

public partial class UsenetClient
{
    private const byte Socks5Version = 5;
    private const byte Socks5AuthNone = 0x00;
    private const byte Socks5AuthUsernamePassword = 0x02;
    private const byte Socks5ConnectCommand = 0x01;
    private const byte Socks5AddressTypeIPv4 = 0x01;
    private const byte Socks5AddressTypeDomain = 0x03;
    private const byte Socks5AddressTypeIPv6 = 0x04;
    private const byte Socks5ReplySucceeded = 0x00;

    /// <summary>
    /// Performs the SOCKS5 CONNECT handshake (RFC 1928, with optional RFC 1929
    /// username/password sub-negotiation) over a stream that is already connected
    /// to the proxy, and leaves the stream positioned at the first byte of the
    /// tunneled NNTP session. Host names are sent as domain addresses so DNS
    /// resolution happens on the proxy (socks5h semantics); TLS remains end-to-end
    /// between the client and the NNTP server because the tunnel is established
    /// before the TLS stream is layered on top.
    /// </summary>
    private static async Task Socks5TunnelAsync(
        Stream stream,
        string host,
        int port,
        Socks5ProxyEndpoint proxy,
        CancellationToken ct)
    {
        // --- Method negotiation (RFC 1928 section 3) ---
        byte[] methods = proxy.HasCredentials
            ? [Socks5AuthNone, Socks5AuthUsernamePassword]
            : [Socks5AuthNone];
        var greeting = new byte[2 + methods.Length];
        greeting[0] = Socks5Version;
        greeting[1] = (byte)methods.Length;
        methods.CopyTo(greeting, 2);
        await stream.WriteAsync(greeting, ct).ConfigureAwait(false);

        var methodReply = new byte[2];
        await Socks5ReadExactlyAsync(stream, methodReply, ct).ConfigureAwait(false);
        if (methodReply[0] != Socks5Version)
            throw new Socks5ProxyException(
                $"The SOCKS5 proxy replied with an unexpected protocol version {methodReply[0]}.");
        var method = methodReply[1];
        if (method == 0xFF)
            throw new Socks5ProxyException(
                "The SOCKS5 proxy rejected all offered authentication methods.");
        if (method != Socks5AuthNone && method != Socks5AuthUsernamePassword)
            throw new Socks5ProxyException(
                $"The SOCKS5 proxy selected unsupported authentication method 0x{method:X2}.");

        // --- Optional username/password sub-negotiation (RFC 1929) ---
        if (method == Socks5AuthUsernamePassword)
        {
            if (!proxy.HasCredentials)
                throw new Socks5ProxyException(
                    "The SOCKS5 proxy requires username/password authentication but no credentials are configured.");
            var user = Encoding.UTF8.GetBytes(proxy.Username ?? string.Empty);
            var password = Encoding.UTF8.GetBytes(proxy.Password ?? string.Empty);
            if (user.Length > 255 || password.Length > 255)
                throw new Socks5ProxyException(
                    "SOCKS5 proxy credentials exceed the RFC 1929 limit of 255 bytes.");
            var authRequest = new byte[3 + user.Length + password.Length];
            authRequest[0] = 0x01; // sub-negotiation version
            authRequest[1] = (byte)user.Length;
            user.CopyTo(authRequest, 2);
            authRequest[2 + user.Length] = (byte)password.Length;
            password.CopyTo(authRequest, 3 + user.Length);
            await stream.WriteAsync(authRequest, ct).ConfigureAwait(false);

            var authReply = new byte[2];
            await Socks5ReadExactlyAsync(stream, authReply, ct).ConfigureAwait(false);
            if (authReply[0] != 0x01)
                throw new Socks5ProxyException(
                    $"The SOCKS5 proxy sent an unexpected auth reply version {authReply[0]}.");
            if (authReply[1] != 0x00)
                throw new Socks5ProxyException(
                    "The SOCKS5 proxy rejected the configured username/password credentials.");
        }

        // --- CONNECT request ---
        byte[] destinationAddress;
        byte addressType;
        if (IPAddress.TryParse(host, out var address))
        {
            destinationAddress = address.GetAddressBytes();
            addressType = destinationAddress.Length == 16 ? Socks5AddressTypeIPv6 : Socks5AddressTypeIPv4;
        }
        else
        {
            destinationAddress = Encoding.ASCII.GetBytes(host);
            if (destinationAddress.Length > 255)
                throw new Socks5ProxyException(
                    $"The destination host name is too long for SOCKS5 (max 255 bytes, got {destinationAddress.Length}).");
            addressType = Socks5AddressTypeDomain;
        }

        var request = new byte[4 + (addressType == Socks5AddressTypeDomain ? 1 : 0)
            + destinationAddress.Length + 2];
        var offset = 0;
        request[offset++] = Socks5Version;
        request[offset++] = Socks5ConnectCommand;
        request[offset++] = 0x00; // reserved
        request[offset++] = addressType;
        if (addressType == Socks5AddressTypeDomain)
            request[offset++] = (byte)destinationAddress.Length;
        destinationAddress.CopyTo(request, offset);
        offset += destinationAddress.Length;
        request[offset++] = (byte)(port >> 8);
        request[offset] = (byte)(port & 0xFF);
        await stream.WriteAsync(request, ct).ConfigureAwait(false);

        // --- CONNECT reply; drain BND.ADDR and BND.PORT so the first NNTP byte
        //     of the tunneled session is not corrupted. ---
        var replyHeader = new byte[4];
        await Socks5ReadExactlyAsync(stream, replyHeader, ct).ConfigureAwait(false);
        if (replyHeader[0] != Socks5Version)
            throw new Socks5ProxyException(
                $"The SOCKS5 proxy replied with an unexpected protocol version {replyHeader[0]}.");
        if (replyHeader[1] != Socks5ReplySucceeded)
            throw new Socks5ProxyException(
                $"The SOCKS5 proxy refused the connection to {host}:{port} (reply code {replyHeader[1]}).");

        var replyAddressLength = replyHeader[3] switch
        {
            Socks5AddressTypeIPv4 => 4,
            Socks5AddressTypeIPv6 => 16,
            Socks5AddressTypeDomain => -1,
            _ => throw new Socks5ProxyException(
                $"The SOCKS5 proxy sent an unknown address type 0x{replyHeader[3]:X2}."),
        };
        if (replyAddressLength == -1)
        {
            var lengthByte = new byte[1];
            await Socks5ReadExactlyAsync(stream, lengthByte, ct).ConfigureAwait(false);
            replyAddressLength = lengthByte[0];
        }
        await Socks5SkipExactlyAsync(stream, replyAddressLength, ct).ConfigureAwait(false);
        await Socks5SkipExactlyAsync(stream, 2, ct).ConfigureAwait(false);
    }

    private static async Task Socks5ReadExactlyAsync(Stream stream, byte[] buffer, CancellationToken ct)
    {
        var offset = 0;
        while (offset < buffer.Length)
        {
            var read = await stream.ReadAsync(buffer.AsMemory(offset), ct).ConfigureAwait(false);
            if (read == 0)
                throw new Socks5ProxyException("The SOCKS5 proxy closed the connection during the handshake.");
            offset += read;
        }
    }

    private static async Task Socks5SkipExactlyAsync(Stream stream, int count, CancellationToken ct)
    {
        if (count <= 0)
            return;
        var buffer = new byte[Math.Min(count, 32)];
        while (count > 0)
        {
            var read = await stream.ReadAsync(buffer.AsMemory(0, Math.Min(count, buffer.Length)), ct).ConfigureAwait(false);
            if (read == 0)
                throw new Socks5ProxyException("The SOCKS5 proxy closed the connection during the handshake.");
            count -= read;
        }
    }
}
