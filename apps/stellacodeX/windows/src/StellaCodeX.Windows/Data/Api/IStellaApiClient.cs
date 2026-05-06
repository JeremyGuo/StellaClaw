namespace StellaCodeX.Windows.Data.Api;

public sealed record ServerConnectionInfo(Uri BaseUrl, string Token);

public interface IStellaApiClient
{
    Task<string> GetModelsJsonAsync(CancellationToken cancellationToken = default);

    Task<string> GetConversationsJsonAsync(int limit = 80, int? offset = null, CancellationToken cancellationToken = default);

    Task<string> GetMessagesJsonAsync(string conversationId, int offset, int limit, CancellationToken cancellationToken = default);
}
