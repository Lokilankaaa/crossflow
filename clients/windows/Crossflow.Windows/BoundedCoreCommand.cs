using System.Diagnostics;

namespace Crossflow.Windows;

/// Shared read-only CLI worker. Callers choose fixed commands, never user shell text.
internal sealed class BoundedCoreCommand<T>
{
    internal const int MaxOutputBytes = 262144;
    private readonly object sync = new();
    private readonly Func<ProcessStartInfo>? fixture;
    private readonly Func<Stream, Stream>? fixtureOutput;
    private readonly TimeSpan timeout;
    private CancellationTokenSource? cancellation;
    private Task<T>? active;
    private Process? process;
    private Task? pipeCleanup;

    private readonly string[] arguments;
    private readonly Func<byte[], T> parser;
    private readonly SemaphoreSlim stopping = new(1, 1);
    private long generation;
    internal BoundedCoreCommand(string[] arguments, Func<byte[], T> parser, Func<ProcessStartInfo>? fixture = null, TimeSpan? timeout = null, Func<Stream, Stream>? fixtureOutput = null)
    { this.arguments = arguments; this.parser = parser; this.fixture = fixture; this.timeout = timeout ?? TimeSpan.FromSeconds(8); this.fixtureOutput = fixtureOutput; }

    internal Task<T> RunAsync()
    {
        lock (sync)
        {
            if (active is { IsCompleted: false } || process is not null || pipeCleanup is { IsCompleted: false })
                throw new InvalidOperationException("上一次读取任务尚未退出，请稍候。");
            cancellation?.Dispose();
            cancellation = new CancellationTokenSource();
            generation++;
            var token = cancellation.Token;
            // All process supervision runs outside the XAML synchronization context.
            active = Task.Run(() => ScanCoreAsync(token));
            return active;
        }
    }

    internal void Cancel() { lock (sync) cancellation?.Cancel(); }

    internal async Task<bool> StopAsync()
    {
        Task<T>? task; long target;
        lock (sync) { cancellation?.Cancel(); task = active; target = generation; }
        if (!await stopping.WaitAsync(TimeSpan.FromSeconds(5)).ConfigureAwait(false)) return false;
        try
        {
            if (task is not null)
            {
                try { await task.WaitAsync(TimeSpan.FromSeconds(5)).ConfigureAwait(false); }
                catch (TimeoutException) { return false; }
                catch { /* Cleanup confirmation below. */ }
            }
            Task? readers; Process? remaining;
            lock (sync)
            {
                // A delayed stop for an old page must never reap a newer read.
                if (generation != target) return true;
                readers = pipeCleanup; remaining = process;
            }
            if (readers is not null && !await CleanupFinishedAsync(readers).ConfigureAwait(false)) return false;
            if (remaining is null) return true;
            if (!await ReapAsync(remaining).ConfigureAwait(false)) return false;
            lock (sync) { if (process == remaining) process = null; }
            remaining.Dispose();
            return true;
        }
        finally { stopping.Release(); }
    }

    private ProcessStartInfo PackagedStart()
    {
        string executable = Path.Combine(AppContext.BaseDirectory, "crossflow.exe");
        if (!File.Exists(executable)) throw new InvalidOperationException("缺少随客户端提供的内核，请重新构建或安装客户端。");
        var info = new ProcessStartInfo(executable) { WorkingDirectory = AppContext.BaseDirectory };
        foreach (var argument in arguments) info.ArgumentList.Add(argument);
        return info;
    }

