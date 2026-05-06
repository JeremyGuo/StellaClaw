namespace StellaCodeX.Windows.Domain.Model;

public sealed record ServerProfile(
    Guid Id,
    string Name,
    ConnectionMode ConnectionMode,
    Uri BaseUrl,
    Uri TargetUrl,
    string SshHost,
    int? SshPort,
    string SshUser,
    string Token,
    string UserName)
{
    public Uri EffectiveTargetUrl => TargetUrl.AbsoluteUri.Length > 0 ? TargetUrl : BaseUrl;

    public bool IsConfigured => !string.IsNullOrWhiteSpace(Token) && ConnectionMode switch
    {
        ConnectionMode.Direct => IsUsableHttpUrl(BaseUrl),
        ConnectionMode.SshProxy => IsUsableHttpUrl(EffectiveTargetUrl) && !string.IsNullOrWhiteSpace(SshHost),
        _ => false,
    };

    public string ConnectionSummary => ConnectionMode switch
    {
        ConnectionMode.SshProxy => BuildSshSummary(),
        _ => TrimTrailingSlash(BaseUrl),
    };

    public static ServerProfile LocalDefault() => new(
        Id: Guid.NewGuid(),
        Name: "Local Stellaclaw",
        ConnectionMode: ConnectionMode.Direct,
        BaseUrl: new Uri("http://127.0.0.1:3111"),
        TargetUrl: new Uri("http://127.0.0.1:3111"),
        SshHost: string.Empty,
        SshPort: null,
        SshUser: string.Empty,
        Token: "local-web-token",
        UserName: "workspace-user");

    private string BuildSshSummary()
    {
        var host = string.IsNullOrWhiteSpace(SshHost) ? "missing SSH host" : SshHost.Trim();
        var userPrefix = string.IsNullOrWhiteSpace(SshUser) ? string.Empty : $"{SshUser.Trim()}@";
        var portSuffix = SshPort is > 0 ? $":{SshPort}" : string.Empty;
        return $"{userPrefix}{host}{portSuffix} -> {TrimTrailingSlash(EffectiveTargetUrl)}";
    }

    private static bool IsUsableHttpUrl(Uri uri) => uri.Scheme is "http" or "https";

    private static string TrimTrailingSlash(Uri uri) => uri.AbsoluteUri.TrimEnd('/');
}
