using NzbWebDAV.Config;
using NzbWebDAV.Database.Models;

namespace NzbWebDAV.Tests.Config;

public sealed class ReadAheadRepairConfigTests
{
    [Theory]
    [InlineData("true")]
    [InlineData("false")]
    [InlineData("True")]
    public void ReadAheadEnabled_AcceptsBool(string value)
    {
        ConfigManager.ValidateConfigItems(
        [
            new ConfigItem { ConfigName = ConfigKeys.RepairReadAheadEnabled, ConfigValue = value },
        ]);
    }

    [Theory]
    [InlineData("yes")]
    [InlineData("1")]
    public void ReadAheadEnabled_RejectsNonBool(string value)
    {
        Assert.Throws<ArgumentException>(() => ConfigManager.ValidateConfigItems(
        [
            new ConfigItem { ConfigName = ConfigKeys.RepairReadAheadEnabled, ConfigValue = value },
        ]));
    }

    [Theory]
    [InlineData("5")]
    [InlineData("45")]
    [InlineData("600")]
    public void ReadAheadWindowSeconds_AcceptsRange(string value)
    {
        ConfigManager.ValidateConfigItems(
        [
            new ConfigItem { ConfigName = ConfigKeys.RepairReadAheadWindowSeconds, ConfigValue = value },
        ]);
    }

    [Theory]
    [InlineData("0")]
    [InlineData("4")]
    [InlineData("601")]
    [InlineData("-5")]
    [InlineData("abc")]
    public void ReadAheadWindowSeconds_RejectsOutOfRange(string value)
    {
        Assert.Throws<ArgumentException>(() => ConfigManager.ValidateConfigItems(
        [
            new ConfigItem { ConfigName = ConfigKeys.RepairReadAheadWindowSeconds, ConfigValue = value },
        ]));
    }

    [Fact]
    public void Defaults_AreDisabledWith45SecondWindow()
    {
        var configManager = new ConfigManager();
        Assert.False(configManager.IsReadAheadRepairEnabled());
        Assert.Equal(45, configManager.GetReadAheadRepairWindowSeconds());
    }

    [Fact]
    public void Enabled_RequiresPar2GapRepair_AndWindowIsClamped()
    {
        var configManager = new ConfigManager();
        configManager.UpdateValues(
        [
            Item(ConfigKeys.RepairPar2Enabled, "false"),
            Item(ConfigKeys.RepairReadAheadEnabled, "true"),
        ]);
        Assert.False(configManager.IsReadAheadRepairEnabled());

        configManager.UpdateValues(
        [
            Item(ConfigKeys.RepairPar2Enabled, "true"),
            Item(ConfigKeys.RepairReadAheadEnabled, "true"),
            Item(ConfigKeys.RepairReadAheadWindowSeconds, "999"),
        ]);
        Assert.True(configManager.IsReadAheadRepairEnabled());
        Assert.Equal(600, configManager.GetReadAheadRepairWindowSeconds());

        configManager.UpdateValues(
        [
            Item(ConfigKeys.RepairReadAheadEnabled, "false"),
            Item(ConfigKeys.RepairReadAheadWindowSeconds, "1"),
        ]);
        Assert.False(configManager.IsReadAheadRepairEnabled());
        Assert.Equal(5, configManager.GetReadAheadRepairWindowSeconds());
    }

    private static ConfigItem Item(string name, string value) =>
        new() { ConfigName = name, ConfigValue = value };
}
