using System.Diagnostics;
using System.Net;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

internal sealed record SessionSnapshot(string State, ulong Epoch, bool CaptureReady, string? Error,
    long Generation, bool ProcessRunning, bool CommandPending = false, bool StopRequested = false)
{
    internal bool CanArm => ProcessRunning && State == "paused" && CaptureReady && !CommandPending && !StopRequested;
}

/// Owns one packaged Rust sender. No private identity material is parsed here.
internal sealed class SessionController
{
    private readonly object sync = new();
    private readonly SemaphoreSlim commands = new(1, 1);
    private readonly Func<string, string, string, ProcessStartInfo>? fixture;
    private readonly Func<ProcessStartInfo, ProcessStartInfo>? peerFixture;
    private Run? current;
    private long generation;
    private SessionSnapshot snapshot = new("idle", 0, false, null, 0, false);
    internal event Action<SessionSnapshot>? Changed;
    internal SessionSnapshot Snapshot { get { lock (sync) return snapshot; } }
    internal string? LastLogPath { get; private set; }

    internal SessionController() { }
    // Test-only factory never participates in the production constructor.
    internal SessionController(Func<string, string, string, ProcessStartInfo> fixture) => this.fixture = fixture;
    // Inspects the actual peer launch request before replacing it with a fixture.
    internal SessionController(Func<ProcessStartInfo, ProcessStartInfo> peerFixture) => this.peerFixture = peerFixture;

    private sealed class Run(Process process, long generation)
    {
        internal readonly Process Process = process;
        internal readonly long Generation = generation;
        internal readonly CancellationTokenSource Lifetime = new();
        internal readonly SemaphoreSlim Writes = new(1, 1);
        internal readonly StringBuilder StderrTail = new(); // Never parsed as state or displayed as an input log.
        internal long LastStatus = Stopwatch.GetTimestamp();
        internal int StopRequested;
        internal bool SawStopped;
        internal string? Failure;
        internal ulong? PendingEpoch;
        internal Task Completion = Task.CompletedTask;
    }

    internal static string ValidateEndpoint(string text)
    {
        if (!IPEndPoint.TryParse(text.Trim(), out var endpoint) || endpoint.Port == 0 ||
            endpoint.Address.Equals(IPAddress.Any) || endpoint.Address.Equals(IPAddress.IPv6Any))
            throw new InvalidOperationException("请输入目标 IP 和端口，例如 192.168.1.20:24800；IPv6 请使用 [地址]:端口。");
        return endpoint.ToString();
    }

    internal async Task ConnectAsync(string host, string identity, string? logDirectory = null)
    {
        host = ValidateEndpoint(host);
        identity = Path.GetFullPath(identity);
        if (!Directory.Exists(identity)) throw new InvalidOperationException("请选择已有的配对身份文件夹。");
        await ConnectCoreAsync(log => fixture?.Invoke(host, identity, log) ?? PackagedStart(host, identity, log), logDirectory);
    }

    internal async Task ConnectPeerAsync(string peerId, string? logDirectory = null)
    {
        if (peerId is null || peerId.Length != 64 || peerId.Any(value => !(value is >= '0' and <= '9' or >= 'a' and <= 'f')))
            throw new InvalidOperationException("已配对设备标识无效，请刷新设备列表后重试。");
        await ConnectCoreAsync(log =>
        {
            var info = PackagedInfo(["desktop-send", "--peer", peerId, "--log", log, "--seconds", "3600", "--max-hz", "1000"]);
            return peerFixture?.Invoke(info) ?? RequirePackaged(info);
        }, logDirectory);
    }

