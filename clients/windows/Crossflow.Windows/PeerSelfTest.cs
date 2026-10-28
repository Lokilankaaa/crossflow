using System.Diagnostics;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

/// Explicit test-only CLI children; no real peers, network, or input are accessed.
internal static class PeerSelfTest
{
    private static readonly string Id = new('a', 64);
    private static string Valid => JsonSerializer.Serialize(new { peers = new[] { new { schema = 1, id = Id, friendly_name = "我的 Mac", last_endpoint = "[fe80::123%7]:24800" } }, invalid_records = 2 });
    internal static async Task FixtureAsync(string mode)
    {
        Console.OutputEncoding = new UTF8Encoding(false);
        if (mode == "silent") { await Task.Delay(TimeSpan.FromMinutes(1)); return; }
        if (mode == "stderr") { await Console.Error.WriteAsync(new string('e', 131072)); await Console.Error.FlushAsync(); }
        await Console.Out.WriteAsync(mode == "invalid" ? "{}" : Valid);
        await Console.Out.FlushAsync();
        if (mode == "nonzero") Environment.ExitCode = 1;
    }
    private static PeerClient Client(string mode, int milliseconds = 3000) => new(() =>
    {
        var info = new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, "Crossflow.Windows.exe"));
        info.ArgumentList.Add("--peer-fixture"); info.ArgumentList.Add(mode);
        return info;
    }, TimeSpan.FromMilliseconds(milliseconds));

    internal static async Task RunAsync(string path)
    {
        var checks = new Dictionary<string, bool>();
        try
        {
            var parsed = PeerClient.Parse(Encoding.UTF8.GetBytes(Valid));
            checks["saved_id_name_scope"] = parsed.Peers.Count == 1 && parsed.Peers[0].Id == Id && parsed.Peers[0].Name == "我的 Mac" && parsed.Peers[0].LastEndpoint == "[fe80::123%7]:24800";
            checks["invalid_records_preserved"] = parsed.InvalidRecords == 2;
            var empty = PeerClient.Parse(Encoding.UTF8.GetBytes("{\"peers\":[],\"invalid_records\":1}"));
            checks["invalid_only_not_empty_store"] = empty.Peers.Count == 0 && empty.InvalidRecords == 1;
            checks["discovery_id_rejected"] = Reject(Valid.Replace(Id, new string('a', 32)));
            checks["uppercase_or_path_id_rejected"] = Reject(Valid.Replace(Id, Id.ToUpperInvariant())) && Reject(Valid.Replace(Id, "../identity"));
            checks["schema_rejected"] = Reject(Valid.Replace("\"schema\":1", "\"schema\":2"));
            checks["bad_endpoint_rejected"] = Reject(Valid.Replace("[fe80::123%7]:24800", "example.com:24800")) && Reject(Valid.Replace("[fe80::123%7]:24800", "0.0.0.0:24800"));
            checks["multicast_rejected"] = Reject(Valid.Replace("[fe80::123%7]:24800", "239.1.1.1:24800"));
            checks["duplicate_field_rejected"] = Reject(Valid.Replace("\"invalid_records\":2", "\"invalid_records\":2,\"invalid_records\":0"));
            checks["negative_invalid_count_rejected"] = Reject(Valid.Replace("\"invalid_records\":2", "\"invalid_records\":-1"));
            checks["invalid_count_cap_rejected"] = Reject(Valid.Replace("\"invalid_records\":2", "\"invalid_records\":65"));
            checks["total_record_cap_rejected"] = Reject(Valid.Replace("\"invalid_records\":2", "\"invalid_records\":64"));
            using var document = JsonDocument.Parse(Valid);
            var peer = document.RootElement.GetProperty("peers")[0];
            checks["duplicate_id_rejected"] = Reject(JsonSerializer.Serialize(new { peers = new[] { peer, peer }, invalid_records = 0 }));
            checks["peer_cap_rejected"] = Reject(JsonSerializer.Serialize(new { peers = Enumerable.Repeat(peer, 65), invalid_records = 0 }));
            checks["unicode_name_boundary"] = PeerClient.Parse(Encoding.UTF8.GetBytes(JsonSerializer.Serialize(new { peers = new[] { new { schema = 1, id = Id, friendly_name = new string('屏', 64), last_endpoint = "127.0.0.1:24800" } }, invalid_records = 0 }))).Peers.Count == 1;
            checks["control_name_rejected"] = Reject(JsonSerializer.Serialize(new { peers = new[] { new { schema = 1, id = Id, friendly_name = "\u2028hidden", last_endpoint = "127.0.0.1:24800" } }, invalid_records = 0 }));
            try { PeerClient.Parse([0xff]); checks["invalid_utf8_rejected"] = false; } catch (InvalidOperationException) { checks["invalid_utf8_rejected"] = true; }
            foreach (string mode in new[] { "valid", "stderr", "invalid", "nonzero", "silent" })
            {
                var client = Client(mode, mode == "silent" ? 900 : 3000);
                var timer = Stopwatch.StartNew();
                try { var result = await client.ReadAsync(); checks[mode + "_outcome"] = mode is "valid" or "stderr" && result.InvalidRecords == 2; }
                catch (InvalidOperationException) { checks[mode + "_outcome"] = mode is "invalid" or "nonzero" or "silent"; }
                finally { checks[mode + "_reaped"] = await client.StopAsync(); }
                checks[mode + "_bounded"] = timer.Elapsed < TimeSpan.FromSeconds(7);
            }
            var cancelled = Client("silent");
            var task = cancelled.ReadAsync(); await Task.Delay(100);
            var stopping = cancelled.StopAsync();
            try { await task; checks["page_leave_cancelled"] = false; } catch (OperationCanceledException) { checks["page_leave_cancelled"] = true; }
            checks["page_leave_reaped"] = await stopping;
            checks["concurrent_stop_idempotent"] = (await Task.WhenAll(cancelled.StopAsync(), cancelled.StopAsync())).All(v => v);
            File.WriteAllText(path, JsonSerializer.Serialize(new { ok = checks.Values.All(v => v), actual_os_input = false, actual_peer_store = false, checks }, CoreBridge.Json));
            Environment.ExitCode = checks.Values.All(v => v) ? 0 : 1;
        }
        catch (Exception error)
        {
            File.WriteAllText(path, JsonSerializer.Serialize(new { ok = false, actual_os_input = false, actual_peer_store = false, checks, error = error.Message }, CoreBridge.Json));
            Environment.ExitCode = 1;
        }
    }
    private static bool Reject(string json)
    {
        try { PeerClient.Parse(Encoding.UTF8.GetBytes(json)); return false; }
        catch (InvalidOperationException) { return true; }
    }
}
