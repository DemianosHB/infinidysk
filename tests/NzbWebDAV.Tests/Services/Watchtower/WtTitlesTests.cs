using NzbWebDAV.Services;

namespace NzbWebDAV.Tests.Services.Watchtower;

public class WtTitlesTests
{
    [Theory]
    [InlineData("tt1234567", true)]
    [InlineData("tt1234567:2", true)]
    [InlineData("tt1234567:2:1", true)]
    [InlineData("1234567", true)]
    [InlineData("kitsu:123", false)]
    [InlineData("kitsu:123:4", false)]
    [InlineData("", false)]
    public void IsImdbId_DetectsImdbShapedIds(string contentId, bool expected)
        => Assert.Equal(expected, WtTitles.IsImdbId(contentId));

    [Theory]
    [InlineData("tt1234567", "tt1234567")]
    [InlineData("1234567", "tt1234567")]
    [InlineData("tt1234567:2:1", "tt1234567")]
    public void CanonicalImdb_StripsEpisodeSuffixAndAddsPrefix(string contentId, string expected)
        => Assert.Equal(expected, WtTitles.CanonicalImdb(contentId));

    [Theory]
    [InlineData(null, "tt1234567:2:1", true)]
    [InlineData("", "tt1234567:2:1", true)]
    [InlineData("tt1234567:2:1", "tt1234567:2:1", true)]
    [InlineData("tt1234567 S02E01", "tt1234567:2:1", true)]
    [InlineData("tt1234567", "tt1234567", true)]
    [InlineData("Example Series S02E01", "tt1234567:2:1", false)]
    [InlineData("Example Movie", "tt1234567", false)]
    [InlineData("Some name", "kitsu:123:4", false)]
    [InlineData("kitsu:123:4", "kitsu:123:4", true)]
    public void IsUnresolvedTitle_DetectsRawIdFallbacks(string? title, string contentId, bool expected)
        => Assert.Equal(expected, WtTitles.IsUnresolvedTitle(title, contentId));

    [Theory]
    [InlineData("tt1234567", "Example Movie", "Example Movie")]
    [InlineData("tt1234567:2", "Example Series", "Example Series S02")]
    [InlineData("tt1234567:2:1", "Example Series", "Example Series S02E01")]
    [InlineData("tt1234567:11:13", "Example Series", "Example Series S11E13")]
    public void ComposeImdbTitle_AppendsSeasonAndEpisodeCodes(string contentId, string baseTitle, string expected)
        => Assert.Equal(expected, WtTitles.ComposeImdbTitle(baseTitle, contentId));

    [Fact]
    public async Task ResolveAsync_NonImdbId_ReturnsNull()
    {
        var resolved = await WtTitles.ResolveAsync(new ImdbTitleResolver(), "series", "kitsu:123:4", CancellationToken.None);
        Assert.Null(resolved);
    }
}