    // Both production entry points share process ownership, pipes and supervision.
    private async Task ConnectCoreAsync(Func<string, ProcessStartInfo> start, string? logDirectory)
    {
        await commands.WaitAsync();
        try
        {
            lock (sync) if (current is not null) throw new InvalidOperationException("请先停止当前连接。");
            string directory = logDirectory ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Crossflow", "Logs");
            Directory.CreateDirectory(directory);
            string log = Path.Combine(directory, $"session-{DateTime.UtcNow:yyyyMMdd-HHmmss}-{Guid.NewGuid():N}.jsonl");
            var info = start(log);
            info.UseShellExecute = false; info.CreateNoWindow = true;
            info.RedirectStandardInput = true; info.RedirectStandardOutput = true; info.RedirectStandardError = true;
            info.StandardInputEncoding = new UTF8Encoding(false);
            info.StandardOutputEncoding = new UTF8Encoding(false, true);
            info.StandardErrorEncoding = new UTF8Encoding(false, false);
            var process = new Process { StartInfo = info };
            Run run;
            lock (sync)
            {
                run = new Run(process, ++generation);
                current = run;
                snapshot = new("starting", 0, false, null, generation, true);
            }
            Publish();
            try
            {
                // Process creation and all pipe IO stay off the UI thread.
                await Task.Run(() => { if (!process.Start()) throw new InvalidOperationException("无法启动共享内核。"); });
                process.StandardInput.AutoFlush = true;
                LastLogPath = log;
                // Supervision must continue even when the XAML dispatcher is busy.
                run.Completion = Task.Run(() => MonitorAsync(run));
            }
            catch
            {
                lock (sync) { if (current == run) { current = null; snapshot = snapshot with { State = "error", Error = "共享内核无法启动，请检查客户端安装。", ProcessRunning = false }; } }
                process.Dispose(); run.Lifetime.Dispose(); Publish(); throw;
            }
        }
        finally { commands.Release(); }
    }

    private static ProcessStartInfo PackagedStart(string host, string identity, string log)
        => RequirePackaged(PackagedInfo(["desktop-send", "--host", host, "--identity", identity, "--log", log, "--seconds", "3600", "--max-hz", "1000"]));

    private static ProcessStartInfo PackagedInfo(string[] arguments)
    {
        string executable = Path.Combine(AppContext.BaseDirectory, "crossflow.exe");
        var info = new ProcessStartInfo(executable) { WorkingDirectory = AppContext.BaseDirectory };
        foreach (string value in arguments) info.ArgumentList.Add(value);
        return info;
    }

    private static ProcessStartInfo RequirePackaged(ProcessStartInfo info)
    {
        if (!File.Exists(info.FileName)) throw new InvalidOperationException("缺少随客户端提供的共享内核，请重新构建或安装客户端。");
        return info;
    }

    internal Task ArmAsync() => CommandAsync("arm");
    internal Task PauseAsync() => CommandAsync("pause");
    private async Task CommandAsync(string op)
    {
        await commands.WaitAsync();
        try
        {
            Run run; ulong epoch;
            lock (sync)
            {
                run = current ?? throw new InvalidOperationException("尚未连接。");
                if (op == "arm" && !snapshot.CanArm) throw new InvalidOperationException("等待连接就绪并处于暂停状态后，再开始共享。");
                if (op == "pause" && (snapshot.State != "sharing" || snapshot.StopRequested || snapshot.CommandPending)) throw new InvalidOperationException("当前没有可暂停的共享。");
                epoch = snapshot.Epoch;
                run.PendingEpoch = epoch;
                snapshot = snapshot with { CommandPending = true };
            }
            Publish();
            try { await WriteAsync(run, op == "arm" ? JsonSerializer.Serialize(new { op, epoch }) : JsonSerializer.Serialize(new { op })); }
            catch { await FailAndStopAsync(run, "命令未能送达，共享已停止；请重新连接。"); throw new InvalidOperationException("命令未能送达，请重新连接。"); }
        }
        finally { commands.Release(); }
    }

