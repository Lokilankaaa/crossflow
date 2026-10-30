using System.Diagnostics;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

/// Exercises geometry binding and real process argument construction without OS input.
internal static class RuntimeLayoutSelfTest
{
    internal static async Task RunAsync(string output)
    {
        var checks = new Dictionary<string, bool>();
        SessionController? session = null;
        try
        {
            const string peerId = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
            var local = DisplayClient.Parse(Encoding.UTF8.GetBytes("[{\"id\":\"left\",\"x\":-1920,\"y\":-200,\"width\":1920,\"height\":1080},{\"id\":\"right\",\"x\":0,\"y\":0,\"width\":2560,\"height\":1440}]"));
            var remote = DisplayClient.Parse(Encoding.UTF8.GetBytes("[{\"id\":\"1\",\"x\":0,\"y\":0,\"width\":1470,\"height\":956},{\"id\":\"2\",\"x\":1470,\"y\":-300,\"width\":1920,\"height\":1080}]"));
            var config = RuntimeLayouts.Create(peerId, local, remote);
            CoreBridge.Validate(config.Layout);
            var left = config.Layout.Screens.Single(x => x.DeviceId == "local" && x.X < 0);
            var r1 = config.Layout.Screens[2]; var r2 = config.Layout.Screens[3];
            checks["all_displays_and_negative_native_origin"] = config.Layout.Screens.Count == 4 && config.Bindings.Count == 4 && left.X == -1920 && left.Y == -200;
            checks["remote_group_native_offsets_preserved"] = r1.X == 2560 && r1.Y == 0 && r2.X - r1.X == 1470 && r2.Y - r1.Y == -300;
            checks["native_coordinates_not_replaced_by_arrangement"] = config.Bindings[2].Native.X == 0 && config.Bindings[2].NativeId == "1" && config.Bindings[3].Native.Y == -300;
            var moved = new LayoutDocument(config.Layout.Screens.Select(s => s.DeviceId == peerId ? s with { Y = s.Y + 200 } : s).ToList());
            var applied = RuntimeLayouts.WithLayout(config, moved);
            checks["layout_edit_keeps_native_bindings"] = applied.Layout.Screens[2].Y == 200 && applied.Bindings.SequenceEqual(config.Bindings);
            var reRead = RuntimeLayouts.Create(peerId, local.AsEnumerable().Reverse().ToList(), remote.AsEnumerable().Reverse().ToList());
            checks["enumeration_order_does_not_change_identity"] = RuntimeLayouts.SameInventory(config, reRead);
            var hotplug = RuntimeLayouts.Create(peerId, local, [remote[0] with { Id = "replacement" }, remote[1]]);
            checks["replacement_display_invalidates_saved_layout"] = !RuntimeLayouts.SameInventory(config, hotplug);
            var resolution = RuntimeLayouts.Create(peerId, local, [remote[0] with { Width = 1400 }, remote[1]]);
            checks["resolution_change_invalidates_saved_layout"] = !RuntimeLayouts.SameInventory(config, resolution);
            checks["different_peer_invalidates_saved_layout"] = !RuntimeLayouts.SameInventory(config, RuntimeLayouts.Create(new string('a', 64), local, remote));
            foreach (var invalid in new[] { new LayoutDocument(config.Layout.Screens.Skip(1).ToList()), new LayoutDocument(config.Layout.Screens.Select(s => s == left ? s with { DeviceId = peerId } : s).ToList()) })
            {
                bool rejected = false; try { RuntimeLayouts.WithLayout(config, invalid); } catch (InvalidOperationException) { rejected = true; }
                if (!rejected) throw new Exception("Changed screen binding accepted");
            }
            checks["missing_or_reassigned_bindings_rejected"] = true;
            foreach (string invalid in new[] { "[]", "null", "[{}]", "[{\"id\":\"1\",\"x\":0,\"y\":0,\"width\":0,\"height\":1}]", "[{\"id\":\"1\",\"x\":0,\"y\":0,\"width\":1,\"height\":1,\"x\":3}]", "[{\"id\":null,\"x\":0,\"y\":0,\"width\":1,\"height\":1}]" })
            {
                bool rejected = false; try { DisplayClient.Parse(Encoding.UTF8.GetBytes(invalid)); } catch (InvalidOperationException) { rejected = true; }
                if (!rejected) throw new Exception("Invalid inventory accepted");
            }
            checks["malformed_inventory_rejected"] = true;
            string json = JsonSerializer.Serialize(config, CoreBridge.Json);
            var roundtrip = JsonSerializer.Deserialize<RuntimeLayoutDocument>(json, CoreBridge.Json)!;
            checks["runtime_schema_roundtrip"] = RuntimeLayouts.SameInventory(config, roundtrip) && json.Contains("\"native_id\"") && json.Contains("\"local_device_id\"");
            string directory = Path.Combine(Path.GetDirectoryName(Path.GetFullPath(output))!, "runtime-fixture-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(directory);
            string path = Path.Combine(directory, "layout with spaces.json"); File.WriteAllText(path, json);
            string[]? actualArguments = null;
            session = new(request =>
            {
                actualArguments = request.ArgumentList.ToArray();
                var fixture = new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, "Crossflow.Windows.exe"));
                fixture.ArgumentList.Add("--session-fixture"); fixture.ArgumentList.Add("peer"); return fixture;
            });
            await session.ConnectPeerAsync(peerId, directory, path);
            checks["runtime_config_one_argument_no_shell"] = actualArguments is not null && actualArguments.TakeLast(2).SequenceEqual(new[] { "--runtime-layout", path }) && actualArguments[2] == peerId && session.IsRouted;
            checks["session_stops_after_fixture"] = await session.StopAsync();
            File.WriteAllText(output, JsonSerializer.Serialize(new { ok = checks.Values.All(x => x), checks, actual_os_input = false }, CoreBridge.Json));
            Environment.ExitCode = checks.Values.All(x => x) ? 0 : 1;
        }
        catch (Exception error)
        {
            File.WriteAllText(output, JsonSerializer.Serialize(new { ok = false, checks, error = error.ToString(), actual_os_input = false }, CoreBridge.Json));
            Environment.ExitCode = 1;
        }
        finally { if (session is not null) await session.StopAsync(); }
    }
}
