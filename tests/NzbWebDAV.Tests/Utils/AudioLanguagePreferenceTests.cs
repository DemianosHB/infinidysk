using NzbWebDAV.Utils;

namespace NzbWebDAV.Tests.Utils;

public class AudioLanguagePreferenceTests
{
    [Theory]
    [InlineData("Spider-Man.Brand.New.Day.2026.GERMAN.WEB-DL.2160p.ATMOS", "de")]
    [InlineData("Show.S01E04.GER.720p.HDTV.x264", "de")]
    [InlineData("Movie.2026.DL.1080p.BluRay.x264", "de,en")]
    [InlineData("Movie.2026.DL.2160p.WEB.H265.TrueHD.Atmos", "de,en")]
    [InlineData("Stalingrad.2013.German.Dubbed.1080p.BluRay.x264", "de")]
    [InlineData("Movie.2026.PT-BR.BLURAY.2160p.HDR10-GRuP", "pt")]
    [InlineData("Movie.2026.Portuguese.Brazil.720p.WEBRip", "pt")]
    [InlineData("Movie.2024.ENGLiSH.720p.WEBRip.XviD", "en")]
    [InlineData("Movie.2024.FRENCH.1080p.BluRay.x264", "fr")]
    [InlineData("Movie.2024.TRUEFRENCH.720p.BDRip", "fr")]
    [InlineData("Movie.2024.VF2.720p.BluRay", "fr")]
    [InlineData("Movie.2024.NL.DUTCH.1080p.WEB-DL", "nl")]
    [InlineData("Movie.2024.SWEDISH.1080p.BluRay.x264", "swe")]
    public void ParseTitleLanguages_DetectsSceneConventions(string title, string expected)
    {
        var languages = AudioLanguagePreference.ParseTitleLanguages(title);
        Assert.Equal(
            expected.Split(',', StringSplitOptions.RemoveEmptyEntries).ToHashSet(StringComparer.Ordinal),
            languages.ToHashSet(StringComparer.Ordinal));
    }

    [Theory]
    [InlineData("Movie.2026.2160p.WEB-DL.x264")]
    [InlineData("Movie.2026.1080p.WEBDL.DD5.1.H.264-GRP")]
    [InlineData("Movie.2026.2160p.WEB.DL.Rip.x265")]
    [InlineData("Movie.2026.MULTi.1080p.BluRay.x264")]
    [InlineData("Movie.2026.DUAL.AUDIO.1080p.BluRay")]
    [InlineData("Movie.2020.GerSub.1080p.BluRay.x264")]
    [InlineData("Movie.2020.GERMAN.SUBS.1080p.BluRay.x264")]
    [InlineData("Stranger.Than.Fiction.2006.1080p.BluRay.x264")]
    [InlineData("The.DLNA.Movie.2020.1080p.BluRay")]
    [InlineData("Movie.2020.NL.SUBS.1080p.BluRay")]
    public void ParseTitleLanguages_FailsOpenForAmbiguousOrSubtitleMarkers(string title)
    {
        Assert.Empty(AudioLanguagePreference.ParseTitleLanguages(title));
    }

    [Theory]
    [InlineData("Movie.2024.German.Dubbed.1080p.WEB-DL.x264-GRP", "de")]
    [InlineData("Movie.2024.GERMAN.HDR.2160p.WEB-DL.H265-GRP", "de")]
    public void ParseTitleLanguages_GermanTokenWinsOverNeutralisedWebDlTag(string title, string expected)
    {
        var languages = AudioLanguagePreference.ParseTitleLanguages(title);
        Assert.Contains(expected, languages);
    }

    [Fact]
    public void Detect_IndexerAttributeWinsOverTitle()
    {
        // PT-BR title, but the indexer attributes the release as German.
        var languages = AudioLanguagePreference.Detect("German", "Movie.2026.PT-BR.BLURAY.2160p");
        Assert.Equal(
            new HashSet<string>(StringComparer.Ordinal) { "de" },
            languages.ToHashSet(StringComparer.Ordinal));
    }

    [Fact]
    public void Detect_JunkAttributeFallsBackToTitle()
    {
        var languages = AudioLanguagePreference.Detect("N/A", "Movie.2026.GERMAN.WEB-DL.2160p");
        Assert.Equal(
            new HashSet<string>(StringComparer.Ordinal) { "de" },
            languages.ToHashSet(StringComparer.Ordinal));
    }

    [Fact]
    public void Detect_NoSourcesYieldsEmpty()
    {
        Assert.Empty(AudioLanguagePreference.Detect(null, null));
        Assert.Empty(AudioLanguagePreference.Detect("N/A", "Movie.2026.2160p.WEB-DL"));
    }

    [Theory]
    [InlineData("German", "Movie.2026.GERMAN.2160p", 0)]
    [InlineData(null, "Movie.2026.GERMAN.2160p", 0)]
    [InlineData(null, "Movie.2026.DL.2160p", 0)]
    [InlineData(null, "Movie.2024.ENGLiSH.1080p", 1)]
    [InlineData(null, "Movie.2026.PT-BR.BLURAY.2160p", 2)]
    [InlineData(null, "Movie.2026.MULTi.1080p", 2)]
    [InlineData(null, "Movie.2026.2160p.WEB-DL", 2)]
    [InlineData(null, null, 2)]
    public void Rank_BestPreferredIndexWinsFailOpenOtherwise(string? attr, string? title, int expected)
    {
        var preferred = new List<string> { "de", "en" };
        Assert.Equal(expected, AudioLanguagePreference.Rank(preferred, attr, title));
    }

    [Fact]
    public void Rank_EmptyPreferredListIsNeutral()
    {
        Assert.Equal(0, AudioLanguagePreference.Rank([], "German", "Movie.GERMAN"));
    }

    [Fact]
    public void NormalizePreferred_FoldsNamesAndPreservesOrder()
    {
        Assert.Equal(
            new List<string> { "de", "en" },
            AudioLanguagePreference.NormalizePreferred(["German", "EN"]));
        Assert.Equal(
            new List<string> { "de", "en" },
            AudioLanguagePreference.NormalizePreferred(["de, en", "de"]));
        Assert.Empty(AudioLanguagePreference.NormalizePreferred(null));
        Assert.Empty(AudioLanguagePreference.NormalizePreferred(["   "]));
    }
}
