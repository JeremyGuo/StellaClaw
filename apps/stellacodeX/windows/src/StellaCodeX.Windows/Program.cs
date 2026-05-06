using StellaCodeX.Windows.Data.Api;
using StellaCodeX.Windows.Domain.Model;

namespace StellaCodeX.Windows;

internal static class Program
{
    private static async Task<int> Main(string[] args)
    {
        if (args.Contains("--help") || args.Contains("-h"))
        {
            PrintHelp();
            return 0;
        }

        var baseUrl = ReadOption(args, "--base-url") ?? "http://127.0.0.1:3111";
        var token = ReadOption(args, "--token") ?? "local-web-token";

        if (!Uri.TryCreate(baseUrl.TrimEnd('/'), UriKind.Absolute, out var uri))
        {
            Console.Error.WriteLine($"Invalid --base-url: {baseUrl}");
            return 2;
        }

        var profile = ServerProfile.LocalDefault() with
        {
            BaseUrl = uri,
            TargetUrl = uri,
            Token = token,
        };

        Console.WriteLine("StellacodeX Windows bootstrap");
        Console.WriteLine($"Connection: {profile.ConnectionSummary}");

        if (args.Contains("--models"))
        {
            return await FetchModelsAsync(profile);
        }

        Console.WriteLine("No UI shell is wired yet. Use --models to smoke-test the Web channel connection.");
        return 0;
    }

    private static async Task<int> FetchModelsAsync(ServerProfile profile)
    {
        using var http = new HttpClient { BaseAddress = profile.BaseUrl };
        http.DefaultRequestHeaders.Accept.ParseAdd("application/json");
        http.DefaultRequestHeaders.Authorization = new System.Net.Http.Headers.AuthenticationHeaderValue("Bearer", profile.Token);

        try
        {
            using var response = await http.GetAsync(StellaApiPaths.Models());
            var body = await response.Content.ReadAsStringAsync();
            if (!response.IsSuccessStatusCode)
            {
                Console.Error.WriteLine($"GET {StellaApiPaths.Models()} failed: {(int)response.StatusCode} {response.ReasonPhrase}");
                Console.Error.WriteLine(body);
                return 1;
            }

            Console.WriteLine(body);
            return 0;
        }
        catch (Exception error)
        {
            Console.Error.WriteLine($"GET {StellaApiPaths.Models()} failed: {error.Message}");
            return 1;
        }
    }

    private static string? ReadOption(IReadOnlyList<string> args, string name)
    {
        for (var index = 0; index < args.Count - 1; index++)
        {
            if (args[index] == name)
            {
                return args[index + 1];
            }
        }
        return null;
    }

    private static void PrintHelp()
    {
        Console.WriteLine("StellacodeX Windows bootstrap");
        Console.WriteLine();
        Console.WriteLine("Options:");
        Console.WriteLine("  --base-url <url>   Stellaclaw Web channel base URL. Default: http://127.0.0.1:3111");
        Console.WriteLine("  --token <token>    Bearer token. Default: local-web-token");
        Console.WriteLine("  --models           GET /api/models and print the JSON response.");
    }
}
