param(
    [string]$Runtime = "win-x64",
    [string]$Configuration = "Release"
)

$ErrorActionPreference = "Stop"

$Root = Resolve-Path (Join-Path $PSScriptRoot "..")
$Project = Join-Path $Root "src\StellaCodeX.Windows\StellaCodeX.Windows.csproj"
$Dist = Join-Path $Root "dist"
$PublishDir = Join-Path $Dist "StellacodeX-Windows-$Runtime"
$ZipPath = Join-Path $Dist "StellacodeX-Windows-$Runtime.zip"

if (!(Get-Command dotnet -ErrorAction SilentlyContinue)) {
    throw "dotnet SDK is required. Install .NET 8 SDK on Windows before packaging."
}

Remove-Item -LiteralPath $PublishDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Dist | Out-Null

dotnet publish $Project `
    -c $Configuration `
    -r $Runtime `
    --self-contained true `
    -p:WindowsPackageType=None `
    -o $PublishDir

Remove-Item -LiteralPath $ZipPath -Force -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $PublishDir "*") -DestinationPath $ZipPath -Force

Write-Host "Portable package: $ZipPath"
