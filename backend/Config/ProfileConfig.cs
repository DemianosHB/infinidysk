using System.Text.Json.Serialization;
using NzbWebDAV.Extensions;

namespace NzbWebDAV.Config;

public class ProfileConfig
{
    public List<Profile> Profiles { get; set; } = [];

    public ProfileConfig Normalized()
    {
        foreach (var p in Profiles) p.MigrateLegacy();
        return this;
    }

    /// <summary>
    /// Find a profile by token using constant-time comparisons over the full list
    /// (no early exit) so timing does not leak token prefix/position.
    /// </summary>
    public Profile? FindByToken(string token)
    {
        Profile? match = null;
        foreach (var profile in Profiles)
        {
            // Always compare; keep the first match without short-circuiting the loop.
            if (token.FixedTimeEquals(profile.Token) && match is null)
                match = profile;
        }
        return match;
    }

    public class Profile
    {
        public required string Token { get; set; }
        public required string Name { get; set; }
        public List<string> IndexerNames { get; set; } = [];
        public List<string>? EnabledAdapters { get; set; }

        public QualitySortMode QualitySort { get; set; } = QualitySortMode.Off;

        /// <summary>
        /// Ordered list of preferred audio languages (e.g. ["de", "en"]; language names
        /// like "German" are normalised too). Biases result ordering — and therefore
        /// preflight/watchtower candidate selection — toward releases whose audio
        /// language matches an earlier entry; ties fall through to the existing
        /// resolution/source ordering. Fail-open: releases whose language cannot be
        /// detected confidently are never demoted, they merely receive no boost.
        /// Null/empty disables the bias (default).
        /// </summary>
        public List<string>? PreferredAudioLanguages { get; set; }

        public FallbackMode MovieFallback { get; set; } = FallbackMode.Off;
        public FallbackMode TvFallback { get; set; } = FallbackMode.Off;
        public int MovieFallbackMinResults { get; set; } = 3;
        public int TvFallbackMinResults { get; set; } = 3;

        public int? QueryFallbackMinResults { get; set; }

        internal void MigrateLegacy()
        {
            if (QueryFallbackMinResults is { } legacy && legacy > 0
                && MovieFallback == FallbackMode.Off
                && TvFallback == FallbackMode.Off)
            {
                MovieFallback = FallbackMode.Title;
                TvFallback = FallbackMode.Broad;
                MovieFallbackMinResults = legacy;
                TvFallbackMinResults = legacy;
            }
            QueryFallbackMinResults = null;
        }
    }

    [JsonConverter(typeof(JsonStringEnumConverter))]
    public enum FallbackMode
    {
        Off = 0,
        Title = 1,
        Broad = 2,
    }

    [JsonConverter(typeof(JsonStringEnumConverter))]
    public enum QualitySortMode
    {
        Off = 0,
        Resolution = 1,
        ResolutionAndSource = 2,
    }
}
