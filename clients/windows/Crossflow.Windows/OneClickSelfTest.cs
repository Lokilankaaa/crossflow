using System.Diagnostics;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

internal static class OneClickSelfTest
{
    private static async Task WaitAsync(Func<bool> ready)
    {
        var watch = Stopwatch.StartNew();
        while (!ready()) { if (watch.Elapsed > TimeSpan.FromSeconds(5)) throw new TimeoutException("one-click fixture timed out"); await Task.Delay(10); }
    }

    internal static async Task RunAsync(string output)
    {
        var checks = new Dictionary<string, bool>();
        var sessions = new List<SessionController>();
        string directory = Path.Combine(Path.GetDirectoryName(Path.GetFullPath(output))!, "oneclick-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory); string config = Path.Combine(directory, "layout.json"); File.WriteAllText(config, "{}");
        const string peer = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        SessionController New(string mode)
        {
            var controller = new SessionController(_ =>
            {
                var info = new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, "Crossflow.Windows.exe"));
                info.ArgumentList.Add("--oneclick-fixture"); info.ArgumentList.Add(mode); return info;
            });
            sessions.Add(controller); return controller;
        }
        try
        {
            var normal = New("normal");
            await normal.StartRoutedAsync(peer, config, directory);
            await WaitAsync(() => normal.Snapshot.State == "sharing");
            await Task.Delay(350);
            checks["one_click_arms_once_with_ready_epoch"] = normal.Snapshot.State == "sharing" && normal.Snapshot.Epoch == 12 && !normal.IsStartPending;
            checks["routed_focus_updates_from_core_status"] = normal.Snapshot.RouteEnabled && normal.Snapshot.FocusDeviceId == "local" && normal.Snapshot.FocusScreenId == "local-main";
            checks["normal_stop_reaped"] = await normal.StopAsync() && normal.Snapshot.State == "stopped";
            checks["stop_clears_route_and_focus"] = !normal.Snapshot.RouteEnabled && normal.Snapshot.FocusDeviceId is null && normal.Snapshot.FocusScreenId is null;

            var reconnect = New("reconnect");
            await reconnect.StartRoutedAsync(peer, config, directory);
            await WaitAsync(() => reconnect.Snapshot.CanArm && reconnect.Snapshot.Epoch == 14);
            await Task.Delay(350);
            checks["reconnect_does_not_consume_old_start_intent"] = reconnect.Snapshot.CanArm && reconnect.Snapshot.State == "paused" && !reconnect.IsStartPending;
            checks["reconnect_reports_no_active_focus"] = !reconnect.Snapshot.RouteEnabled && reconnect.Snapshot.FocusDeviceId is null;
            checks["reconnect_fixture_saw_exactly_one_arm"] = await reconnect.StopAsync() && reconnect.Snapshot.State == "stopped";

            foreach (string mode in new[] { "lost-before-ready", "error-before-ready", "rejected" })
            {
                var controller = New(mode); await controller.StartRoutedAsync(peer, config, directory);
                ulong expected = mode == "lost-before-ready" ? 13UL : 12UL;
                await WaitAsync(() => controller.Snapshot.CanArm && controller.Snapshot.Epoch == expected && controller.Snapshot.Error is null);
                await Task.Delay(250);
                checks[mode + "_no_automatic_retry"] = controller.Snapshot.State == "paused" && !controller.IsStartPending && await controller.StopAsync() && controller.Snapshot.State == "stopped";
            }
            var cancel = New("cancel-before-ready"); await cancel.StartRoutedAsync(peer, config, directory);
            cancel.CancelPendingStart(); await WaitAsync(() => cancel.Snapshot.CanArm);
            checks["closing_cancels_pending_start"] = !cancel.IsStartPending && await cancel.StopAsync() && cancel.Snapshot.State == "stopped";
            var stop = New("stop-before-ready"); await stop.StartRoutedAsync(peer, config, directory);
            checks["stop_before_ready_never_arms"] = await stop.StopAsync() && stop.Snapshot.State == "stopped" && !stop.IsStartPending;
            var readyCancel = New("cancel-at-ready");
            readyCancel.Changed += value => { if (value.State == "paused" && value.CaptureReady) readyCancel.CancelPendingStart(); };
            await readyCancel.StartRoutedAsync(peer, config, directory); await WaitAsync(() => readyCancel.Snapshot.CanArm);
            checks["queued_ready_arm_cancelled_before_write"] = !readyCancel.IsStartPending && await readyCancel.StopAsync() && readyCancel.Snapshot.State == "stopped";

            var ownership = new DragCaptureOwnership(); object canvas = new(), child = new(); ownership.Begin(7);
            checks["child_capture_loss_does_not_cancel_drag"] = !ownership.Lost(child, canvas, 7) && ownership.Matches(7);
            checks["other_pointer_does_not_cancel_drag"] = !ownership.Lost(canvas, canvas, 8) && ownership.Matches(7);
            checks["own_capture_loss_cancels_drag"] = ownership.Lost(canvas, canvas, 7) && !ownership.Matches(7);
            ownership.Begin(9); ownership.End();
            checks["drag_release_resets_capture"] = !ownership.Matches(9);
            File.WriteAllText(output, JsonSerializer.Serialize(new { ok = checks.Values.All(x => x), checks, actual_os_input = false, pointer_events_simulated = true }, CoreBridge.Json));
            Environment.ExitCode = checks.Values.All(x => x) ? 0 : 1;
        }
        catch (Exception error)
        {
            File.WriteAllText(output, JsonSerializer.Serialize(new { ok = false, checks, error = error.ToString(), actual_os_input = false }, CoreBridge.Json));
            Environment.ExitCode = 1;
        }
        finally { foreach (var controller in sessions) await controller.StopAsync(); }
    }

    internal static async Task FixtureAsync(string mode)
    {
        using var input = new StreamReader(Console.OpenStandardInput(), new UTF8Encoding(false));
        using var output = new StreamWriter(Console.OpenStandardOutput(), new UTF8Encoding(false)) { AutoFlush = true };
        var writes = new SemaphoreSlim(1, 1); using var lifetime = new CancellationTokenSource();
        string state = "connecting"; ulong epoch = 10; bool ready = false; string? error = null; int arms = 0;
        async Task Emit()
        {
            await writes.WaitAsync();
            try { await output.WriteLineAsync(JsonSerializer.Serialize(new { protocol = 2, state, epoch, capture_ready = ready, error, route_enabled = state == "sharing", focus_device_id = state == "sharing" ? "local" : null, focus_screen_id = state == "sharing" ? "local-main" : null })); }
            finally { writes.Release(); }
        }
        await Emit(); await Task.Delay(200);
        state = "paused"; epoch = 11; await Emit();
        if (mode == "lost-before-ready") { state = "connecting"; epoch = 12; await Emit(); epoch = 13; }
        if (mode == "error-before-ready") { error = "screen configuration changed"; await Emit(); error = null; epoch = 12; }
        ready = true; state = "paused"; await Emit(); await Emit();
        var timer = Task.Run(async () => { try { while (!lifetime.IsCancellationRequested) { await Task.Delay(100, lifetime.Token); await Emit(); } } catch (OperationCanceledException) { } });
        try
        {
            while (await input.ReadLineAsync() is string line)
            {
                using var json = JsonDocument.Parse(line); string? op = json.RootElement.GetProperty("op").GetString();
                if (op == "arm")
                {
                    arms++;
                    if (arms != 1 || state != "paused" || json.RootElement.GetProperty("epoch").GetUInt64() != epoch) { Environment.ExitCode = 9; return; }
                    if (mode == "rejected") { error = "state_changed"; await Emit(); error = null; epoch++; await Emit(); continue; }
                    state = "sharing"; epoch++; await Emit();
                    if (mode == "reconnect")
                    {
                        await Task.Delay(50); state = "connecting"; ready = false; epoch++; await Emit();
                        await Task.Delay(50); state = "paused"; ready = true; epoch++; await Emit();
                    }
                }
                else if (op == "stop")
                {
                    int expected = mode is "normal" or "reconnect" or "rejected" ? 1 : 0;
                    if (arms != expected) { Environment.ExitCode = 9; return; }
                    state = "stopped"; ready = false; await Emit(); return;
                }
            }
        }
        finally { lifetime.Cancel(); await timer; }
    }
}
