using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

internal sealed record NearbyDevice(string Id, string Name, string Platform, ushort Protocol, bool Compatible, IReadOnlyList<string> Addresses);
internal sealed record DiscoveryResult(IReadOnlyList<NearbyDevice> Devices, bool CapacityLimited);

/// Discovery-only adapter; all process ownership lives in the shared bounded worker.
internal sealed class DiscoveryClient
{
    internal const int MaxOutputBytes = BoundedCoreCommand<DiscoveryResult>.MaxOutputBytes;
    private readonly BoundedCoreCommand<DiscoveryResult> command;
    internal DiscoveryClient() => command = new(["discover", "--pairing", "--seconds", "3"], Parse);
    internal DiscoveryClient(Func<ProcessStartInfo> fixture, TimeSpan timeout, Func<Stream, Stream>? fixtureOutput = null)
        => command = new(["discover", "--pairing", "--seconds", "3"], Parse, fixture, timeout, fixtureOutput);
    internal Task<DiscoveryResult> ScanAsync() => command.RunAsync();
    internal void Cancel() => command.Cancel();
    internal Task<bool> StopAsync() => command.StopAsync();
    internal static DiscoveryResult Parse(byte[] bytes)
    {
        if (bytes.Length > MaxOutputBytes) throw InvalidResult();
        try
        {
            // Strict decoding rejects invalid UTF-8 rather than replacing characters.
            string text = new UTF8Encoding(false, true).GetString(bytes);
            using var document = JsonDocument.Parse(text, new JsonDocumentOptions { MaxDepth = 8 });
            var root = document.RootElement;
            Fields(root, "state", "candidates", "capacity_limited", "error");
            string state = String(root, "state", 16);
            var error = root.GetProperty("error");
            if (error.ValueKind != JsonValueKind.Null && (error.ValueKind != JsonValueKind.String || (error.GetString()?.Length ?? 0) > 512)) throw InvalidResult();
            if (state != "browsing" || error.ValueKind != JsonValueKind.Null) throw new InvalidOperationException("发现服务尚未就绪，请检查网络后刷新。");
            bool capacity = root.GetProperty("capacity_limited").GetBoolean();
            var candidates = root.GetProperty("candidates");
            if (candidates.ValueKind != JsonValueKind.Array || candidates.GetArrayLength() > 64) throw InvalidResult();
            var result = new List<NearbyDevice>();
            var ids = new HashSet<string>(StringComparer.Ordinal);
            foreach (var candidate in candidates.EnumerateArray())
            {
                Fields(candidate, "device_id", "friendly_name", "platform", "protocol", "compatible", "trusted", "addresses");
                string id = String(candidate, "device_id", 32);
                if (id.Length != 32 || id.Any(c => !(c is >= '0' and <= '9' or >= 'a' and <= 'f')) || !ids.Add(id)) throw InvalidResult();
                string name = String(candidate, "friendly_name", 96);
                if (string.IsNullOrWhiteSpace(name) || name.Any(c => char.IsControl(c) || c is '\u061c' or '\u200e' or '\u200f' or >= '\u202a' and <= '\u202e' or >= '\u2066' and <= '\u2069')) throw InvalidResult();
                string platform = String(candidate, "platform", 16);
                if (platform is not ("windows" or "macos" or "linux" or "ios" or "ipados")) throw InvalidResult();
                ushort protocol = candidate.GetProperty("protocol").GetUInt16();
                if (protocol == 0 || candidate.GetProperty("trusted").GetBoolean()) throw InvalidResult();
                bool compatible = candidate.GetProperty("compatible").GetBoolean();
                var addresses = candidate.GetProperty("addresses");
                if (addresses.ValueKind != JsonValueKind.Array || addresses.GetArrayLength() is < 1 or > 32) throw InvalidResult();
                var normalized = new HashSet<string>(StringComparer.Ordinal);
                foreach (var address in addresses.EnumerateArray())
                {
                    if (address.ValueKind != JsonValueKind.String || address.GetString() is not string value || value.Length > 96 || !IPEndPoint.TryParse(value, out var endpoint) || endpoint.Port == 0) throw InvalidResult();
                    var ip = endpoint.Address;
                    if (IPAddress.IsLoopback(ip) || ip.Equals(IPAddress.Any) || ip.Equals(IPAddress.IPv6Any) || ip.Equals(IPAddress.Broadcast) || ip.IsIPv4MappedToIPv6 || ip.IsIPv6Multicast || (ip.IsIPv6LinkLocal && ip.ScopeId == 0)) throw InvalidResult();
                    if (ip.AddressFamily == AddressFamily.InterNetwork && (ip.GetAddressBytes()[0] == 0 || ip.GetAddressBytes()[0] >= 224)) throw InvalidResult();
                    if (!normalized.Add(endpoint.ToString())) throw InvalidResult();
                }
                result.Add(new(id, name, platform, protocol, compatible, normalized.ToArray()));
            }
            return new(result, capacity);
        }
        catch (Exception error) when (error is JsonException or DecoderFallbackException or InvalidOperationException or FormatException or OverflowException or KeyNotFoundException)
        { throw InvalidResult(); }
    }

    private static string String(JsonElement parent, string key, int maxBytes)
    {
        var value = parent.GetProperty(key);
        if (value.ValueKind != JsonValueKind.String || value.GetString() is not string text || Encoding.UTF8.GetByteCount(text) > maxBytes) throw InvalidResult();
        return text;
    }
    private static void Fields(JsonElement element, params string[] names)
    {
        if (element.ValueKind != JsonValueKind.Object) throw InvalidResult();
        var actual = element.EnumerateObject().Select(p => p.Name).ToArray();
        if (actual.Length != names.Length || actual.Distinct(StringComparer.Ordinal).Count() != names.Length || names.Except(actual, StringComparer.Ordinal).Any()) throw InvalidResult();
    }
    private static InvalidOperationException InvalidResult() => new("发现结果无效，请更新客户端后重试。");
}
