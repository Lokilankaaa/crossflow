using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

internal sealed record AuthenticatedPairingPeer(string Name, string Platform, string Fingerprint);

/// Interactive initiator only. The issuer's invitation budget belongs to its
/// persistent Rust service. No short code, command body or stderr is logged.
internal sealed class PairingController
{
    private const int MaxLineBytes = 4096, MaxStreamBytes = 65536;
    private readonly object sync = new();
    private readonly SemaphoreSlim stopping = new(1, 1);
    private readonly Func<ProcessStartInfo, ProcessStartInfo>? fixture;
    private Run? current;
    private long generation;

    internal PairingController() { }
    internal PairingController(Func<ProcessStartInfo, ProcessStartInfo> fixture) => this.fixture = fixture;
    internal bool IsRunning { get { lock (sync) return current is not null; } }
    internal static bool IsAvailable => new[] { "crossflow-enroll.exe", "libc++.dll", "libunwind.dll", "libwinpthread-1.dll" }
        .All(name => File.Exists(Path.Combine(AppContext.BaseDirectory, name)));

    private sealed class Run(Process process, long generation)
    {
        internal readonly Process Process = process;
        internal readonly long Generation = generation;
        internal readonly SemaphoreSlim Writes = new(1, 1);
        internal readonly TaskCompletionSource<AuthenticatedPairingPeer> Pending = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal readonly TaskCompletionSource<string> Saved = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal readonly TaskCompletionSource Stop = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal readonly TaskCompletionSource Failed = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal Task Completion = Task.CompletedTask;
        internal Task? Output, Errors, ClosePipes;
        internal bool Started, ApproveSent;
        internal string? Fingerprint, SavedId;
        internal int StopRequested, Failure;
    }

    internal static string ValidateEndpoint(string value)
    {
        if (value.Length > 96 || !IPEndPoint.TryParse(value, out var endpoint) || endpoint.Port == 0)
            throw InvalidEndpoint();
        var ip = endpoint.Address;
        bool allowed = ip.AddressFamily == AddressFamily.InterNetwork
            ? ip.GetAddressBytes() is var b && (b[0] == 10 || (b[0] == 172 && b[1] is >= 16 and <= 31) || (b[0] == 192 && b[1] == 168) || (b[0] == 169 && b[1] == 254))
            : ip.AddressFamily == AddressFamily.InterNetworkV6 && !ip.IsIPv4MappedToIPv6 &&
                ((ip.IsIPv6LinkLocal && ip.ScopeId != 0) || (ip.GetAddressBytes()[0] & 0xfe) == 0xfc);
        if (!allowed) throw InvalidEndpoint();
        return endpoint.ToString();
    }
    private static InvalidOperationException InvalidEndpoint() => new("该设备没有可用的局域网配对地址，请刷新后重试。");
    private static InvalidOperationException Failed() => new("未能确认配对结果。请刷新已配对设备检查状态；也可重新发起邀请。");

