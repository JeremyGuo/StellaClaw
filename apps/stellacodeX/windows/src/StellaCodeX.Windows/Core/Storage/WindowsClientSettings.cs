using StellaCodeX.Windows.Domain.Model;

namespace StellaCodeX.Windows.Core.Storage;

public sealed record WindowsClientSettings(
    Guid ActiveServerId,
    IReadOnlyList<ServerProfile> Servers,
    string ThemeMode,
    int DisplayFontSize,
    double UiScale)
{
    public static WindowsClientSettings Default()
    {
        var local = ServerProfile.LocalDefault();
        return new WindowsClientSettings(
            ActiveServerId: local.Id,
            Servers: [local],
            ThemeMode: "system",
            DisplayFontSize: 12,
            UiScale: 1.0);
    }
}
