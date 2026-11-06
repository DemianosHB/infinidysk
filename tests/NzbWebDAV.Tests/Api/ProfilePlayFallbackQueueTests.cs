using NzbWebDAV.Api.Controllers.Profiles;
using NzbWebDAV.Config;
using NzbWebDAV.Services;
using NzbWebDAV.Utils;

namespace NzbWebDAV.Tests.Api;

// Fork (preferred audio languages): the watchdog play fallback queue must rank
// candidates by the profile's audio-language preference first, then by quality
// (resolution, then source) — the same keys ProfileResultSorter applies to search
// results. The client-reported order (and the subtitle bias) survive as tiebreaks
// within one tier; the clicked release always stays first.

public class ProfilePlayFallbackQueueTests
{
    private const string Token = "token";

    private static readonly IReadOnlyList<string> PreferGermanEnglish = ["de", "en"];

    private static NzbResolutionCache.Candidate Candidate(
        string title,
        string? language = null,
        long size = 1_000,
        string? subs = null)
    {
        return new NzbResolutionCache.Candidate
        {
            IndexerName = "indexer",
            IndexerUserAgent = "ua",
            NzbUrl = $"https://nzb/{title}",
            Title = title,
            Size = size,
            UsenetDate = DateTimeOffset.UnixEpoch,
            Language = language,
            Subs = subs,
        };
    }

    private static NzbResolutionCache.Entry Entry(
        IReadOnlyList<NzbResolutionCache.Candidate> candidates,
        int startIndex)
    {
        return new NzbResolutionCache.Entry
        {
            Candidates = candidates,
            StartIndex = startIndex,
            Type = "series",
            ProfileToken = Token,
            Id = "tt123:1:4",
            CreatedAt = DateTime.UtcNow,
        };
    }

    private static List<string> ClientOrder(params NzbResolutionCache.Candidate[] candidates) =>
        candidates.Select(c => ReleaseIdentity.Key(c.Size, c.Poster, c.UsenetDate, c.NzbUrl)).ToList();

    private static List<string> Titles(List<NzbResolutionCache.Candidate> queue) =>
        queue.Select(c => c.Title).ToList();

    [Fact]
    public void Bias_GermanBeforeEnglish_EvenWhenClientPrefersEnglish()
    {
        // The infinidysk#1765-style scenario in the play path: the client-reported
        // order (AIOStreams' own sorting) ranks a 2160p English WEB-DL above the German
        // releases; the profile bias must pull the German candidates forward and order
        // them by quality (2160p before 1080p).
        var german2160 = Candidate("Devs.S01E04.GERMAN.DL.HDR.2160P.WEB.H265-RiLE", size: 6_050);
        var german1080 = Candidate("Devs.S01E04.Verschiedene.Welten.GERMAN.5.1.DUBBED.DL.AC3.1080p.WEB-DL.h264-TvR", size: 2_880);
        var english2160 = Candidate("Devs.S01E04.2160p.DSNP.WEB-DL.DDP5.1.DV.HEVC-FLUX", size: 5_590);
        var primary = german2160;

        var entry = Entry(new[] { primary, english2160, german1080 }, startIndex: 0);
        var clientOrder = ClientOrder(english2160, german1080);

        var queue = ProfilePlayController.BuildFallbackQueue(
            entry, clientOrder, preferSubtitles: false,
            PreferGermanEnglish, ProfileConfig.QualitySortMode.ResolutionAndSource);

        // Primary first, then German by quality (2160p before 1080p), English last.
        Assert.Equal(
            new[] { german2160.Title, german1080.Title, english2160.Title },
            Titles(queue));
    }

    [Fact]
    public void Bias_QualityOrdersWithinLanguageTier()
    {
        var german1080 = Candidate("Show.GERMAN.1080p.WEB-DL", size: 3_000);
        var german2160 = Candidate("Show.GERMAN.2160p.WEB-DL", size: 6_000);
        var german2160WebRip = Candidate("Show.GERMAN.2160p.WEBRip", size: 5_000);
        var primary = german1080;

        var entry = Entry(new[] { primary, german2160WebRip, german2160 }, startIndex: 0);

        var queue = ProfilePlayController.BuildFallbackQueue(
            entry, preferredOrder: null, preferSubtitles: false,
            PreferGermanEnglish, ProfileConfig.QualitySortMode.ResolutionAndSource);

        Assert.Equal(
            new[] { german1080.Title, german2160.Title, german2160WebRip.Title },
            Titles(queue));
    }

    [Fact]
    public void Bias_QualitySortOff_LanguageBiasKeepsClientOrderWithinTier()
    {
        // With qualitySort Off the quality keys are a constant; only the language tier
        // reorders, and within the German tier the client-reported order survives
        // (a 1080p listed first is NOT jumped by a later 2160p).
        var german2160 = Candidate("Show.GERMAN.2160p.WEB-DL");
        var german1080 = Candidate("Show.GERMAN.1080p.WEB-DL");
        var english2160 = Candidate("Show.2160p.WEB-DL");
        var primary = english2160;

        var entry = Entry(new[] { primary, german2160, german1080 }, startIndex: 0);
        var clientOrder = ClientOrder(german2160, german1080);

        var queue = ProfilePlayController.BuildFallbackQueue(
            entry, clientOrder, preferSubtitles: false,
            PreferGermanEnglish, ProfileConfig.QualitySortMode.Off);

        Assert.Equal(
            new[] { english2160.Title, german2160.Title, german1080.Title },
            Titles(queue));
    }

