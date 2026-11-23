using NzbWebDAV.Config;
using NzbWebDAV.Services;

namespace NzbWebDAV.Tests.Services;

/// <summary>
/// Fork (stall negative marking): the advisory stall mark must stay isolated from the
/// queue-failure negative cache — a serving stall can be transient, so it must never
/// poison a candidate as permanently failed, and a genuine failure must not imply a
/// stall. Default config keeps the same TTL source for both reads.
/// </summary>
public sealed class CandidateNegativeCacheStallMarkTests
{
    private static CandidateNegativeCache CreateCache() =>
        new(new ConfigManager());

    [Fact]
    public void MarkStalled_Only_Affects_The_Marked_Url()
    {
        var cache = CreateCache();

        cache.MarkStalled("https://indexer.example/nzb/a");

        Assert.True(cache.IsStalled("https://indexer.example/nzb/a"));
        Assert.False(cache.IsStalled("https://indexer.example/nzb/b"));
        Assert.False(cache.IsStalled(""));
    }

    [Fact]
    public void Stall_Mark_Does_Not_Poison_The_Negative_Cache()
    {
        var cache = CreateCache();

        cache.MarkStalled("https://indexer.example/nzb/a");

        Assert.False(cache.IsFailed("https://indexer.example/nzb/a"));
    }

    [Fact]
    public void Negative_Cache_Mark_Does_Not_Imply_A_Stall()
    {
        var cache = CreateCache();

        cache.MarkFailed("https://indexer.example/nzb/a");

        Assert.False(cache.IsStalled("https://indexer.example/nzb/a"));
    }

    [Fact]
    public void Unmarked_Urls_Are_Never_Stalled()
    {
        var cache = CreateCache();

        Assert.False(cache.IsStalled("https://indexer.example/nzb/never-marked"));
    }

    [Fact]
    public void Broken_File_Name_And_Stall_Are_Independent_Marks()
    {
        var cache = CreateCache();

        cache.MarkStalled("https://indexer.example/nzb/a");
        cache.MarkFileNameBroken("release-name.mkv");

        Assert.True(cache.IsStalled("https://indexer.example/nzb/a"));
        Assert.True(cache.IsFileNameBroken("release-name.mkv"));
        Assert.False(cache.IsFileNameBroken("other-release.mkv"));
    }
}