    private async Task<T> ScanCoreAsync(CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        var info = fixture?.Invoke() ?? PackagedStart();
        info.UseShellExecute = false; info.CreateNoWindow = true;
        info.RedirectStandardInput = true; info.RedirectStandardOutput = true; info.RedirectStandardError = true;
        var child = new Process { StartInfo = info };
        using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        deadline.CancelAfter(timeout);
        Task<byte[]>? output = null;
        Task? errors = null;
        Stream? outputStream = null, errorStream = null;
        bool started = false;
        try
        {
            cancellationToken.ThrowIfCancellationRequested();
            if (!child.Start()) throw new InvalidOperationException("无法启动设备信息读取任务。");
            started = true;
            lock (sync) process = child;
            child.StandardInput.Close();
            outputStream = fixtureOutput?.Invoke(child.StandardOutput.BaseStream) ?? child.StandardOutput.BaseStream;
            errorStream = child.StandardError.BaseStream;
            output = ReadBoundedAsync(outputStream, deadline.Token);
            errors = DrainErrorsAsync(errorStream, deadline.Token);
            var exited = child.WaitForExitAsync(deadline.Token);
            // Do not await only exit: a malformed/flooding stdout must abort promptly.
            var pending = new List<Task> { output, errors, exited };
            while (pending.Count > 0)
            {
                var finished = await Task.WhenAny(pending).WaitAsync(deadline.Token).ConfigureAwait(false);
                await finished.ConfigureAwait(false);
                pending.Remove(finished);
            }
            if (child.ExitCode != 0) throw new InvalidOperationException("无法读取设备信息，请稍后重试；若持续失败，请检查客户端与已保存记录。");
            return parser(await output.ConfigureAwait(false));
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        { throw new InvalidOperationException("读取任务超时，请稍后刷新设备信息。"); }
        finally
        {
            deadline.Cancel();
            // Closing our read ends must not inherit an unbounded stream Dispose
            // or ReadAsync. Observe all tasks, retain cleanup until finished, and
            // do not allow another scan while these resources remain outstanding.
            var readers = CloseReadersAsync(outputStream, errorStream, output, errors);
            lock (sync) pipeCleanup = readers;
            bool reaped = !started || await ReapAsync(child).ConfigureAwait(false);
            bool readersFinished = await CleanupFinishedAsync(readers).ConfigureAwait(false);
            if (reaped && readersFinished)
            {
                lock (sync) { if (process == child) process = null; }
                child.Dispose();
            }
            else throw new InvalidOperationException("读取任务清理尚未完成，请重试停止或关闭。");
        }
    }

    private static async Task CloseReadersAsync(Stream? outputStream, Stream? errorStream, Task? output, Task? errors)
    {
        static async Task Observe(Task task) { try { await task.ConfigureAwait(false); } catch { } }
        var tasks = new List<Task>();
        if (outputStream is not null) tasks.Add(Observe(Task.Run(outputStream.Dispose)));
        if (errorStream is not null) tasks.Add(Observe(Task.Run(errorStream.Dispose)));
        if (output is not null) tasks.Add(Observe(output));
        if (errors is not null) tasks.Add(Observe(errors));
        await Task.WhenAll(tasks).ConfigureAwait(false);
    }

    private static async Task<bool> CleanupFinishedAsync(Task cleanup)
    {
        try { await cleanup.WaitAsync(TimeSpan.FromMilliseconds(500)).ConfigureAwait(false); return true; }
        catch (TimeoutException) { return false; }
    }

    private static async Task<bool> ReapAsync(Process child)
    {
        try
        {
            if (!child.HasExited) child.Kill(entireProcessTree: true);
            await child.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(3)).ConfigureAwait(false);
            return child.HasExited;
        }
        catch { return false; }
    }

    private static async Task<byte[]> ReadBoundedAsync(Stream source, CancellationToken token)
    {
        using var output = new MemoryStream();
        byte[] buffer = new byte[4096];
        while (true)
        {
            int count = await source.ReadAsync(buffer, token).ConfigureAwait(false);
            if (count == 0) return output.ToArray();
            if (output.Length + count > MaxOutputBytes) throw new InvalidOperationException("内核输出超过安全大小限制。");
            output.Write(buffer, 0, count);
        }
    }

    private static async Task DrainErrorsAsync(Stream source, CancellationToken token)
    {
        // No stderr strings are interpreted as candidate/status or retained.
        byte[] buffer = new byte[4096];
        while (await source.ReadAsync(buffer, token).ConfigureAwait(false) != 0) { }
    }

}
