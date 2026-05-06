namespace StellaCodeX.Windows.Data.Api;

public static class StellaApiPaths
{
    public static string Models() => "/api/models";

    public static string Conversations(int limit = 80, int? offset = null)
    {
        var query = offset is null
            ? $"limit={limit}"
            : $"offset={offset.Value}&limit={limit}";
        return $"/api/conversations?{query}";
    }

    public static string Conversation(string conversationId) => $"/api/conversations/{EscapeSegment(conversationId)}";

    public static string ConversationSeen(string conversationId) => $"{Conversation(conversationId)}/seen";

    public static string ConversationStatus(string conversationId) => $"{Conversation(conversationId)}/status";

    public static string Messages(string conversationId, int offset, int limit) =>
        $"{Conversation(conversationId)}/messages?offset={offset}&limit={limit}";

    public static string MessageDetail(string conversationId, string messageId) =>
        $"{Conversation(conversationId)}/messages/{EscapeSegment(messageId)}";

    public static string ConversationStream() => "/api/conversations/stream";

    public static string ForegroundWebSocket(string conversationId, string token) =>
        $"{Conversation(conversationId)}/foreground/ws?token={Uri.EscapeDataString(token)}";

    public static string WorkspaceList(string conversationId, string path = "", int? limit = null)
    {
        var query = $"path={Uri.EscapeDataString(path)}";
        if (limit is > 0)
        {
            query += $"&limit={limit.Value}";
        }
        return $"{Conversation(conversationId)}/workspace?{query}";
    }

    public static string WorkspaceFile(string conversationId, string path, long offset = 0, long? limitBytes = null)
    {
        var query = $"path={Uri.EscapeDataString(path)}&offset={offset}";
        if (limitBytes is > 0)
        {
            query += $"&limit_bytes={limitBytes.Value}";
        }
        return $"{Conversation(conversationId)}/workspace/file?{query}";
    }

    public static string WorkspaceDownload(string conversationId, string path) =>
        $"{Conversation(conversationId)}/workspace/download?path={Uri.EscapeDataString(path)}";

    public static string WorkspaceUpload(string conversationId, string path) =>
        $"{Conversation(conversationId)}/workspace/upload?path={Uri.EscapeDataString(path)}";

    public static string Terminals(string conversationId) => $"{Conversation(conversationId)}/terminals";

    public static string Terminal(string conversationId, string terminalId) =>
        $"{Terminals(conversationId)}/{EscapeSegment(terminalId)}";

    public static string TerminalStream(string conversationId, string terminalId, string token) =>
        $"{Terminal(conversationId, terminalId)}/stream?token={Uri.EscapeDataString(token)}";

    private static string EscapeSegment(string value) => Uri.EscapeDataString(value);
}
