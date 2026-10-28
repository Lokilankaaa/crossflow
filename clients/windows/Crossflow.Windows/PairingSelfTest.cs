using System.Diagnostics;
using System.Text.Json;

namespace Crossflow.Windows;

/// Explicit process-supervision tests; no network, peer store or OS input.
internal static class PairingSelfTest
{
    private static readonly string Id = new('a', 64);
    private static readonly string OtherId = new('b', 64);

    internal static async Task FixtureAsync(string mode)
    {
        if (await Console.In.ReadLineAsync() is null) return;
        async Task Emit(object message) { await Console.Out.WriteLineAsync(JsonSerializer.Serialize(message)); await Console.Out.FlushAsync(); }
        var pending = new { @event = "pending", name = "Fixture Mac", platform = "macos", fingerprint = Id };
        if (mode == "early_saved") { await Emit(new { @event = "saved", id = Id }); return; }
        if (mode == "oversized") { await Console.Out.WriteLineAsync(new string('x', 70000)); await Console.Out.FlushAsync(); return; }
        if (mode == "unknown_field") { await Emit(new { @event = "pending", name = "Fixture", platform = "macos", fingerprint = Id, extra = true }); return; }
        if (mode == "unsafe_name") { await Emit(new { @event = "pending", name = "Fixture\u202eMac", platform = "macos", fingerprint = Id }); return; }
        await Emit(pending);
        if (mode == "duplicate_pending") { await Emit(pending); return; }
        if (mode == "cancel") { await Console.In.ReadLineAsync(); return; }
        var line = await Console.In.ReadLineAsync();
        if (line is null) return;
        using var command = JsonDocument.Parse(line);
        if (command.RootElement.GetProperty("op").GetString() != "approve") return;
        await Emit(new { @event = "saved", id = mode == "wrong_id" ? OtherId : Id });
        if (mode == "nonzero") Environment.ExitCode = 7;
    }

    internal static async Task RunAsync(string report)
    {
        var checks = new Dictionary<string, bool>();
        try
        {
            foreach (string mode in new[] { "success", "early_saved", "oversized", "unknown_field", "unsafe_name", "duplicate_pending", "wrong_id", "nonzero", "cancel" })
            {
                bool noCodeInArguments = false;
                var controller = new PairingController(info =>
                {
                    noCodeInArguments = info.ArgumentList.Count == 0;
                    info.FileName = Environment.ProcessPath!;
                    info.ArgumentList.Add("--pairing-fixture"); info.ArgumentList.Add(mode);
                    return info;
                });
                bool succeeded = false, cancelled = false;
                try
                {
                    var pending = await controller.BeginAsync("192.168.1.2:24801", "12345678").WaitAsync(TimeSpan.FromSeconds(8));
                    if (mode == "cancel")
                    {
                        controller.Cancel();
                        cancelled = await controller.StopAsync();
                    }
                    else
                    {
                        string saved = await controller.ApproveAsync().WaitAsync(TimeSpan.FromSeconds(8));
                        succeeded = saved == pending.Fingerprint && saved == Id;
                    }
                }
                catch { /* A malicious or failed child must not become success. */ }
                finally
                {
                    checks[mode + "_reaped"] = await controller.StopAsync() && !controller.IsRunning;
                }
                checks[mode + "_result"] = mode == "success" ? succeeded : mode == "cancel" ? cancelled && !succeeded : !succeeded;
                checks[mode + "_code_not_in_arguments"] = noCodeInArguments;
            }
            foreach (string endpoint in new[] { "127.0.0.1:1", "8.8.8.8:1", "localhost:1", "192.168.1.2:0", "[::ffff:192.168.1.2]:1", "[fe80::1]:1" })
            {
                bool rejected = false;
                try { PairingController.ValidateEndpoint(endpoint); } catch { rejected = true; }
                checks["reject_endpoint_" + checks.Count] = rejected;
            }
            checks["scoped_link_local_preserved"] = PairingController.ValidateEndpoint("[fe80::1%7]:24801") == "[fe80::1%7]:24801";
            await File.WriteAllTextAsync(report, JsonSerializer.Serialize(new { ok = checks.Values.All(v => v), checks, network = false, trust_written = false }));
            Environment.ExitCode = checks.Values.All(v => v) ? 0 : 1;
        }
        catch
        {
            await File.WriteAllTextAsync(report, JsonSerializer.Serialize(new { ok = false, checks, error = "pairing_self_test_failed" }));
            Environment.ExitCode = 1;
        }
    }
}
