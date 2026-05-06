# StellacodeX Windows Development

## Environment split

A Linux server can do useful work for this client, but it cannot fully validate WinUI:

- Suitable on Linux/server:
  - edit docs and protocol notes;
  - implement and review platform-neutral C# domain/API code;
  - run Stellaclaw as a Web channel test backend;
  - maintain JSON fixtures and endpoint compatibility docs.
- Requires Windows:
  - WinUI 3 / Windows App SDK project creation and build;
  - window rendering and input behavior;
  - Windows Credential Manager / DPAPI integration;
  - Windows notification integration;
  - `ssh.exe` tunnel behavior on Windows;
  - portable self-contained publish and zip verification.

The current server environment was checked with `dotnet --info`; `dotnet` is not installed here. Build verification therefore needs a Windows machine/VM/runner with .NET 8 and Windows App SDK tooling.

## Recommended Windows setup

1. Install Visual Studio 2022 with:
   - .NET desktop development;
   - Windows App SDK / WinUI workload;
   - Windows 10/11 SDK.
2. Install .NET 8 SDK if Visual Studio did not install it.
3. Ensure Windows OpenSSH Client is available:
   - `ssh.exe -V`
4. Run the Stellaclaw backend separately and expose the Web channel, for example `http://127.0.0.1:3111` with a test token.

## Portable package target

Once a runnable WinUI app project exists, package as an unpackaged/self-contained publish directory and zip it. The intended shape is:

```powershell
# exact project path may change when the WinUI shell is added
 dotnet publish .\src\StellaCodeX.Windows\StellaCodeX.Windows.csproj `
  -c Release `
  -r win-x64 `
  --self-contained true `
  -p:WindowsPackageType=None `
  -o .\dist\StellacodeX-Windows-win-x64

Compress-Archive `
  -Path .\dist\StellacodeX-Windows-win-x64\* `
  -DestinationPath .\dist\StellacodeX-Windows-win-x64.zip `
  -Force
```

Do not add MSIX, installer, or automatic update flow until the portable zip path is stable.

## First verification slice

The first Windows runnable slice should prove:

1. profile settings can store base URL, token, and username;
2. `/api/models` succeeds against a local or SSH-proxied backend;
3. errors are visible in the UI;
4. portable publish output launches after extraction.
