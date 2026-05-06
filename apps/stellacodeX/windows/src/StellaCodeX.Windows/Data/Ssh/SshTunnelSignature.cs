using StellaCodeX.Windows.Domain.Model;

namespace StellaCodeX.Windows.Data.Ssh;

public sealed record SshTunnelSignature(
    string SshHost,
    int? SshPort,
    string SshUser,
    Uri TargetUrl)
{
    public static SshTunnelSignature FromProfile(ServerProfile profile) => new(
        SshHost: profile.SshHost.Trim(),
        SshPort: profile.SshPort,
        SshUser: profile.SshUser.Trim(),
        TargetUrl: profile.EffectiveTargetUrl);

    public string CacheKey => string.Join("|", [
        SshHost,
        SshPort?.ToString() ?? string.Empty,
        SshUser,
        TargetUrl.AbsoluteUri.TrimEnd('/'),
    ]);
}