    [Fact]
    public void Bias_PrimaryStaysFirst_EvenWhenWorseRanked()
    {
        // The explicit click wins: an English primary is not overridden by German
        // candidates, they only follow it in the fallback queue.
        var englishPrimary = Candidate("Show.2160p.WEB-DL");
        var german1080 = Candidate("Show.GERMAN.1080p.WEB-DL");

        var entry = Entry(new[] { englishPrimary, german1080 }, startIndex: 0);

        var queue = ProfilePlayController.BuildFallbackQueue(
            entry, preferredOrder: null, preferSubtitles: false,
            PreferGermanEnglish, ProfileConfig.QualitySortMode.ResolutionAndSource);

        Assert.Equal(new[] { englishPrimary.Title, german1080.Title }, Titles(queue));
    }

    [Fact]
    public void NoPreference_PreservesClientOrderExactly()
    {
        // Regression guard: without preferred languages and with qualitySort Off the
        // queue is exactly the pre-bias behaviour (client order, primary first).
        var a = Candidate("Show.A.1080p.WEB-DL", size: 1_000);
        var b = Candidate("Show.B.GERMAN.2160p.WEB-DL", size: 2_000);
        var primary = Candidate("Show.Primary.720p.WEB-DL", size: 900);

        var entry = Entry(new[] { primary, b, a }, startIndex: 0);
        var clientOrder = ClientOrder(b, a);

        var queue = ProfilePlayController.BuildFallbackQueue(
            entry, clientOrder, preferSubtitles: false);

        Assert.Equal(new[] { primary.Title, b.Title, a.Title }, Titles(queue));
    }

    [Fact]
    public void NoPreference_PreservesSizeProximityExactly()
    {
        // No client-reported order: base ordering stays size proximity around the
        // clicked release (smaller candidates descending, then larger ascending).
        var smaller = Candidate("Show.Small.1080p.WEB-DL", size: 500);
        var larger = Candidate("Show.Large.2160p.WEB-DL", size: 5_000);
        var primary = Candidate("Show.Primary.1080p.WEB-DL", size: 1_000);

        var entry = Entry(new[] { primary, larger, smaller }, startIndex: 0);

        var queue = ProfilePlayController.BuildFallbackQueue(
            entry, preferredOrder: null, preferSubtitles: false);

        Assert.Equal(new[] { primary.Title, smaller.Title, larger.Title }, Titles(queue));
    }

    [Fact]
    public void Bias_UnknownLanguageSharesNonMatchTier()
    {
        // Fail-open: releases with no confidently detected language keep their relative
        // order and are never demoted below other non-matches; a preferred match still
        // comes first. (Watch the title: a bare "DL" token would itself map to de+en.)
        var unknownA = Candidate("Show.S01E04.AC3.1080p.WEB-DL", size: 1_100);
        var unknownB = Candidate("Show.S01E04.1080p.WEB-DL", size: 1_200);
        var german = Candidate("Show.GERMAN.1080p.WEB-DL", size: 1_300);
        var primary = german;

        var entry = Entry(new[] { primary, unknownB, unknownA }, startIndex: 0);
        var clientOrder = ClientOrder(unknownB, unknownA);

        var queue = ProfilePlayController.BuildFallbackQueue(
            entry, clientOrder, preferSubtitles: false,
            PreferGermanEnglish, ProfileConfig.QualitySortMode.Off);

        // German (primary) first; the two unknown-language releases keep their
        // client-reported relative order behind the preferred match.
        Assert.Equal(
            new[] { german.Title, unknownB.Title, unknownA.Title },
            Titles(queue));
    }

    [Fact]
    public void Bias_SubtitlePreferenceSurvivesWithinLanguageQualityTier()
    {
        // Two German 1080p WEB-DL releases in the same language + quality tier: the
        // subtitle bias (subbed ranks above subless) keeps deciding between them.
        var germanSubbed = Candidate("Show.GERMAN.1080p.WEB-DL", size: 3_000, subs: "German");
        var germanSubless = Candidate("Show.GERMAN.1080p.WEB-DL.Alt", size: 3_100);
        var primary = Candidate("Show.GERMAN.2160p.WEB-DL", size: 6_000, subs: "German");

        var entry = Entry(new[] { primary, germanSubless, germanSubbed }, startIndex: 0);
        var clientOrder = ClientOrder(germanSubless, germanSubbed);

        var queue = ProfilePlayController.BuildFallbackQueue(
            entry, clientOrder, preferSubtitles: true,
            PreferGermanEnglish, ProfileConfig.QualitySortMode.ResolutionAndSource);

        Assert.Equal(
            new[] { primary.Title, germanSubbed.Title, germanSubless.Title },
            Titles(queue));
    }

    [Fact]
    public void Bias_LanguageAttributeWinsOverTitleParsing()
    {
        // A candidate whose indexer language attribute says German but whose title
        // carries no German token ranks with the German tier (attribute is primary,
        // mirroring AudioLanguagePreference.Detect) — above a release whose language
        // cannot be detected at all.
        var attributeGerman = Candidate("Show.S01E04.1080p.WEB-DL", language: "German", size: 3_000);
        var unknown = Candidate("Show.S01E04.1080p.WEB-DL.Alt", language: null, size: 3_050);
        var englishPrimary = Candidate("Show.S01E04.1080p.WEB-DL.ENG", language: "English", size: 3_100);

        var entry = Entry(new[] { englishPrimary, unknown, attributeGerman }, startIndex: 0);

        var queue = ProfilePlayController.BuildFallbackQueue(
            entry, preferredOrder: null, preferSubtitles: false,
            PreferGermanEnglish, ProfileConfig.QualitySortMode.Off);

        Assert.Equal(
            new[] { englishPrimary.Title, attributeGerman.Title, unknown.Title },
            Titles(queue));
    }
}
