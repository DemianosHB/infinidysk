namespace NzbWebDAV.WebDav.Requests;

/// <summary>
/// Carries an optional byte budget for the current ranged read on the async
/// call context so stream constructors can cap segment prefetch.
/// </summary>
public static class RangeContext
{
    private static readonly AsyncLocal<long?> CurrentReadBudget = new();
    private static readonly AsyncLocal<ReadAheadRepairOptions?> CurrentReadAheadRepair = new();

    public static void SetReadBudget(long? readBudgetBytes) =>
        CurrentReadBudget.Value = readBudgetBytes;

    public static long? GetReadBudget() => CurrentReadBudget.Value;

    public static void SetReadAheadRepair(ReadAheadRepairOptions? options) =>
        CurrentReadAheadRepair.Value = options;

    public static ReadAheadRepairOptions? GetReadAheadRepair() => CurrentReadAheadRepair.Value;

    /// <summary>
    /// Fork (read-ahead repair): opt-in settings that let a stream prefetch far enough
    /// ahead of its read head that holes are discovered and repaired before the reader
    /// reaches them.
    /// </summary>
    public sealed record ReadAheadRepairOptions(bool Enabled, int WindowSeconds);
}
