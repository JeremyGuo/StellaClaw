namespace StellaCodeX.Windows.Domain.Model;

public enum ConnectionMode
{
    Direct,
    SshProxy,
}

public static class ConnectionModeWire
{
    public const string Direct = "direct";
    public const string SshProxy = "ssh_proxy";

    public static string ToWireName(this ConnectionMode mode) => mode switch
    {
        ConnectionMode.SshProxy => SshProxy,
        _ => Direct,
    };

    public static ConnectionMode FromWireName(string? value) => value == SshProxy
        ? ConnectionMode.SshProxy
        : ConnectionMode.Direct;
}
