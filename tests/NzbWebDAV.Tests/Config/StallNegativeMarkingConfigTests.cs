using NzbWebDAV.Config;
using NzbWebDAV.Database.Models;

namespace NzbWebDAV.Tests.Config;

public sealed class StallNegativeMarkingConfigTests
{
    [Theory]
    [InlineData("true")]
    [InlineData("false")]
    [InlineData("True")]
    public void StallMarkingEnabled_AcceptsBool(string value)
    {
        ConfigManager.ValidateConfigItems(
        [
            new ConfigItem { ConfigName = ConfigKeys.PlayStallNegativeMarkingEnabled, ConfigValue = value },
        ]);
    }

    [Theory]
    [InlineData("yes")]
    [InlineData("1")]
    public void StallMarkingEnabled_RejectsNonBool(string value)
    {
        Assert.Throws<ArgumentException>(() => ConfigManager.ValidateConfigItems(
        [
            new ConfigItem { ConfigName = ConfigKeys.PlayStallNegativeMarkingEnabled, ConfigValue = value },
        ]));
    }

    [Theory]
    [InlineData("1")]
    [InlineData("15")]
    [InlineData("1440")]
    public void StallMarkingMinutes_AcceptsRange(string value)
    {
        ConfigManager.ValidateConfigItems(
        [
            new ConfigItem { ConfigName = ConfigKeys.PlayStallNegativeMarkingMinutes, ConfigValue = value },
        ]);
    }

    [Theory]
    [InlineData("0")]
    [InlineData("-5")]
    [InlineData("1441")]
    [InlineData("abc")]
    public void StallMarkingMinutes_RejectsOutOfRange(string value)
    {
        Assert.Throws<ArgumentException>(() => ConfigManager.ValidateConfigItems(
        [
            new ConfigItem { ConfigName = ConfigKeys.PlayStallNegativeMarkingMinutes, ConfigValue = value },
        ]));
    }

    [Fact]
    public void Defaults_AreDisabledWith15MinuteTtl()
    {
        var configManager = new ConfigManager();

        Assert.False(configManager.IsPlayStallNegativeMarkingEnabled());
        Assert.Equal(TimeSpan.FromMinutes(15), configManager.GetPlayStallNegativeMarkingTtl());
    }
}
