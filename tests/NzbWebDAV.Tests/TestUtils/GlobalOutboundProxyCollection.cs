namespace NzbWebDAV.Tests.TestUtils;

/// <summary>
/// Tests that read or mutate the process-wide <c>OutboundProxy</c> static state
/// belong to this collection. xUnit runs collections in parallel, so without it
/// two such classes race on the same static and observe each other's proxy settings.
/// </summary>
[CollectionDefinition(nameof(GlobalOutboundProxyCollection), DisableParallelization = true)]
public class GlobalOutboundProxyCollection;
