using System.Text.Json;

namespace StellaCodeX.Windows.Core.Storage;

public sealed class JsonFileStore<T>(string filePath, JsonSerializerOptions? jsonOptions = null)
{
    private readonly JsonSerializerOptions _jsonOptions = jsonOptions ?? new JsonSerializerOptions
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        WriteIndented = true,
    };

    public async Task<T?> LoadAsync(CancellationToken cancellationToken = default)
    {
        if (!File.Exists(filePath))
        {
            return default;
        }

        await using var stream = File.OpenRead(filePath);
        return await JsonSerializer.DeserializeAsync<T>(stream, _jsonOptions, cancellationToken);
    }

    public async Task SaveAsync(T value, CancellationToken cancellationToken = default)
    {
        var directory = Path.GetDirectoryName(filePath);
        if (!string.IsNullOrWhiteSpace(directory))
        {
            Directory.CreateDirectory(directory);
        }

        await using var stream = File.Create(filePath);
        await JsonSerializer.SerializeAsync(stream, value, _jsonOptions, cancellationToken);
        await stream.FlushAsync(cancellationToken);
    }
}