    internal async Task<bool> StopAsync()
    {
        await commands.WaitAsync();
        try
        {
            Run? run; lock (sync) run = current;
            if (run is null) return true;
            await RequestStopAsync(run);
            try { await run.Completion.WaitAsync(TimeSpan.FromSeconds(5)); }
            catch (TimeoutException)
            {
                SetFailure(run, "共享内核未及时退出，已请求强制结束；远端将按超时保护释放。");
                TryKill(run);
                try { await run.Completion.WaitAsync(TimeSpan.FromSeconds(3)); } catch (TimeoutException) { return false; }
            }
            lock (sync) if (current != run) return true;
            // A supervisor fault must not strand an already-exited child or
            // permit another session while its process is still alive.
            TryKill(run);
            try { await run.Process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(3)); }
            catch { return false; }
            lock (sync)
            {
                if (current == run)
                {
                    snapshot = snapshot with { State = "error", Error = run.Failure ?? "共享内核已结束，退出确认未完整返回。", CaptureReady = false, ProcessRunning = false, CommandPending = false, StopRequested = false };
                    current = null;
                }
            }
            run.Process.Dispose(); run.Lifetime.Dispose(); Publish(); return true;
        }
        finally { commands.Release(); }
    }

    private static async Task WriteAsync(Run run, string line)
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(1));
        await run.Writes.WaitAsync(timeout.Token);
        try
        {
            await run.Process.StandardInput.WriteLineAsync(line.AsMemory(), timeout.Token);
            await run.Process.StandardInput.FlushAsync(timeout.Token);
        }
        finally { run.Writes.Release(); }
    }

    private async Task RequestStopAsync(Run run)
    {
        if (Interlocked.Exchange(ref run.StopRequested, 1) != 0) return;
        lock (sync) if (current == run) snapshot = snapshot with { StopRequested = true, CommandPending = true };
        Publish();
        try { await WriteAsync(run, "{\"op\":\"stop\"}"); }
        catch { TryKill(run); }
        // Close on a background thread; shutdown never synchronously blocks XAML.
        try { await Task.Run(() => run.Process.StandardInput.Close()).WaitAsync(TimeSpan.FromSeconds(1)); }
        catch { TryKill(run); }
    }

    private async Task FailAndStopAsync(Run run, string message)
    {
        SetFailure(run, message);
        await RequestStopAsync(run);
        try { await run.Process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(3)); }
        catch { TryKill(run); }
    }

    private void SetFailure(Run run, string message)
    {
        lock (sync)
        {
            if (current != run) return;
            run.Failure ??= message;
            snapshot = snapshot with { State = "error", CaptureReady = false, Error = run.Failure };
        }
        Publish();
    }

    private static void TryKill(Run run)
    {
        try { if (!run.Process.HasExited) run.Process.Kill(entireProcessTree: true); } catch (InvalidOperationException) { } catch (System.ComponentModel.Win32Exception) { }
    }

    private async Task MonitorAsync(Run run)
    {
        Task output = ReadStatusesAsync(run), stderr = DrainStderrAsync(run), heartbeat = WatchStatusAsync(run);
        try
        {
            await run.Process.WaitForExitAsync();
            run.Lifetime.Cancel();
            await Task.WhenAll(output, stderr, heartbeat);
            lock (sync)
            {
                if (current != run) return;
                string? error = run.Failure ?? (run.Process.ExitCode != 0 ? "共享内核异常退出，请检查配对与网络后重试。" : !run.SawStopped ? "共享内核提前结束，未收到停止确认。" : null);
                snapshot = snapshot with { State = error is null ? "stopped" : "error", CaptureReady = false, Error = error, ProcessRunning = false, CommandPending = false, StopRequested = false };
                current = null;
            }
        }
        catch
        {
            SetFailure(run, "无法确认共享内核退出，请停止连接后重试。");
            TryKill(run);
        }
        finally
        {
            run.Lifetime.Cancel();
            lock (sync) { if (current != run) { run.Process.Dispose(); run.Lifetime.Dispose(); } }
            Publish();
        }
    }

    private async Task ReadStatusesAsync(Run run)
    {
        var pending = new StringBuilder(); var buffer = new char[1024];
        try
        {
            while (true)
            {
                int count = await run.Process.StandardOutput.ReadAsync(buffer.AsMemory());
                if (count == 0) break;
                for (int i = 0; i < count; i++)
                {
                    if (buffer[i] == '\n')
                    {
                        string line = pending.ToString().TrimEnd('\r'); pending.Clear();
                        ApplyStatus(run, ParseStatus(line));
                    }
                    else { pending.Append(buffer[i]); if (pending.Length >= 4096) throw new InvalidDataException(); }
                }
            }
            if (pending.Length != 0 || !run.SawStopped) await FailAndStopAsync(run, "共享状态通道提前关闭，连接已停止。");
        }
        catch { await FailAndStopAsync(run, "收到无效共享状态，连接已停止。"); }
    }

    private async Task DrainStderrAsync(Run run)
    {
        var buffer = new char[1024];
        try
        {
            int count;
            while ((count = await run.Process.StandardError.ReadAsync(buffer.AsMemory())) != 0)
            {
                lock (run.StderrTail) { run.StderrTail.Append(buffer, 0, count); if (run.StderrTail.Length > 8192) run.StderrTail.Remove(0, run.StderrTail.Length - 8192); }
            }
        }
        catch { /* Never interpret diagnostic stream text as session state. */ }
    }

    private async Task WatchStatusAsync(Run run)
    {
        try
        {
            while (!run.Lifetime.IsCancellationRequested)
            {
                await Task.Delay(200, run.Lifetime.Token);
                if (Stopwatch.GetElapsedTime(Interlocked.Read(ref run.LastStatus)) >= TimeSpan.FromSeconds(2))
                { await FailAndStopAsync(run, "超过两秒未收到共享状态，连接已停止。"); return; }
            }
        }
        catch (OperationCanceledException) { }
    }

    internal static SessionSnapshot ParseStatus(string line)
    {
        if (Encoding.UTF8.GetByteCount(line) >= 4096) throw new InvalidDataException();
        using var document = JsonDocument.Parse(line, new JsonDocumentOptions { MaxDepth = 8 });
        var value = document.RootElement;
        if (value.ValueKind != JsonValueKind.Object || value.EnumerateObject().Count() != 5 ||
            value.EnumerateObject().Select(property => property.Name).Distinct().Count() != 5) throw new InvalidDataException();
        if (value.GetProperty("protocol").GetInt32() != 1) throw new InvalidDataException();
        string? state = value.GetProperty("state").GetString();
        if (state is not ("starting" or "connecting" or "paused" or "sharing" or "stopping" or "stopped" or "error")) throw new InvalidDataException();
        ulong epoch = value.GetProperty("epoch").GetUInt64();
        bool ready = value.GetProperty("capture_ready").GetBoolean();
        var error = value.GetProperty("error");
        if (error.ValueKind is not (JsonValueKind.Null or JsonValueKind.String)) throw new InvalidDataException();
        string? errorText = error.GetString();
        if (errorText?.Length > 512) throw new InvalidDataException();
        return new(state, epoch, ready, errorText, 0, true);
    }

    private void ApplyStatus(Run run, SessionSnapshot status)
    {
        bool fatal;
        lock (sync)
        {
            if (current != run || run.Generation != generation) return;
            Interlocked.Exchange(ref run.LastStatus, Stopwatch.GetTimestamp());
            run.SawStopped |= status.State == "stopped";
            fatal = status.State == "error";
            if (run.PendingEpoch is ulong pending && (status.Epoch > pending || status.Error is not null || status.State is "error" or "stopping" or "stopped"))
                run.PendingEpoch = null;
            if (run.Failure is null) snapshot = status with { Generation = run.Generation, StopRequested = run.StopRequested != 0, CommandPending = run.PendingEpoch is not null || run.StopRequested != 0 };
        }
        Publish();
        if (fatal) _ = FailAndStopAsync(run, status.Error is { Length: > 0 } reason ? "共享内核报告：" + reason : "共享内核报告错误，连接已停止。");
    }

    private void Publish()
    {
        SessionSnapshot value; lock (sync) value = snapshot;
        try { Changed?.Invoke(value); } catch { /* A UI observer must not break process supervision. */ }
    }
}
