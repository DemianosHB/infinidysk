namespace NzbWebDAV.Api.Controllers.TestOutboundProxy;

public class TestOutboundProxyResponse : BaseApiResponse
{
    public bool Connected { get; set; }

    /// <summary>Short human-readable reason when the proxy could not be verified.</summary>
    public string? Reason { get; set; }
}
