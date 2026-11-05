using NzbWebDAV.Config;

namespace NzbWebDAV.Utils;

public static class ProfileResultSorter
{
    public static List<T> Sort<T>(
        IEnumerable<T> items,
        Func<T, string?> title,
        Func<T, int?> grabs,
        Func<T, long> size,
        Func<T, DateTimeOffset?> posted,
        ProfileConfig.QualitySortMode qualitySort,
        bool preferDownloaded,
        Func<T, string?>? language = null,
        IReadOnlyList<string>? preferredAudioLanguages = null)
    {
        var bias = language is not null && preferredAudioLanguages is { Count: > 0 };

        var ranked = items.Select(item =>
        {
            var quality = qualitySort == ProfileConfig.QualitySortMode.Off
                ? ReleaseQualityRanks.Unknown
                : ReleaseQuality.Parse(title(item));
            return new Ranked<T>(
                item,
                bias
                    ? AudioLanguagePreference.Rank(preferredAudioLanguages!, language(item), title(item))
                    : 0,
                qualitySort == ProfileConfig.QualitySortMode.Off ? 0 : quality.Resolution,
                qualitySort == ProfileConfig.QualitySortMode.ResolutionAndSource ? quality.Source : 0,
                grabs(item) ?? -1,
                size(item),
                posted(item) ?? DateTimeOffset.MinValue);
        });

        // Language bias first; a constant key is stable, so with the bias disabled the
        // ordering below is exactly the pre-bias behaviour.
        var ordered = ranked.OrderBy(x => x.LanguageRank);

        if (qualitySort != ProfileConfig.QualitySortMode.Off)
            ordered = ordered.ThenByDescending(x => x.Resolution).ThenByDescending(x => x.Source);

        if (preferDownloaded)
            ordered = ordered.ThenByDescending(x => x.Grabs);

        return ordered
            .ThenByDescending(x => x.Size)
            .ThenByDescending(x => x.Posted)
            .Select(x => x.Item)
            .ToList();
    }

    private sealed record Ranked<T>(
        T Item,
        int LanguageRank,
        int Resolution,
        int Source,
        int Grabs,
        long Size,
        DateTimeOffset Posted);
}
