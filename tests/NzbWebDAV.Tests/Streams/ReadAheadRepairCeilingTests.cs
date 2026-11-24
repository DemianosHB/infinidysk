using NzbWebDAV.Streams;
using NzbWebDAV.Tests.Fakes;
using NzbWebDAV.WebDav.Requests;

namespace NzbWebDAV.Tests.Streams;

public sealed class ReadAheadRepairCeilingTests
{
    [Theory]
    [InlineData(0, 45, 1_000_000, 0)]
    [InlineData(-5, 45, 1_000_000, 0)]
    [InlineData(1_000, 0, 1_000_000, 0)]
    [InlineData(500_000, 45, 1_000_000_000, 22_500_000)]
    [InlineData(1_000_000, 60, 10_000_000, 10_000_000)]
    public void ComputeReadAheadWindowBytes_MatchesExpectation(
        double bytesPerSecond,
        int windowSeconds,
        long capBytes,
        long expected)
    {
        Assert.Equal(
            expected,
            MultiSegmentStream.ComputeReadAheadWindowBytes(bytesPerSecond, windowSeconds, capBytes));
    }

    [Fact]
    public async Task Ceiling_ExtendsWithDeliveryRate_WhenReadAheadRepairEnabled()
    {
        const int estimate = 100_000;
        var segments = new Dictionary<string, byte[]>
        {
            ["seg-0"] = new byte[estimate],
            ["seg-1"] = new byte[estimate],
        };
        var client = new FakeNntpClient(segments, useCachedYencStreams: true);
        var budget = new InFlightArticleBudget(4L * 1024 * 1024);
        RangeContext.SetReadAheadRepair(new RangeContext.ReadAheadRepairOptions(true, 60));
        try
        {
            await using var stream = (MultiSegmentStream)MultiSegmentStream.Create(
                segments.Keys.OrderBy(k => k, StringComparer.Ordinal).ToArray().AsMemory(),
                client,
                articleBufferSize: 2,
                estimatedSegmentSize: estimate,
                failFastOnFirstSegment: false,
                usePipelinedBodyRequests: false,
                CancellationToken.None,
                fileName: "readahead.bin",
                inFlightArticleBudget: budget);

            // No delivery rate yet: the stock prefetch ceiling applies unchanged.
            var baseCeiling = stream.CurrentPrefetchByteCeiling;
            Assert.True(baseCeiling > 0);

            // 1 MB/s × 60 s = 60 MB, capped by the 4 MB article budget.
            stream.SetDeliveryRateForTests(1_000_000);
            Assert.Equal(budget.CapBytes, stream.CurrentPrefetchByteCeiling);

            // 1 kB/s × 60 s = 60 kB, below the stock ceiling: no change.
            stream.SetDeliveryRateForTests(1_000);
            Assert.Equal(baseCeiling, stream.CurrentPrefetchByteCeiling);
        }
        finally
        {
            RangeContext.SetReadAheadRepair(null);
        }
    }

    [Fact]
    public async Task Ceiling_IgnoresDeliveryRate_WhenReadAheadRepairDisabled()
    {
        const int estimate = 100_000;
        var segments = new Dictionary<string, byte[]>
        {
            ["seg-0"] = new byte[estimate],
            ["seg-1"] = new byte[estimate],
        };
        var client = new FakeNntpClient(segments, useCachedYencStreams: true);
        RangeContext.SetReadAheadRepair(null);
        await using var stream = (MultiSegmentStream)MultiSegmentStream.Create(
            segments.Keys.OrderBy(k => k, StringComparer.Ordinal).ToArray().AsMemory(),
            client,
            articleBufferSize: 2,
            estimatedSegmentSize: estimate,
            failFastOnFirstSegment: false,
            usePipelinedBodyRequests: false,
            CancellationToken.None,
            fileName: "stock.bin",
            inFlightArticleBudget: new InFlightArticleBudget(4L * 1024 * 1024));

        var baseCeiling = stream.CurrentPrefetchByteCeiling;
        stream.SetDeliveryRateForTests(1_000_000);
        Assert.Equal(baseCeiling, stream.CurrentPrefetchByteCeiling);
    }
}
