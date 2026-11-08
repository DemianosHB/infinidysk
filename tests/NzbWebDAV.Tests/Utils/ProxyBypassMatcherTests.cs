using NzbWebDAV.Utils;

namespace NzbWebDAV.Tests.Utils;

public class ProxyBypassMatcherTests
{
    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("   ,  \t\n")]
    public void Parse_EmptyInput_IsEmpty(string? raw)
    {
        Assert.True(ProxyBypassMatcher.Parse(raw).IsEmpty);
    }

    [Theory]
    [InlineData("*")]
    [InlineData("sonarr.lan")]
    [InlineData(".internal")]
    [InlineData("*.corp.example")]
    [InlineData("192.168.1.50")]
    [InlineData("192.168.0.0/16")]
    [InlineData("fd00::/8")]
    [InlineData("SONARR.LAN.")] // FQDN trailing dot tolerated, case-insensitive
    public void IsValidEntry_ValidEntries_Accepted(string entry)
    {
        Assert.True(ProxyBypassMatcher.IsValidEntry(entry), $"expected valid: {entry}");
    }

    [Theory]
    [InlineData("")]
    [InlineData("not a host!")] // whitespace splits into words, "host!" is invalid
    [InlineData("192.168.0.0/33")] // broken CIDR
    [InlineData("10.0.0.0/abc")]
    [InlineData("http://sonarr.lan")] // scheme is not a host entry
    [InlineData("*foo")] // wildcard only as "*" or "*."
    [InlineData("sonarr..lan")] // empty label
    [InlineData(".")]
    [InlineData("sonarr:8989")] // port is not part of a host entry
    public void IsValidEntry_InvalidEntries_Rejected(string entry)
    {
        Assert.False(ProxyBypassMatcher.IsValidEntry(entry), $"expected invalid: {entry}");
    }

    [Fact]
    public void SplitEntries_SplitsOnCommasAndWhitespace()
    {
        Assert.Equal(
            ["prowlarr", "hydra.lan", "192.168.1.0/24", "10.0.0.5"],
            ProxyBypassMatcher.SplitEntries("prowlarr, hydra.lan\n192.168.1.0/24\t10.0.0.5"));
    }

    [Theory]
    [InlineData("sonarr.lan", true)]
    [InlineData("Sonarr.LAN", true)] // case-insensitive
    [InlineData("sub.sonarr.lan", true)] // plain host entry covers subdomains
    [InlineData("notsonarr.lan", false)]
    [InlineData("sonarr.lan.evil.com", false)]
    public void Matches_ExactHostEntry(string host, bool expected)
    {
        var matcher = ProxyBypassMatcher.Parse("sonarr.lan");
        Assert.Equal(expected, matcher.Matches(host));
    }

    [Theory]
    [InlineData("nas.internal", true)]
    [InlineData("deep.nas.internal", true)]
    [InlineData("internal", false)] // suffix entries match subdomains only
    [InlineData("notinternal", false)]
    [InlineData("internal.evil.com", false)]
    public void Matches_DomainSuffixEntry(string host, bool expected)
    {
        var dot = ProxyBypassMatcher.Parse(".internal");
        var star = ProxyBypassMatcher.Parse("*.internal");
        Assert.Equal(expected, dot.Matches(host));
        Assert.Equal(expected, star.Matches(host));
    }

    [Theory]
    [InlineData("192.168.1.50", true)]
    [InlineData("192.168.1.51", false)]
    [InlineData("::ffff:192.168.1.50", true)] // IPv4-mapped IPv6 normalized
    public void Matches_IpLiteralEntry(string host, bool expected)
    {
        var matcher = ProxyBypassMatcher.Parse("192.168.1.50");
        Assert.Equal(expected, matcher.Matches(host));
    }

    [Theory]
    [InlineData("192.168.1.5", true)]
    [InlineData("192.168.255.254", true)]
    [InlineData("192.167.1.5", false)]
    [InlineData("10.0.0.1", false)]
    [InlineData("fd00::1", false)] // v4 CIDR does not match v6
    public void Matches_CidrEntry(string host, bool expected)
    {
        var matcher = ProxyBypassMatcher.Parse("192.168.0.0/16");
        Assert.Equal(expected, matcher.Matches(host));
    }

    [Fact]
    public void Matches_Ipv6CidrEntry()
    {
        var matcher = ProxyBypassMatcher.Parse("fd00::/8");
        Assert.True(matcher.Matches("fd12:3456::1"));
        Assert.False(matcher.Matches("fe80::1"));
    }

    [Fact]
    public void Matches_Star_MatchesEverything()
    {
        var matcher = ProxyBypassMatcher.Parse("*");
        Assert.True(matcher.Matches("anything.example.com"));
        Assert.True(matcher.Matches("1.2.3.4"));
    }

    [Theory]
    [InlineData("")]
    [InlineData("   ")]
    public void Matches_EmptyHost_NeverMatches(string host)
    {
        Assert.False(ProxyBypassMatcher.Parse("*").Matches(host));
    }

    [Fact]
    public void Parse_InvalidEntriesAreSkipped_NotFatal()
    {
        var matcher = ProxyBypassMatcher.Parse("sonarr.lan, 192.168.0.0/99, broken/entry");
        Assert.False(matcher.IsEmpty);
        Assert.True(matcher.Matches("sonarr.lan"));
        Assert.False(matcher.Matches("192.168.1.5"));
    }

    [Fact]
    public void Parse_OnlyInvalidEntries_IsEmpty()
    {
        Assert.True(ProxyBypassMatcher.Parse("broken/entry, 192.168.0.0/99").IsEmpty);
    }
}
