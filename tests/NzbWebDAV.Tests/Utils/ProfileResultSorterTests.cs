using NzbWebDAV.Config;
using NzbWebDAV.Utils;

namespace NzbWebDAV.Tests.Utils;

public class ProfileResultSorterTests
{
    [Fact]
    public void Sort_Off_ReproducesGrabsSizeDateOrder()
    {
        var items = new[]
        {
            new Release("Movie.2024.2160p.WEB-DL", Grabs: 1, Size: 10, Posted: DateTimeOffset.UnixEpoch),
            new Release("Movie.2024.1080p.BluRay", Grabs: 50, Size: 20, Posted: DateTimeOffset.UnixEpoch.AddDays(1)),
            new Release("Movie.2024.720p.WEB-DL", Grabs: 50, Size: 30, Posted: DateTimeOffset.UnixEpoch),
        };

        var sorted = Sort(items, ProfileConfig.QualitySortMode.Off, preferDownloaded: true);

        Assert.Equal(
            ["Movie.2024.720p.WEB-DL", "Movie.2024.1080p.BluRay", "Movie.2024.2160p.WEB-DL"],
            sorted.Select(x => x.Title));
    }

    [Fact]
    public void Sort_Resolution_IgnoresSourceRank()
    {
        var items = new[]
        {
            new Release("Movie.2024.1080p.REMUX", 100, 50, DateTimeOffset.UnixEpoch),
            new Release("Movie.2024.2160p.HDTV", 1, 10, DateTimeOffset.UnixEpoch),
            new Release("Movie.2024.1080p.WEB-DL", 500, 40, DateTimeOffset.UnixEpoch),
        };

        var sorted = Sort(items, ProfileConfig.QualitySortMode.Resolution, preferDownloaded: true);

        Assert.Equal(
            ["Movie.2024.2160p.HDTV", "Movie.2024.1080p.WEB-DL", "Movie.2024.1080p.REMUX"],
            sorted.Select(x => x.Title));
    }

    [Fact]
    public void Sort_ResolutionAndSource_AppliesSourceOnlyWithinResolution()
    {
        var items = new[]
        {
            new Release("Movie.2024.1080p.REMUX", 100, 50, DateTimeOffset.UnixEpoch),
            new Release("Movie.2024.2160p.HDTV", 1, 10, DateTimeOffset.UnixEpoch),
            new Release("Movie.2024.1080p.WEB-DL", 500, 40, DateTimeOffset.UnixEpoch),
        };

        var sorted = Sort(items, ProfileConfig.QualitySortMode.ResolutionAndSource);

        Assert.Equal(
            ["Movie.2024.2160p.HDTV", "Movie.2024.1080p.REMUX", "Movie.2024.1080p.WEB-DL"],
            sorted.Select(x => x.Title));
    }

    [Fact]
    public void Sort_FallbackMergedResults_UseTheSameOrdering()
    {
        var initial = new[]
        {
            new Release("Movie.2024.720p.WEB-DL", 1, 10, DateTimeOffset.UnixEpoch),
        };
        var fallback = new[]
        {
            new Release("Movie.2024.2160p.WEB-DL", 1, 10, DateTimeOffset.UnixEpoch),
            new Release("Movie.2024.1080p.BluRay", 1, 20, DateTimeOffset.UnixEpoch),
        };

        var sorted = Sort(initial.Concat(fallback), ProfileConfig.QualitySortMode.ResolutionAndSource);

        Assert.Equal(
            ["Movie.2024.2160p.WEB-DL", "Movie.2024.1080p.BluRay", "Movie.2024.720p.WEB-DL"],
            sorted.Select(x => x.Title));
    }

    [Fact]
    public void Sort_DoesNotReorderAfterWatchtowerBoost()
    {
        var ordinary = Sort(
            [
                new Release("Movie.2024.1080p.WEB-DL", 1, 10, DateTimeOffset.UnixEpoch),
                new Release("Movie.2024.2160p.WEB-DL", 1, 10, DateTimeOffset.UnixEpoch),
            ],
            ProfileConfig.QualitySortMode.Resolution);

        var watchtowerFirst = new Release("Movie.2024.720p.HDTV", 1, 5, DateTimeOffset.UnixEpoch);
        var merged = new[] { watchtowerFirst }.Concat(ordinary).ToList();

        Assert.Equal("Movie.2024.720p.HDTV", merged[0].Title);
        Assert.Equal("Movie.2024.2160p.WEB-DL", merged[1].Title);
    }

    private static List<Release> Sort(
        IEnumerable<Release> items,
        ProfileConfig.QualitySortMode mode,
        bool preferDownloaded = false) =>
        ProfileResultSorter.Sort(
            items,
            item => item.Title,
            item => item.Grabs,
            item => item.Size,
            item => item.Posted,
            mode,
            preferDownloaded);

    private sealed record Release(string Title, int Grabs, long Size, DateTimeOffset Posted, string? Language = null);

