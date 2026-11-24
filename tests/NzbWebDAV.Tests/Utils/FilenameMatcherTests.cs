using NzbWebDAV.Utils;

namespace NzbWebDAV.Tests.Utils;

public class FilenameMatcherTests
{
    [Fact]
    public void YearCompatible_UnknownCanonicalYear_IsPermissive()
    {
        Assert.True(FilenameMatcher.YearCompatible(null, "Dune.1984.1080p.BluRay"));
    }

    [Fact]
    public void YearCompatible_ExactYear_Matches()
    {
        Assert.True(FilenameMatcher.YearCompatible(2024, "Dune.Part.Two.2024.2160p.WEB-DL"));
    }

    [Theory]
    [InlineData(2024, "Dune.Part.Two.2023.1080p")]
    [InlineData(2024, "Dune.Part.Two.2025.1080p")]
    public void YearCompatible_PlusOrMinusOne_Matches(int canonicalYear, string title)
    {
        Assert.True(FilenameMatcher.YearCompatible(canonicalYear, title));
    }

    [Fact]
    public void YearCompatible_WrongRemakeYear_IsRejected()
    {
        Assert.False(FilenameMatcher.YearCompatible(2024, "Dune.1984.1080p.BluRay"));
    }

    [Fact]
    public void YearCompatible_MissingReleaseYear_IsPermissive()
    {
        Assert.True(FilenameMatcher.YearCompatible(2024, "Dune.2160p.WEB-DL"));
    }

    [Fact]
    public void ParseReleaseYears_DoesNotTreatResolutionAsYear()
    {
        Assert.Empty(FilenameMatcher.ParseReleaseYears("Dune.2160p.WEB-DL"));
        Assert.Empty(FilenameMatcher.ParseReleaseYears("Movie.1080p.BluRay"));
        Assert.Empty(FilenameMatcher.ParseReleaseYears("Movie.1920x1080.WEB-DL"));
    }

    [Fact]
    public void YearCompatible_NumericTitle1917_UsesTheReleaseYear()
    {
        var titles = new[] { FilenameMatcher.NormalizeTitle("1917") };
        Assert.True(FilenameMatcher.YearCompatible(2019, "1917.2019.1080p.BluRay", titles));
        Assert.False(FilenameMatcher.YearCompatible(2019, "1917.2017.1080p.BluRay", titles));
        Assert.Equal(new[] { 1917, 2019 }, FilenameMatcher.ParseReleaseYears("1917.2019.1080p.BluRay"));
    }

    [Fact]
    public void YearCompatible_NumericTitle1984_UsesTheReleaseYear()
    {
        var titles = new[] { FilenameMatcher.NormalizeTitle("1984") };
        Assert.True(FilenameMatcher.YearCompatible(1984, "1984.1984.1080p", titles));
        Assert.False(FilenameMatcher.YearCompatible(1984, "1984.2023.1080p", titles));
    }

    [Fact]
    public void YearCompatible_BladeRunner2049_DoesNotTreatTitleTokenAsYear()
    {
        var titles = new[] { FilenameMatcher.NormalizeTitle("Blade Runner 2049") };
        Assert.True(FilenameMatcher.YearCompatible(2017, "Blade.Runner.2049.2017.1080p", titles));
        Assert.False(FilenameMatcher.YearCompatible(2017, "Blade.Runner.2049.2015.1080p", titles));
        Assert.True(FilenameMatcher.YearCompatible(2017, "Blade.Runner.2049.2160p.Remux", titles));
    }

    [Fact]
    public void YearCompatible_SeriesStartYearInCanonicalTitle_IsIgnored()
    {
        var titles = new[] { FilenameMatcher.NormalizeTitle("Doctor Who 2005") };
        Assert.True(FilenameMatcher.YearCompatible(2023, "Doctor.Who.2005.S01E01.720p", titles));
    }

