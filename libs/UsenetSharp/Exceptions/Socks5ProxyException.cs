namespace UsenetSharp.Exceptions;

/// <summary>
/// Thrown when the SOCKS5 tunnel to the NNTP destination cannot be established,
/// for example because the proxy rejects the handshake, authentication, or the
/// CONNECT request.
/// </summary>
public sealed class Socks5ProxyException(string message) : UsenetException(message);
