using System.Diagnostics;
using System.Collections.Concurrent;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

/// Explicit CLI fixtures never initialize WinUI or launch the Rust input core.
internal static class SessionSelfTest
{
    private static async Task WaitAsync(Func<bool> condition, int milliseconds = 8000)
    {
        var watch = Stopwatch.StartNew();
        while (!condition()) { if (watch.ElapsedMilliseconds > milliseconds) throw new TimeoutException("session fixture timed out"); await Task.Delay(10); }
    }
    private static SessionController Controller(string mode) => new((_, _, _) =>
    {
        var info = new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, "Crossflow.Windows.exe"));
        info.ArgumentList.Add("--session-fixture"); info.ArgumentList.Add(mode); return info;
    });
    private sealed class PausableContext : SynchronizationContext
    {
        private readonly BlockingCollection<(SendOrPostCallback Callback, object? State)> queue = new();
        public override void Post(SendOrPostCallback callback, object? state) => queue.Add((callback, state));
        internal void PumpUntil(Task task)
        {
            var watch = Stopwatch.StartNew();
            while (!task.IsCompleted)
            {
                if (watch.Elapsed > TimeSpan.FromSeconds(8)) throw new TimeoutException("fixture context startup timed out");
                if (queue.TryTake(out var work, 20)) work.Callback(work.State);
            }
            task.GetAwaiter().GetResult();
        }
    }
    internal static async Task RunAsync(string output)
    {
        var checks = new Dictionary<string, bool>();
        var controllers = new List<SessionController>();
        string directory = Path.Combine(Path.GetDirectoryName(Path.GetFullPath(output))!, "session-fixture-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        try
        {
            string Record(string state, bool ready, string? error = null) => JsonSerializer.Serialize(new { protocol = 1, state, epoch = ulong.MaxValue, capture_ready = ready, error });
            checks["ulong_epoch_and_nonfatal_error"] = SessionController.ParseStatus(Record("paused", true, "state_changed")).CanArm;
            checks["readiness_gate"] = !SessionController.ParseStatus(Record("paused", false)).CanArm && !SessionController.ParseStatus(Record("connecting", true)).CanArm;
            foreach (string invalid in new[] { "{}", Record("invented", true), Record("paused", true).Replace("\"protocol\":1", "\"protocol\":2"), new string('x', 4096) })
            { try { SessionController.ParseStatus(invalid); throw new Exception("invalid status accepted"); } catch (Exception failure) when (failure.Message != "invalid status accepted") { } }
            checks["invalid_status_rejected"] = true;
            try { SessionController.ValidateEndpoint("example.com:24800"); throw new Exception("DNS accepted"); } catch (InvalidOperationException) { checks["ip_literal_only"] = true; }
            var normal = Controller("normal"); controllers.Add(normal);
            await normal.ConnectAsync("127.0.0.1:24800", directory, directory);
            await WaitAsync(() => normal.Snapshot.State == "connecting");
            try { await normal.ArmAsync(); throw new Exception("early arm accepted"); } catch (InvalidOperationException) { checks["early_arm_rejected"] = true; }
            await WaitAsync(() => normal.Snapshot.CanArm);
            ulong before = normal.Snapshot.Epoch;
            await normal.ArmAsync(); await WaitAsync(() => normal.Snapshot.State == "sharing");
            checks["arm_uses_reported_epoch"] = normal.Snapshot.Epoch == before + 1;
            await Task.Delay(2200); checks["periodic_status_and_stderr_drain"] = normal.Snapshot.State == "sharing";
            await normal.PauseAsync(); await WaitAsync(() => normal.Snapshot.CanArm && normal.Snapshot.Epoch > before);
            await Task.Delay(250); checks["pause_requires_manual_rearm"] = normal.Snapshot.State == "paused";
            long firstGeneration = normal.Snapshot.Generation;
            checks["graceful_stop_reaped"] = await normal.StopAsync() && normal.Snapshot.State == "stopped" && !normal.Snapshot.ProcessRunning;
            await normal.ConnectAsync("127.0.0.1:24800", directory, directory); await WaitAsync(() => normal.Snapshot.CanArm);
            checks["generation_and_no_autostart"] = normal.Snapshot.Generation > firstGeneration && normal.Snapshot.State == "paused";
            await normal.StopAsync();
            var delayed = Controller("old-state"); controllers.Add(delayed);
            await delayed.ConnectAsync("127.0.0.1:24800", directory, directory); await WaitAsync(() => delayed.Snapshot.CanArm);
            ulong delayedEpoch = delayed.Snapshot.Epoch;
            int oldPaused = 0, oldSharing = 0;
            delayed.Changed += value =>
            {
                if (value.CommandPending && value.State == "paused" && value.Epoch == delayedEpoch) Interlocked.Increment(ref oldPaused);
                if (value.CommandPending && value.State == "sharing" && value.Epoch == delayedEpoch + 1) Interlocked.Increment(ref oldSharing);
            };
            await delayed.ArmAsync(); await WaitAsync(() => Volatile.Read(ref oldPaused) >= 2);
            checks["old_paused_does_not_clear_pending"] = delayed.Snapshot.CommandPending && !delayed.Snapshot.CanArm;
            await WaitAsync(() => delayed.Snapshot.State == "sharing" && !delayed.Snapshot.CommandPending);
            await delayed.PauseAsync(); await WaitAsync(() => Volatile.Read(ref oldSharing) >= 2);
            checks["old_sharing_does_not_clear_pending"] = delayed.Snapshot.CommandPending;
            await WaitAsync(() => delayed.Snapshot.CanArm); await delayed.StopAsync();
            var independent = Controller("silent"); controllers.Add(independent);
            var contextReady = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            using var unfreeze = new ManualResetEventSlim();
            var uiThread = new Thread(() =>
            {
                try
                {
                    var context = new PausableContext(); SynchronizationContext.SetSynchronizationContext(context);
                    context.PumpUntil(independent.ConnectAsync("127.0.0.1:24800", directory, directory));
                    contextReady.TrySetResult();
                    unfreeze.Wait(); // No queued continuations run during supervision.
                }
                catch (Exception failure) { contextReady.TrySetException(failure); }
            }) { IsBackground = true };
            uiThread.Start();
            try
            {
                await contextReady.Task.WaitAsync(TimeSpan.FromSeconds(8));
                await WaitAsync(() => !independent.Snapshot.ProcessRunning);
                checks["watchdog_independent_of_frozen_ui_context"] = independent.Snapshot.State == "error";
            }
            finally { unfreeze.Set(); if (!uiThread.Join(2000)) throw new TimeoutException("fixture UI context did not exit"); }
            foreach (string mode in new[] { "bad-status", "oversized", "eof", "nonzero", "silent", "fatal" })
            {
                var controller = Controller(mode); controllers.Add(controller);
                await controller.ConnectAsync("127.0.0.1:24800", directory, directory);
                await WaitAsync(() => !controller.Snapshot.ProcessRunning);
                checks[mode + "_fails_closed"] = controller.Snapshot.State == "error";
                if (mode == "fatal") checks["fatal_reason_preserved"] = controller.Snapshot.Error?.Contains("hotkey registration failed", StringComparison.Ordinal) == true;
            }
            var stubborn = Controller("ignore-stop"); controllers.Add(stubborn);
            await stubborn.ConnectAsync("127.0.0.1:24800", directory, directory); await WaitAsync(() => stubborn.Snapshot.CanArm);
            checks["unresponsive_child_reaped"] = await stubborn.StopAsync() && !stubborn.Snapshot.ProcessRunning && stubborn.Snapshot.State == "error";
            File.WriteAllText(output, JsonSerializer.Serialize(new { ok = checks.Values.All(value => value), actual_os_input = false, checks }));
            Environment.ExitCode = checks.Values.All(value => value) ? 0 : 1;
        }
        catch (Exception error)
        {
            File.WriteAllText(output, JsonSerializer.Serialize(new { ok = false, actual_os_input = false, checks, error = error.Message })); Environment.ExitCode = 1;
        }
        finally { foreach (var controller in controllers) await controller.StopAsync(); }
    }

    internal static async Task FixtureAsync(string mode)
    {
        using var input = new StreamReader(Console.OpenStandardInput(), new UTF8Encoding(false));
        using var output = new StreamWriter(Console.OpenStandardOutput(), new UTF8Encoding(false)) { AutoFlush = true };
        using var error = new StreamWriter(Console.OpenStandardError(), new UTF8Encoding(false)) { AutoFlush = true };
        var gate = new SemaphoreSlim(1, 1);
        string state = "connecting"; ulong epoch = 0; bool ready = false; string? errorText = null;
        using var lifetime = new CancellationTokenSource();
        async Task EmitAsync()
        {
            await gate.WaitAsync();
            try { await output.WriteLineAsync(JsonSerializer.Serialize(new { protocol = 1, state, epoch, capture_ready = ready, error = errorText })); }
            finally { gate.Release(); }
        }
        if (mode == "eof") return;
        if (mode == "nonzero") { state = "stopped"; await EmitAsync(); Environment.ExitCode = 7; return; }
        if (mode == "fatal") { state = "error"; errorText = "hotkey registration failed: already registered"; await EmitAsync(); return; }
        if (mode == "oversized") await output.WriteLineAsync(new string('x', 5000));
        else if (mode == "bad-status") await output.WriteLineAsync("{\"protocol\":2}");
        else await EmitAsync();
        if (mode == "normal") await error.WriteAsync(new string('d', 131072));
        await Task.Delay(300);
        state = "paused"; epoch = 1; ready = true; await EmitAsync();
        var timer = Task.Run(async () =>
        {
            try { while (!lifetime.IsCancellationRequested) { await Task.Delay(150, lifetime.Token); if (mode != "silent") await EmitAsync(); } }
            catch (OperationCanceledException) { }
        });
        while (true)
        {
            string? line = await input.ReadLineAsync();
            if (line is null) { if (mode == "ignore-stop") { await Task.Delay(Timeout.Infinite); } break; }
            using var command = JsonDocument.Parse(line);
            string? op = command.RootElement.GetProperty("op").GetString();
            if (op == "arm")
            {
                if (command.RootElement.GetProperty("epoch").GetUInt64() != epoch || state != "paused") { Environment.ExitCode = 9; break; }
                if (mode == "old-state") { await EmitAsync(); await Task.Delay(500); }
                state = "sharing"; epoch++;
            }
            if (op == "pause") { if (mode == "old-state") { await EmitAsync(); await Task.Delay(500); } state = "paused"; epoch++; }
            if (op == "stop")
            {
                if (mode == "ignore-stop") continue;
                lifetime.Cancel(); await timer;
                state = "stopped"; ready = false; await EmitAsync(); return;
            }
            await EmitAsync();
        }
        lifetime.Cancel(); await timer;
    }
}