    [Fact]
    public void NormalizeTitle_FoldsAmpersandToAnd()
    {
        Assert.Equal("law and order", FilenameMatcher.NormalizeTitle("Law & Order"));
        Assert.Equal(
            "feuer and flamme mit feuerwehrmannern im einsatz",
            FilenameMatcher.NormalizeTitle("Feuer & Flamme - Mit Feuerwehrmännern im Einsatz"));
    }

    [Fact]
    public void TitleMatches_AmpersandCanonical_MatchesAndUndConjunctionReleases()
    {
        var canonical = new[] { FilenameMatcher.NormalizeTitle("Feuer & Flamme") };
        Assert.True(FilenameMatcher.TitleMatches(
            canonical, "Feuer.und.Flamme.S10E07.GERMAN.DOKU.1080p.WEB.x264-TSCC"));
        Assert.True(FilenameMatcher.TitleMatches(
            canonical, "Feuer.and.Flamme.S10E07.GERMAN.DOKU.1080p.WEB.x264-TSCC"));
        Assert.True(FilenameMatcher.TitleMatches(
            canonical, "Feuer & Flamme S10E07 GERMAN DOKU 1080p"));

        var english = new[] { FilenameMatcher.NormalizeTitle("Law & Order") };
        Assert.True(FilenameMatcher.TitleMatches(
            english, "Law.and.Order.S20E01.720p.WEB.h264-GRP"));
    }

    [Fact]
    public void TitleMatches_CanonicalMarketingSubtitle_SceneShortHeadMatchesByPrefix()
    {
        var canonical = new[]
        {
            FilenameMatcher.NormalizeTitle("Feuer & Flamme - Mit Feuerwehrmännern im Einsatz")
        };
        Assert.True(FilenameMatcher.TitleMatches(
            canonical, "Feuer.und.Flamme.S10E07.GERMAN.DOKU.1080p.WEB.x264-TSCC"));
        Assert.True(FilenameMatcher.TitleMatches(
            canonical, "Feuer.und.Flamme.2017.S10.GERMAN.1080p.ARD.WEB-DL.AAC2.0.H.264-KOMET"));
    }

    [Fact]
    public void TitleMatches_PrefixRule_IsOneDirectional()
    {
        // Release head longer than the canonical title: spin-off territory, must stay rejected.
        var canonical = new[] { FilenameMatcher.NormalizeTitle("Law & Order") };
        Assert.False(FilenameMatcher.TitleMatches(
            canonical, "Law.and.Order.SVU.S24E01.720p.WEB.h264-GRP"));

        // Diverging token within the head: different show, stays rejected.
        var svu = new[] { FilenameMatcher.NormalizeTitle("Law & Order: Special Victims Unit") };
        Assert.False(FilenameMatcher.TitleMatches(
            svu, "Law.and.Order.SVU.S24E01.720p.WEB.h264-GRP"));
    }

    [Fact]
    public void TitleMatches_SingleTokenHead_DoesNotMatchLongerCanonical()
    {
        var canonical = new[] { FilenameMatcher.NormalizeTitle("CSI: Crime Scene Investigation") };
        Assert.False(FilenameMatcher.TitleMatches(canonical, "CSI.S03E12.720p.WEB.h264-GRP"));
    }

    [Fact]
    public void TitleMatches_PlainAndApostropheTitles_StillMatchExactly()
    {
        var silo = new[] { FilenameMatcher.NormalizeTitle("Silo") };
        Assert.True(FilenameMatcher.TitleMatches(
            silo, "Silo.2023.S02E01.The.Engineer.2160p.ATV.WEB-DL.MULTi.DDP5.1.Atmos.H.265-FUZEER"));

        var handmaid = new[] { FilenameMatcher.NormalizeTitle("The Handmaid's Tale") };
        Assert.True(FilenameMatcher.TitleMatches(
            handmaid, "The Handmaid's Tale - S01E07 - The Other Side WEBDL-2160p"));

        var loveIsBlind = new[] { FilenameMatcher.NormalizeTitle("Love Is Blind") };
        Assert.True(FilenameMatcher.TitleMatches(
            loveIsBlind, "Love.Is.Blind.S11E13.1080p.WEB.H264-HOTDOGWATER"));
    }
}