    private static List<Release> SortWithBias(
        IEnumerable<Release> items,
        ProfileConfig.QualitySortMode mode,
        IReadOnlyList<string> preferredAudioLanguages,
        bool preferDownloaded = false) =>
        ProfileResultSorter.Sort(
            items,
            item => item.Title,
            item => item.Grabs,
            item => item.Size,
            item => item.Posted,
            mode,
            preferDownloaded,
            item => item.Language,
            preferredAudioLanguages);

    [Fact]
    public void Sort_LanguageBias_PutsPreferredLanguageAboveBetterQuality()
    {
        var items = new[]
        {
            new Release("Movie.2026.PT-BR.BLURAY.2160p.HDR10-GRuP", 50, 30, DateTimeOffset.UnixEpoch),
            new Release("Movie.2026.GERMAN.WEB-DL.1080p.x264-DE", 5, 10, DateTimeOffset.UnixEpoch),
        };

        var sorted = SortWithBias(items, ProfileConfig.QualitySortMode.ResolutionAndSource, ["de"]);

        Assert.Equal(
            ["Movie.2026.GERMAN.WEB-DL.1080p.x264-DE", "Movie.2026.PT-BR.BLURAY.2160p.HDR10-GRuP"],
            sorted.Select(x => x.Title));
    }

    [Fact]
    public void Sort_LanguageBias_IndexerAttributeBeatsTitleTokens()
    {
        var items = new[]
        {
            // Title screams German but the indexer attributes Portuguese-Brazil.
            new Release("Movie.2026.GERMAN.2160p.WEB-DL-GRP", 50, 30, DateTimeOffset.UnixEpoch, "Português (Brasil)"),
            new Release("Movie.2026.GERMAN.1080p.WEB-DL-DE2", 5, 10, DateTimeOffset.UnixEpoch, "German"),
        };

        var sorted = SortWithBias(items, ProfileConfig.QualitySortMode.ResolutionAndSource, ["de"]);

        Assert.Equal("Movie.2026.GERMAN.1080p.WEB-DL-DE2", sorted[0].Title);
    }

    [Fact]
    public void Sort_LanguageBias_TieBreaksAmongMatchesByResolution()
    {
        var items = new[]
        {
            new Release("Movie.2026.GERMAN.1080p.WEB-DL", 1, 10, DateTimeOffset.UnixEpoch),
            new Release("Movie.2026.GERMAN.2160p.WEB-DL", 1, 20, DateTimeOffset.UnixEpoch),
        };

        var sorted = SortWithBias(items, ProfileConfig.QualitySortMode.ResolutionAndSource, ["de"]);

        Assert.Equal("Movie.2026.GERMAN.2160p.WEB-DL", sorted[0].Title);
    }

    [Fact]
    public void Sort_LanguageBias_UnknownLanguageKeepsQualityOrder()
    {
        // Neither release matches the preference: a known foreign release and an
        // unknown-language release share the non-match rank, so the existing
        // resolution ordering decides (fail-open: no demotion of unknowns).
        var items = new[]
        {
            new Release("Movie.2026.MULTi.1080p.BluRay", 1, 10, DateTimeOffset.UnixEpoch),
            new Release("Movie.2026.PT-BR.2160p.BluRay", 1, 20, DateTimeOffset.UnixEpoch),
        };

        var sorted = SortWithBias(items, ProfileConfig.QualitySortMode.ResolutionAndSource, ["de"]);

        Assert.Equal("Movie.2026.PT-BR.2160p.BluRay", sorted[0].Title);
    }

    [Fact]
    public void Sort_LanguageBias_AppliesEvenWhenQualitySortIsOff()
    {
        var items = new[]
        {
            new Release("Movie.2026.PT-BR.2160p.BluRay", 1, 10, DateTimeOffset.UnixEpoch),
            new Release("Movie.2026.DL.1080p.WEB-DL", 1, 10, DateTimeOffset.UnixEpoch),
        };

        var sorted = SortWithBias(items, ProfileConfig.QualitySortMode.Off, ["de", "en"]);

        Assert.Equal("Movie.2026.DL.1080p.WEB-DL", sorted[0].Title);
    }

    [Fact]
    public void Sort_WithoutPreferredLanguages_KeepsQualityOrder()
    {
        var items = new[]
        {
            new Release("Movie.2026.GERMAN.WEB-DL.1080p.x264-DE", 5, 10, DateTimeOffset.UnixEpoch),
            new Release("Movie.2026.PT-BR.BLURAY.2160p.HDR10-GRuP", 50, 30, DateTimeOffset.UnixEpoch),
        };

        var sorted = ProfileResultSorter.Sort(
            items,
            item => item.Title,
            item => item.Grabs,
            item => item.Size,
            item => item.Posted,
            ProfileConfig.QualitySortMode.ResolutionAndSource,
            preferDownloaded: false,
            language: item => item.Language,
            preferredAudioLanguages: []);

        Assert.Equal("Movie.2026.PT-BR.BLURAY.2160p.HDR10-GRuP", sorted[0].Title);
    }
}