    internal Task<AuthenticatedPairingPeer> BeginAsync(string endpoint, string code)
    {
        endpoint = ValidateEndpoint(endpoint);
        if (code.Length != 8 || code.Any(c => c is < '0' or > '9'))
            throw new InvalidOperationException("请输入 8 位数字配对码。");
        if (fixture is null && !IsAvailable)
            throw new InvalidOperationException("此安装缺少配对组件，请使用包含配对组件的客户端版本。");
        var info = new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, "crossflow-enroll.exe"))
        {
            WorkingDirectory = AppContext.BaseDirectory, UseShellExecute = false, CreateNoWindow = true,
            RedirectStandardInput = true, RedirectStandardOutput = true, RedirectStandardError = true,
            StandardOutputEncoding = new UTF8Encoding(false, true), StandardErrorEncoding = new UTF8Encoding(false, true)
        };
        info = fixture?.Invoke(info) ?? info;
        // Test overrides cannot accidentally create visible consoles or shell commands.
        info.UseShellExecute = false; info.CreateNoWindow = true;
        info.RedirectStandardInput = true; info.RedirectStandardOutput = true; info.RedirectStandardError = true;
        info.StandardOutputEncoding = new UTF8Encoding(false, true); info.StandardErrorEncoding = new UTF8Encoding(false, true);
        lock (sync)
        {
            if (current is not null) throw new InvalidOperationException("上一次配对尚未结束，请等待取消完成。");
            var run = new Run(new Process { StartInfo = info }, ++generation);
            current = run;
            // Process launch, all I/O and supervision remain outside the XAML dispatcher.
            run.Completion = Task.Run(() => RunAsync(run, endpoint, code));
            return run.Pending.Task;
        }
    }

    internal async Task<string> ApproveAsync()
    {
        Run run;
        lock (sync)
        {
            run = current ?? throw Failed();
            if (run.Fingerprint is null || run.ApproveSent || run.StopRequested != 0 || run.Failure != 0) throw Failed();
            run.ApproveSent = true;
        }
        try { await WriteAsync(run, new { op = "approve" }).ConfigureAwait(false); }
        catch { MarkFailed(run); }
        return await run.Saved.Task.ConfigureAwait(false);
    }

    internal void Cancel()
    {
        lock (sync) if (current is Run run) { Interlocked.Exchange(ref run.StopRequested, 1); run.Stop.TrySetResult(); }
    }

    internal async Task<bool> StopAsync()
    {
        Run? run;
        lock (sync) { run = current; if (run is not null) { Interlocked.Exchange(ref run.StopRequested, 1); run.Stop.TrySetResult(); } }
        if (run is null) return true;
        if (!await stopping.WaitAsync(TimeSpan.FromSeconds(9)).ConfigureAwait(false)) return false;
        try
        {
            try { await run.Completion.WaitAsync(TimeSpan.FromSeconds(9)).ConfigureAwait(false); }
            catch (TimeoutException) { return false; }
            catch { /* Reap and prove cleanup below. */ }
            lock (sync) if (current != run) return true;
            if (!await CleanupAsync(run).ConfigureAwait(false)) return false;
            Release(run);
            return true;
        }
        finally { stopping.Release(); }
    }

    private async Task RunAsync(Run run, string endpoint, string code)
    {
        bool clean = false;
        try
        {
            if (run.StopRequested != 0) throw new OperationCanceledException();
            if (!run.Process.Start()) throw Failed();
            run.Started = true;
            run.Output = ReadOutputAsync(run); run.Errors = DrainErrorsAsync(run);
            var exit = run.Process.WaitForExitAsync();
            await WriteAsync(run, new { op = "begin", endpoint, code }).ConfigureAwait(false);
            code = string.Empty;
            var all = Task.WhenAll(exit, run.Output, run.Errors);
            var first = await Task.WhenAny(all, run.Failed.Task, run.Stop.Task, Task.Delay(TimeSpan.FromSeconds(125))).ConfigureAwait(false);
            if (first != all || run.Failure != 0 || run.StopRequested != 0)
            {
                if (first != run.Stop.Task && run.StopRequested == 0) MarkFailed(run);
                try { await WriteAsync(run, new { op = "cancel" }).ConfigureAwait(false); } catch { }
                try { run.Process.StandardInput.Close(); } catch { }
            }
            else
            {
                await all.ConfigureAwait(false);
                if (run.Process.ExitCode != 0 || run.SavedId is null) MarkFailed(run);
            }
        }
        catch (OperationCanceledException) { Interlocked.Exchange(ref run.StopRequested, 1); }
        catch { MarkFailed(run); }
        finally
        {
            code = string.Empty;
            clean = await CleanupAsync(run).ConfigureAwait(false);
            if (!clean) MarkFailed(run);
            bool saved = clean && run.Failure == 0 && run.StopRequested == 0 && run.SavedId is not null;
            if (clean) Release(run);
            if (saved) run.Saved.TrySetResult(run.SavedId!);
            else if (run.StopRequested != 0) { run.Pending.TrySetCanceled(); run.Saved.TrySetCanceled(); }
            else { run.Pending.TrySetException(Failed()); run.Saved.TrySetException(Failed()); }
        }
    }

    private void Release(Run run)
    {
        lock (sync)
        {
            if (current != run) return;
            current = null;
        }
        run.Process.Dispose();
    }

    private static void MarkFailed(Run run)
    { Interlocked.Exchange(ref run.Failure, 1); run.Failed.TrySetResult(); }

    private static async Task WriteAsync(Run run, object command)
    {
        byte[] json = JsonSerializer.SerializeToUtf8Bytes(command);
        byte[] line = new byte[json.Length + 1]; json.CopyTo(line, 0); line[^1] = (byte)'\n';
        CryptographicOperations.ZeroMemory(json);
        async Task WriteCore()
        {
            if (!await run.Writes.WaitAsync(TimeSpan.FromSeconds(1)).ConfigureAwait(false)) throw Failed();
            try
            {
                await run.Process.StandardInput.BaseStream.WriteAsync(line).ConfigureAwait(false);
                await run.Process.StandardInput.BaseStream.FlushAsync().ConfigureAwait(false);
            }
            finally { run.Writes.Release(); }
        }
        var write = WriteCore();
        try { await write.WaitAsync(TimeSpan.FromSeconds(1)).ConfigureAwait(false); }
        finally
        {
            if (write.IsCompleted) CryptographicOperations.ZeroMemory(line);
            else _ = write.ContinueWith(_ => CryptographicOperations.ZeroMemory(line), CancellationToken.None, TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
        }
    }

    private async Task ReadOutputAsync(Run run)
    {
        var line = new StringBuilder(); var buffer = new char[512]; int total = 0;
        try
        {
            int count;
            while ((count = await run.Process.StandardOutput.ReadAsync(buffer.AsMemory()).ConfigureAwait(false)) != 0)
            {
                total += Encoding.UTF8.GetByteCount(buffer.AsSpan(0, count));
                if (total > MaxStreamBytes) throw Failed();
                for (int i = 0; i < count; i++)
                {
                    if (buffer[i] == '\n') { ApplyEvent(run, line.ToString().TrimEnd('\r')); line.Clear(); }
                    else { line.Append(buffer[i]); if (line.Length >= MaxLineBytes) throw Failed(); }
                }
                Array.Clear(buffer);
            }
            if (line.Length != 0 || run.SavedId is null) MarkFailed(run);
        }
        catch { MarkFailed(run); }
        finally { Array.Clear(buffer); line.Clear(); }
    }

    private static async Task DrainErrorsAsync(Run run)
    {
        // Discard immediately: no stderr tail that could retain a buggy secret echo.
        var buffer = new byte[1024]; int total = 0;
        try
        {
            int count;
            while ((count = await run.Process.StandardError.BaseStream.ReadAsync(buffer).ConfigureAwait(false)) != 0)
            { total += count; Array.Clear(buffer); if (total > MaxStreamBytes) throw Failed(); }
        }
        catch { MarkFailed(run); }
        finally { Array.Clear(buffer); }
    }

    private void ApplyEvent(Run run, string line)
    {
        if (Encoding.UTF8.GetByteCount(line) >= MaxLineBytes) throw Failed();
        using var document = JsonDocument.Parse(line, new JsonDocumentOptions { MaxDepth = 4 });
        var value = document.RootElement;
        if (value.ValueKind != JsonValueKind.Object) throw Failed();
        string kind = value.GetProperty("event").GetString() ?? throw Failed();
        lock (sync)
        {
            if (current != run || generation != run.Generation || run.StopRequested != 0) return;
            if (run.Failure != 0) throw Failed();
            if (kind == "pending")
            {
                Fields(value, "event", "name", "platform", "fingerprint");
                if (run.Fingerprint is not null || run.SavedId is not null) throw Failed();
                string name = value.GetProperty("name").GetString() ?? throw Failed();
                string platform = value.GetProperty("platform").GetString() ?? throw Failed();
                string fingerprint = value.GetProperty("fingerprint").GetString() ?? throw Failed();
                if (string.IsNullOrWhiteSpace(name) || Encoding.UTF8.GetByteCount(name) > 96 ||
                    name.Any(c => char.IsControl(c) || c is '\u061c' or '\u200e' or '\u200f' or >= '\u202a' and <= '\u202e' or >= '\u2066' and <= '\u2069') ||
                    platform is not ("windows" or "macos" or "linux" or "ios" or "ipados") || !IsId(fingerprint)) throw Failed();
                run.Fingerprint = fingerprint;
                run.Pending.TrySetResult(new(name, platform, fingerprint));
            }
            else if (kind == "saved")
            {
                Fields(value, "event", "id");
                string id = value.GetProperty("id").GetString() ?? throw Failed();
                if (!run.ApproveSent || run.SavedId is not null || !IsId(id) || id != run.Fingerprint) throw Failed();
                run.SavedId = id;
            }
            else if (kind == "error")
            {
                Fields(value, "event", "error");
                if (value.GetProperty("error").ValueKind != JsonValueKind.String || (value.GetProperty("error").GetString()?.Length ?? 0) > 128) throw Failed();
                MarkFailed(run);
            }
            else throw Failed();
        }
    }

    private static bool IsId(string value) => value.Length == 64 && value.All(c => c is >= '0' and <= '9' or >= 'a' and <= 'f');
    private static void Fields(JsonElement value, params string[] names)
    {
        var actual = value.EnumerateObject().Select(p => p.Name).ToArray();
        if (actual.Length != names.Length || actual.Distinct(StringComparer.Ordinal).Count() != names.Length || names.Except(actual, StringComparer.Ordinal).Any()) throw Failed();
    }

    private static async Task<bool> CleanupAsync(Run run)
    {
        if (!run.Started) return true;
        try
        {
            if (!run.Process.HasExited)
            {
                try { await run.Process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(2)).ConfigureAwait(false); }
                catch (TimeoutException)
                {
                    run.Process.Kill(entireProcessTree: true);
                    await run.Process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(3)).ConfigureAwait(false);
                }
            }
            if (!run.Process.HasExited) return false;
            // A descendant may retain stdout after its parent exits. Explicitly
            // close our read handles and bound both disposal and reader tasks.
            run.ClosePipes ??= Task.Run(() =>
            {
                try { run.Process.StandardInput.Close(); } catch { }
                try { run.Process.StandardOutput.Dispose(); } catch { }
                try { run.Process.StandardError.Dispose(); } catch { }
            });
            await run.ClosePipes.WaitAsync(TimeSpan.FromSeconds(1)).ConfigureAwait(false);
            await Task.WhenAll(run.Output ?? Task.CompletedTask, run.Errors ?? Task.CompletedTask)
                .WaitAsync(TimeSpan.FromSeconds(1)).ConfigureAwait(false);
            return true;
        }
        catch { return false; }
    }
}
