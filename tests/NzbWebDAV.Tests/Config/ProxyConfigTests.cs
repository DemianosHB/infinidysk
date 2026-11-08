using NzbWebDAV.Config;
using NzbWebDAV.Database.Models;

namespace NzbWebDAV.Tests.Config;

public sealed class ProxyConfigTests
{
    [Theory]
    [InlineData("true")]
    [InlineData("false")]
    public void ValidBoolean_IsAccepted(string configured)
    {
        ConfigManager.ValidateConfigItems([
            new ConfigItem
            {
                ConfigName = ConfigKeys.GeneralTrustProxy,
                ConfigValue = configured,
            },
        ]);
    }

    [Fact]
    public void InvalidValue_IsRejected()
    {
        Assert.Throws<ArgumentException>(() => ConfigManager.ValidateConfigItems([
            new ConfigItem
            {
                ConfigName = ConfigKeys.GeneralTrustProxy,
                ConfigValue = "sometimes",
            },
        ]));
    }

    [Theory]
    [InlineData("https://nzbdav.example.com")]
    [InlineData("http://localhost:3000/app")]
    public void ValidBaseUrl_IsAccepted(string configured)
    {
        ConfigManager.ValidateConfigItems([
            new ConfigItem
            {
                ConfigName = ConfigKeys.GeneralBaseUrl,
                ConfigValue = configured,
            },
        ]);
    }

    [Theory]
    [InlineData("not-a-url")]
    [InlineData("ftp://nzbdav.example.com")]
    [InlineData("https://user@nzbdav.example.com")]
    [InlineData("https://nzbdav.example.com?query=true")]
    [InlineData("https://nzbdav.example.com#fragment")]
    public void InvalidBaseUrl_IsRejected(string configured)
    {
        Assert.Throws<ArgumentException>(() => ConfigManager.ValidateConfigItems([
            new ConfigItem
            {
                ConfigName = ConfigKeys.GeneralBaseUrl,
                ConfigValue = configured,
            },
        ]));
    }

    [Theory]
    [InlineData("")]
    [InlineData("sonarr.lan")]
    [InlineData("sonarr.lan, .internal, *.corp.example, 192.168.1.50, 192.168.0.0/16, fd00::/8")]
    [InlineData("*")]
    [InlineData("nas.lan\n10.0.0.0/8")] // newline-separated also works
    public void ValidProxyBypass_IsAccepted(string configured)
    {
        ConfigManager.ValidateConfigItems([
            new ConfigItem
            {
                ConfigName = ConfigKeys.GeneralSocks5ProxyBypass,
                ConfigValue = configured,
            },
        ]);
    }

    [Theory]
    [InlineData("192.168.0.0/33")] // broken CIDR
    [InlineData("http://sonarr.lan")] // scheme is not a host entry
    [InlineData("sonarr:8989")] // port is not part of an entry
    [InlineData("*foo")] // wildcard only as "*" or "*."
    public void InvalidProxyBypass_IsRejected(string configured)
    {
        Assert.Throws<ArgumentException>(() => ConfigManager.ValidateConfigItems([
            new ConfigItem
            {
                ConfigName = ConfigKeys.GeneralSocks5ProxyBypass,
                ConfigValue = configured,
            },
        ]));
    }
}