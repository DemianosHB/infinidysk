using System.Text.RegularExpressions;

namespace NzbWebDAV.Utils;

/// <summary>
/// Helpers for biasing profile result ordering toward the user's preferred audio
/// languages (e.g. ["de", "en"]). Mirrors the subtitle bias pattern in
/// <see cref="SubtitlePreference"/>: an ordering bias, never a filter.
///
/// Language detection prefers the indexer-supplied language attribute (the Newznab
/// "language" attribute) and falls back to release-name token parsing (GERMAN, GER,
/// DL, PT-BR, ENGLiSH, …) using the token conventions established by Stremio addon
/// parsers (AIOStreams/Torrentio), including word-boundary guards and a
/// subtitle-marker exclusion so "GerSub"/"German Subs" never count as German audio.
///
/// Fail-open by design: when no confident language can be detected (neither attribute
/// nor release name yields a known token), the release is treated as a non-match — it
/// never receives a boost, but it is also never demoted below other non-matching
/// releases. Known limitation: language words inside the content title itself (e.g. a
/// film literally titled "French Dispatch") can produce a false positive; this only
/// matters when the false positive names a preferred language and is accepted for a
/// bias (in contrast to a filter, it cannot hide releases).
/// Pure and side-effect free.
/// </summary>
public static class AudioLanguagePreference
{
    // Token-boundary guards ported from the AIOStreams/Torrentio convention:
    // a language token must not be preceded by a word character (letters/digits other
    // than the separator set) and must be followed by a separator or end of string, so
    // "ger" inside "Stranger" or "dl" inside "DLNA" never matches.
    private const string TokenGuardPrefix = @"(?<![^\s\[(_\-.,])";
    private const string TokenGuardSuffix = @"(?=[\s\[()\]_.\-,]|$)";
    private const string SubtitleMarkerExclusion = @"(?![ .\-_]?sub(title)?s?)";

    private const RegexOptions Options =
        RegexOptions.IgnoreCase | RegexOptions.Compiled | RegexOptions.CultureInvariant;

    /// <summary>
    /// German-Usenet convention: a bare "DL" token marks a dual-language release with
    /// German audio (German + English). Neutralises the WEB-DL source tag first so the
    /// source is never mistaken for the token ("WEB-DL", "WEBDL", "Web.DL.Rip" all
    /// collapse to "web" before scanning).
    /// </summary>
    private static readonly Regex WebDlSourceTag = new(@"web[ .\-_]?dl", Options);

    private static readonly Regex DualLanguageToken = new(
        TokenGuardPrefix + @"dl" + SubtitleMarkerExclusion + TokenGuardSuffix,
        Options);

    // Release-name token patterns per canonical language. Multiple patterns for the
    // same language are combined with "|". Ported from the AIOStreams language table;
    // canonical tokens align with the SubtitlePreference alias map so attribute-derived
    // and title-derived tokens compare equal.
    private static readonly (string Canonical, string Pattern)[] TitlePatterns =
    [
        ("en", "english|eng"),
        ("de", "deu(tsch)?(land)?|ger(man)?"),
        ("fr", "french|fra|fr|vf|vff|vfi|vf2|vfq|truefrench"),
        ("es", "spanish|spa|esp|latino"),
        ("it", "italian|ita"),
        ("pt", "portuguese[ .\\-_]?brazil|pt[ .\\-_]?br"),
        ("pt", "portuguese"),
        ("nl", "dutch|dut"),
        ("ru", "russian|rus"),
        ("ja", "japanese|jap|jpn"),
        ("ko", "korean|kor"),
        ("zh", "chinese|chi"),
        ("ar", "arabic|ara"),
        ("hi", "hindi|hin"),
        ("tr", "turkish|tur"),
        ("swe", "swedish|swe"),
        ("nor", "norwegian|nor"),
        ("dan", "danish|dan"),
        ("fin", "finnish|fin"),
        ("pol", "polish|pol"),
        ("bn", "bengali|ben(?![ .\\-_]?the[ .\\-_]?men)"),
        ("pa", "punjabi|pan"),
        ("mr", "marathi|mar"),
        ("gu", "gujarati|guj"),
        ("ta", "tamil|tam"),
        ("te", "telugu|tel"),
        ("kn", "kannada|kan"),
        ("ml", "malayalam|mal"),
        ("th", "thai|tha"),
        ("vi", "vietnamese|vie"),
        ("id", "indonesian|ind"),
        ("he", "hebrew|heb"),
        ("fa", "persian|per"),
        ("uk", "ukrainian|ukr"),
        ("el", "greek|ell"),
        ("lt", "lithuanian|lit"),
        ("lv", "latvian|lav"),
        ("et", "estonian|est"),
        ("cs", "czech|cze"),
        ("sk", "slovak|slo"),
        ("hu", "hungarian|hun"),
        ("ro", "romanian|rum"),
        ("bg", "bulgarian|bul"),
        ("sr", "serbian|srp"),
        ("hr", "croatian|hrv"),
        ("sl", "slovenian|slv"),
        ("ms", "malay"),
    ];

