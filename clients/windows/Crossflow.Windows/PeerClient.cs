using System.Diagnostics;
using System.Net;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

internal sealed record SavedPeer(string Id, string Name, string LastEndpoint);
internal sealed record PeerListResult(IReadOnlyList<SavedPeer> Peers, uint InvalidRecords);

/// Public summaries only. Rust validates/decrypts the saved identities and owns their paths.
internal sealed class PeerClient
{
    private readonly BoundedCoreCommand<PeerListResult> command;
    internal PeerClient() => command = new(["peers"], Parse);
    internal PeerClient(Func<ProcessStartInfo> fixture, TimeSpan timeout) => command = new(["peers"], Parse, fixture, timeout);
    internal Task<PeerListResult> ReadAsync() => command.RunAsync();
    internal void Cancel() => command.Cancel();
    internal Task<bool> StopAsync() => command.StopAsync();

    internal static PeerListResult Parse(byte[] bytes)
    {
        if (bytes.Length > BoundedCoreCommand<PeerListResult>.MaxOutputBytes) throw Invalid();
        try
        {
            using var document = JsonDocument.Parse(new UTF8Encoding(false, true).GetString(bytes), new JsonDocumentOptions { MaxDepth = 6 });
            var root = document.RootElement;
            Fields(root, "peers", "invalid_records");
            uint invalid = root.GetProperty("invalid_records").GetUInt32();
            var peers = root.GetProperty("peers");
            if (peers.ValueKind != JsonValueKind.Array || peers.GetArrayLength() > 64) throw Invalid();
            if (invalid > 64 || invalid + (uint)peers.GetArrayLength() > 64) throw Invalid();
            var result = new List<SavedPeer>();
            var ids = new HashSet<string>(StringComparer.Ordinal);
            foreach (var peer in peers.EnumerateArray())
            {
                Fields(peer, "schema", "id", "friendly_name", "last_endpoint");
                if (peer.GetProperty("schema").GetInt32() != 1) throw Invalid();
                string id = String(peer, "id", 64);
                if (id.Length != 64 || id.Any(c => !(c is >= '0' and <= '9' or >= 'a' and <= 'f')) || !ids.Add(id)) throw Invalid();
                string name = String(peer, "friendly_name", 256);
                if (string.IsNullOrWhiteSpace(name) || name.Trim() != name || name.EnumerateRunes().Count() > 64 || name.Any(c => char.IsControl(c) || c is '\u061c' or '\u200e' or '\u200f' or >= '\u2028' and <= '\u202e' or >= '\u2066' and <= '\u206f' or '\ufeff')) throw Invalid();
                string address = String(peer, "last_endpoint", 96);
                if (!IPEndPoint.TryParse(address, out var endpoint) || endpoint.Port == 0 || endpoint.Address.Equals(IPAddress.Any) || endpoint.Address.Equals(IPAddress.IPv6Any)) throw Invalid();
                var ip = endpoint.Address.IsIPv4MappedToIPv6 ? endpoint.Address.MapToIPv4() : endpoint.Address;
                if (ip.Equals(IPAddress.Any) || ip.Equals(IPAddress.Broadcast) || ip.IsIPv6Multicast || (ip.AddressFamily == System.Net.Sockets.AddressFamily.InterNetwork && ip.GetAddressBytes()[0] is >= 224 and <= 239)) throw Invalid();
                // This is an advisory last address, not a current-online assertion.
                result.Add(new(id, name, endpoint.ToString()));
            }
            return new(result, invalid);
        }
        catch (Exception error) when (error is JsonException or DecoderFallbackException or InvalidOperationException or FormatException or OverflowException or KeyNotFoundException)
        { throw Invalid(); }
    }

    private static void Fields(JsonElement element, params string[] names)
    {
        if (element.ValueKind != JsonValueKind.Object) throw Invalid();
        var actual = element.EnumerateObject().Select(p => p.Name).ToArray();
        if (actual.Length != names.Length || actual.Distinct(StringComparer.Ordinal).Count() != names.Length || names.Except(actual, StringComparer.Ordinal).Any()) throw Invalid();
    }
    private static string String(JsonElement parent, string key, int maxBytes)
    {
        var value = parent.GetProperty(key);
        if (value.ValueKind != JsonValueKind.String || value.GetString() is not string text || Encoding.UTF8.GetByteCount(text) > maxBytes) throw Invalid();
        return text;
    }
    private static InvalidOperationException Invalid() => new("已配对设备列表无效，无法确认保存状态。请更新客户端后重试。");
}
