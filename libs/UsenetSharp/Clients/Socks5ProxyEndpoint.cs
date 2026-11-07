namespace UsenetSharp.Clients;

/// <summary>
/// A SOCKS5 proxy endpoint that NNTP connections are tunneled through.
/// </summary>
/// <param name="Host">Host name or address of the proxy.</param>
/// <param name="Port">Port of the proxy.</param>
/// <param name="Username">Optional RFC 1929 username.</param>
/// <param name="Password">Optional RFC 1929 password.</param>
public sealed record Socks5ProxyEndpoint(string Host, int Port, string? Username = null, string? Password = null)
{
    /// <summary>True when RFC 1929 username/password authentication should be offered.</summary>
    public bool HasCredentials => !string.IsNullOrEmpty(Username) || !string.IsNullOrEmpty(Password);
}