    private static readonly Dictionary<string, Regex> TitleTokenPatterns = BuildTitleTokenPatterns();

    /// <summary>
    /// Detect the audio languages of a release. The indexer's language attribute wins
    /// when it yields a confident language; otherwise the release name is parsed. Returns
    /// an empty set when neither source yields a confident language (fail-open).
    /// </summary>
    public static IReadOnlyCollection<string> Detect(string? indexerLanguage, string? title)
    {
        var fromAttribute = ParseAttributeLanguages(indexerLanguage);
        if (fromAttribute.Count > 0) return fromAttribute;
        return ParseTitleLanguages(title);
    }

    /// <summary>
    /// Two-letter attribute tokens are only confident when they are known ISO codes;
    /// indexers sometimes emit junk like "n/a" which splits into single letters.
    /// </summary>
    private static readonly HashSet<string> KnownTwoLetterTokens = new(StringComparer.Ordinal)
    {
        "en", "es", "fr", "de", "it", "pt", "nl", "ru", "ja", "ko", "zh", "ar", "hi",
    };

    private static readonly HashSet<string> NoiseTokens = new(StringComparer.Ordinal)
    {
        "unknown", "none", "other", "original", "noidentity", "nolanguage",
    };

    private static HashSet<string> ParseAttributeLanguages(string? indexerLanguage)
    {
        return SubtitlePreference.ParseLanguages(indexerLanguage)
            .Where(token => (token.Length > 2 || KnownTwoLetterTokens.Contains(token))
                            && !NoiseTokens.Contains(token))
            .ToHashSet(StringComparer.Ordinal);
    }

    /// <summary>
    /// Parse release-name tokens into canonical language tokens. MULTI/DUAL AUDIO/DUBBED
    /// markers are deliberately not mapped to a language: they do not state which
    /// languages are present, so they stay fail-open (no boost, no demotion).
    /// </summary>
    public static IReadOnlyCollection<string> ParseTitleLanguages(string? title)
    {
        var result = new HashSet<string>(StringComparer.Ordinal);
        if (string.IsNullOrWhiteSpace(title)) return result;

        var normalized = WebDlSourceTag.Replace(title, "web");

        foreach (var (canonical, regex) in TitleTokenPatterns)
        {
            if (regex.IsMatch(normalized))
                result.Add(canonical);
        }

        if (DualLanguageToken.IsMatch(normalized))
        {
            result.Add("de");
            result.Add("en");
        }

        return result;
    }

    /// <summary>
    /// Rank a release against the ordered preferred list: the index of the best matching
    /// preferred entry, or <paramref name="preferred"/>.Count when nothing matches.
    /// Releases with no confidently detected language share the non-match rank, so the
    /// bias only boosts matches and never demotes unknowns below other non-matches.
    /// Returns 0 for an empty preferred list (bias disabled).
    /// </summary>
    public static int Rank(IReadOnlyList<string> preferred, string? indexerLanguage, string? title)
    {
        if (preferred.Count == 0) return 0;

        var languages = Detect(indexerLanguage, title);
        if (languages.Count == 0) return preferred.Count;

        var best = preferred.Count;
        for (var i = 0; i < preferred.Count; i++)
        {
            if (i >= best) break;
            if (languages.Contains(preferred[i]))
                best = i;
        }
        return best;
    }

    /// <summary>
    /// Normalise user-supplied preferred languages ("German", "de", "DEU", "de, en")
    /// into an ordered list of canonical tokens, preserving preference order and
    /// dropping duplicates.
    /// </summary>
    public static List<string> NormalizePreferred(IEnumerable<string>? preferred)
    {
        var result = new List<string>();
        if (preferred is null) return result;

        var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (var raw in preferred)
        {
            foreach (var token in SubtitlePreference.ParseLanguages(raw))
            {
                if (seen.Add(token))
                    result.Add(token);
            }
        }
        return result;
    }

    private static Dictionary<string, Regex> BuildTitleTokenPatterns()
    {
        var combined = new Dictionary<string, List<string>>(StringComparer.Ordinal);
        foreach (var (canonical, pattern) in TitlePatterns)
        {
            if (!combined.TryGetValue(canonical, out var list))
                combined[canonical] = list = [];
            list.Add(pattern);
        }

        return combined.ToDictionary(
            pair => pair.Key,
            pair => new Regex(
                TokenGuardPrefix + "(?:" + string.Join("|", pair.Value) + ")" + SubtitleMarkerExclusion + TokenGuardSuffix,
                Options),
            StringComparer.Ordinal);
    }
}