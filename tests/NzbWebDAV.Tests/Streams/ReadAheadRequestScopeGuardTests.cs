using NzbWebDAV.Streams;
using NzbWebDAV.Tests.Fakes;
using NzbWebDAV.WebDav.Requests;

namespace NzbWebDAV.Tests.Streams;

// Fork (read-ahead repair): request-scope guard for the Watchdog play flow.
// Both serving entry points (the WebDAV GET/HEAD handler and /view GET) publish
// the opt-in read-ahead options on their request's async context. Playback-
// selection candidates never run inside such a request: pre-verify probes talk
// NNTP directly and candidate downloads run in the queue, as sibling flows of
// the play click. These tests pin that a request-scoped publish can never reach
// those sibling flows, so repair.readahead.enabled can only ever widen the
// prefetch of a stream the entry point itself is serving to the player.
public sealed class ReadAheadRequestScopeGuardTests
{
    [Fact]
    public async Task RequestScopedPublish_IsInvisibleToSiblingFlows()
    {
        RangeContext.SetReadAheadRepair(null);
        try
        {
            Assert.Null(RangeContext.GetReadAheadRepair());

            // Simulated HTTP entry point: it publishes inside its own execution flow.
            var publishedInRequest = false;
            await Task.Run(() =>
            {
                RangeContext.SetReadAheadRepair(new RangeContext.ReadAheadRepairOptions(true, 60));
                publishedInRequest =
                    RangeContext.GetReadAheadRepair() is { Enabled: true, WindowSeconds: 60 };
            }).ConfigureAwait(false);

            // The request flow observed its own publish...
            Assert.True(publishedInRequest);

            // ...but the sibling context (queue download / NNTP pre-verify) never does.
            Assert.Null(RangeContext.GetReadAheadRepair());
        }
        finally
        {
            RangeContext.SetReadAheadRepair(null);
        }
    }

    [Fact]
    public async Task StreamInSiblingFlow_KeepsStockCeilingDespiteRequestPublish()
    {
        const int estimate = 100_000;
        var segments = new Dictionary<string, byte[]>
        {
            ["seg-0"] = new byte[estimate],
            ["seg-1"] = new byte[estimate],
        };
        var client = new FakeNntpClient(segments, useCachedYencStreams: true);

        RangeContext.SetReadAheadRepair(null);
        try
        {
            // A request flow somewhere else in the process published the options.
            await Task.Run(() =>
                RangeContext.SetReadAheadRepair(
                    new RangeContext.ReadAheadRepairOptions(true, 60))).ConfigureAwait(false);

            // Candidate context: this flow never passed through a serving entry
            // point, so it sees no options and the stream keeps its stock prefetch
            // ceiling even at a high delivery rate.
            Assert.Null(RangeContext.GetReadAheadRepair());
            await using var stream = (MultiSegmentStream)MultiSegmentStream.Create(
                segments.Keys.OrderBy(k => k, StringComparer.Ordinal).ToArray().AsMemory(),
                client,
                articleBufferSize: 2,
                estimatedSegmentSize: estimate,
                failFastOnFirstSegment: false,
                usePipelinedBodyRequests: false,
                CancellationToken.None,
                fileName: "candidate.bin",
                inFlightArticleBudget: new InFlightArticleBudget(4L * 1024 * 1024));

            var baseCeiling = stream.CurrentPrefetchByteCeiling;
            Assert.True(baseCeiling > 0);
            stream.SetDeliveryRateForTests(1_000_000);
            Assert.Equal(baseCeiling, stream.CurrentPrefetchByteCeiling);
        }
        finally
        {
            RangeContext.SetReadAheadRepair(null);
        }
    }
}
