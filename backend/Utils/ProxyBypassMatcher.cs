using System.Net;
using Serilog;

namespace NzbWebDAV.Utils;

/// <summary>
/// Parses the <c>general.socks5-proxy-bypass</c> list: comma/whitespace-separated
/// destinations that connect directly even while the process-wide outbound SOCKS5
/// proxy is configured. Supported entries: <c>*</c> (everything), an exact host
/// name (<c>sonarr.lan</c>, which also covers its subdomains, NO_PROXY-style), a
/// domain suffix (<c>.lan</c> or <c>*.lan</c>, subdomains only), an IP literal, or
/// a CIDR range. IP entries match IP-literal destinations only; names are never
/// resolved here.
/// </summary>
public sealed class ProxyBypassMatcher
{
    public static readonly ProxyBypassMatcher Empty = new(
        matchAll: false,
        hosts: new HashSet<string>(StringComparer.OrdinalIgnoreCase),
        suffixes: new HashSet<string>(StringComparer.OrdinalIgnoreCase),
        addresses: [],
        networks: []);

    private readonly bool _matchAll;
    private readonly HashSet<string> _hosts;
    private readonly HashSet<string> _suffixes;
    private readonly HashSet<IPAddress> _addresses;
    private readonly List<IPNetwork> _networks;

    private ProxyBypassMatcher(
        bool matchAll,
        HashSet<string> hosts,
        HashSet<string> suffixes,
        HashSet<IPAddress> addresses,
        List<IPNetwork> networks)
    {
        _matchAll = matchAll;
        _hosts = hosts;
        _suffixes = suffixes;
        _addresses = addresses;
        _networks = networks;
    }

    public bool IsEmpty =>
        !_matchAll && _hosts.Count == 0 && _suffixes.Count == 0 && _addresses.Count == 0 && _networks.Count == 0;

    /// <summary>Splits a raw list on commas and whitespace; empty entries are dropped.</summary>
    public static string[] SplitEntries(string? rawEntries) =>
        string.IsNullOrWhiteSpace(rawEntries)
            ? []
            : rawEntries.Split(
                [',', ' ', '\t', '\r', '\n'],
                StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);

    /// <summary>True when <paramref name="entry"/> is a single valid exception.</summary>
    public static bool IsValidEntry(string entry) => TryParseEntry(entry, out _);

    /// <summary>
    /// Parses the raw list, skipping invalid entries with a warning (mirrors
    /// <see cref="TrustedHostsMatcher"/>). Empty or whitespace input yields
    /// <see cref="Empty"/>.
    /// </summary>
    public static ProxyBypassMatcher Parse(string? rawEntries)
    {
        var entries = SplitEntries(rawEntries);
        if (entries.Length == 0)
            return Empty;

        var matchAll = false;
        var hosts = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var suffixes = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var addresses = new HashSet<IPAddress>();
        var networks = new List<IPNetwork>();

        foreach (var entry in entries)
        {
            if (!TryParseEntry(entry, out var parsed) || parsed is null)
            {
                Log.Warning("Ignoring invalid proxy-bypass entry: {Entry}", entry);
                continue;
            }
            switch (parsed)
            {
                case Entry.All:
                    matchAll = true;
                    break;
                case Entry.Host host:
                    hosts.Add(host.Name);
                    break;
                case Entry.Suffix suffix:
                    suffixes.Add(suffix.Value);
                    break;
                case Entry.Address address:
                    addresses.Add(address.Value);
                    break;
                case Entry.Network network:
                    networks.Add(network.Value);
                    break;
            }
        }

        if (matchAll || hosts.Count > 0 || suffixes.Count > 0 || addresses.Count > 0 || networks.Count > 0)
            return new ProxyBypassMatcher(matchAll, hosts, suffixes, addresses, networks);
        return Empty;
    }

    /// <summary>
    /// True when <paramref name="host"/> (a DNS name or IP literal) is an excepted
    /// destination. Matching is case-insensitive; IP literals also match CIDR entries.
    /// </summary>
    public bool Matches(string host)
    {
        if (string.IsNullOrWhiteSpace(host))
            return false;
        if (_matchAll)
            return true;

        if (IPAddress.TryParse(host, out var address))
        {
            address = Normalize(address);
            if (_addresses.Contains(address))
                return true;
            foreach (var network in _networks)
            {
                if (network.Contains(address))
                    return true;
            }
            return false;
        }

        if (_hosts.Contains(host))
            return true;
        foreach (var suffix in _suffixes)
        {
            if (host.EndsWith(suffix, StringComparison.OrdinalIgnoreCase))
                return true;
        }
        // A plain host entry also covers its subdomains (NO_PROXY semantics).
        foreach (var name in _hosts)
        {
            if (host.EndsWith("." + name, StringComparison.OrdinalIgnoreCase))
                return true;
        }
        return false;
    }

    internal static bool TryParseEntry(string entry, out Entry? parsed)
    {
        parsed = null;
        if (entry == "*")
        {
            parsed = new Entry.All();
            return true;
        }
        if (entry.Contains('/', StringComparison.Ordinal))
        {
            if (IPNetwork.TryParse(entry, out var network))
            {
                parsed = new Entry.Network(network);
                return true;
            }
            return false;
        }
        if (IPAddress.TryParse(entry, out var address))
        {
            parsed = new Entry.Address(Normalize(address));
            return true;
        }

        var suffixOnly = false;
        var name = entry;
        if (name.StartsWith("*.", StringComparison.Ordinal))
        {
            name = name[1..]; // "*.example.com" -> ".example.com"
            suffixOnly = true;
        }
        if (name.StartsWith('.'))
        {
            suffixOnly = true;
            name = name[1..]; // strip the leading dot; suffix entries re-add it below
        }
        if (name.EndsWith('.'))
            name = name[..^1]; // tolerate the FQDN trailing dot
        if (!IsValidHostName(name))
            return false;

        parsed = suffixOnly ? new Entry.Suffix("." + name) : new Entry.Host(name);
        return true;
    }

    private static bool IsValidHostName(string name)
    {
        if (name.Length is 0 or > 253)
            return false;
        foreach (var label in name.Split('.'))
        {
            if (label.Length == 0)
                return false;
            if (label.Any(c => !char.IsLetterOrDigit(c) && c is not '-' and not '_'))
                return false;
        }
        return true;
    }

    private static IPAddress Normalize(IPAddress address) =>
        address.IsIPv4MappedToIPv6 ? address.MapToIPv4() : address;

    internal abstract record Entry
    {
        public sealed record All : Entry;
        public sealed record Host(string Name) : Entry;
        public sealed record Suffix(string Value) : Entry;
        public sealed record Address(IPAddress Value) : Entry;
        public sealed record Network(IPNetwork Value) : Entry;
    }
}
