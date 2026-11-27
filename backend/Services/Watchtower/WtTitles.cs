namespace NzbWebDAV.Services;

public static class WtTitles
{
    public static bool IsImdbId(string contentId)
    {
        var s = contentId;
        var colon = s.IndexOf(':', StringComparison.Ordinal);
        if (colon > 0) s = s[..colon];
        if (s.StartsWith("tt", StringComparison.OrdinalIgnoreCase)) s = s[2..];
        return s.Length > 0 && s.All(char.IsDigit);
    }

    public static string CanonicalImdb(string contentId)
    {
        var s = contentId;
        var colon = s.IndexOf(':', StringComparison.Ordinal);
        if (colon > 0) s = s[..colon];
        return s.StartsWith("tt", StringComparison.OrdinalIgnoreCase) ? s : "tt" + s;
    }

    public static bool IsUnresolvedTitle(string? title, string contentId)
    {
        if (string.IsNullOrWhiteSpace(title)) return true;
        if (title == contentId) return true;
        var baseId = contentId.Split(':')[0];
        return IsImdbId(baseId) && title.StartsWith(baseId, StringComparison.OrdinalIgnoreCase);
    }

    public static string ComposeImdbTitle(string baseTitle, string contentId)
    {
        var parts = contentId.Split(':');
        if (parts.Length >= 3
            && int.TryParse(parts[1], out var season)
            && int.TryParse(parts[2], out var episode))
            return $"{baseTitle} S{season:D2}E{episode:D2}";
        if (parts.Length == 2 && int.TryParse(parts[1], out var s))
            return $"{baseTitle} S{s:D2}";
        return baseTitle;
    }

    public static async Task<string?> ResolveAsync(
        ImdbTitleResolver resolver, string type, string contentId, CancellationToken ct)
    {
        if (!IsImdbId(contentId)) return null;
        var imdb = CanonicalImdb(contentId);
        var baseType = type == "movie" ? "movie" : "series";
        var baseTitle = await resolver
            .GetTitleAsync(baseType, imdb[2..], null, ct)
            .ConfigureAwait(false);
        return string.IsNullOrWhiteSpace(baseTitle) ? null : ComposeImdbTitle(baseTitle, contentId);
    }
}
