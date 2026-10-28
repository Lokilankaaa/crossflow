using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

internal sealed record NearbyDevice(string Id, string Name, string Platform, ushort Protocol, bool Compatible, IReadOnlyList<string> Addresses);
internal sealed record DiscoveryResult(IReadOnlyList<NearbyDevice> Devices, bool CapacityLimited);

/// Owns a short-lived discovery-only child. Discovery never starts input or writes trust.
internal sealed class DiscoveryClient
{
    internal const int MaxOutputBytes = 262144;
    private readonly object sync = new();
    private readonly Func<ProcessStartInfo>? fixture;
    private readonly Func<Stream, Stream>? fixtureOutput;
    private readonly TimeSpan timeout;
    private CancellationTokenSource? cancellation;
    private Task<DiscoveryResult>? active;
    private Process? process;
    private Task? pipeCleanup;

    internal DiscoveryClient() => timeout = TimeSpan.FromSeconds(8);
    internal DiscoveryClient(Func<ProcessStartInfo> fixture, TimeSpan timeout, Func<Stream, Stream>? fixtureOutput = null)
    { this.fixture = fixture; this.timeout = timeout; this.fixtureOutput = fixtureOutput; }

    internal Task<DiscoveryResult> ScanAsync()
    {
        lock (sync)
        {
            if (active is { IsCompleted: false } || process is not null || pipeCleanup is { IsCompleted: false })
                throw new InvalidOperationException("上一次扫描尚未退出，请稍候。");
            cancellation?.Dispose();
            cancellation = new CancellationTokenSource();
            var token = cancellation.Token;
            // All process supervision runs outside the XAML synchronization context.
            active = Task.Run(() => ScanCoreAsync(token));
            return active;
        }
    }

    internal void Cancel() { lock (sync) cancellation?.Cancel(); }

    internal async Task<bool> StopAsync()
    {
        Task<DiscoveryResult>? task;
        lock (sync) { cancellation?.Cancel(); task = active; }
        if (task is not null)
        {
            try { await task.WaitAsync(TimeSpan.FromSeconds(5)).ConfigureAwait(false); }
            catch (TimeoutException) { return false; }
            catch { /* Cleanup confirmation below. */ }
        }
        Task? readers; Process? remaining; lock (sync) { readers = pipeCleanup; remaining = process; }
        if (readers is not null && !await CleanupFinishedAsync(readers).ConfigureAwait(false)) return false;
        if (remaining is null) return true;
        if (!await ReapAsync(remaining).ConfigureAwait(false)) return false;
        lock (sync) { if (process == remaining) process = null; }
        remaining.Dispose();
        return true;
    }

    private static ProcessStartInfo PackagedStart()
    {
        string executable = Path.Combine(AppContext.BaseDirectory, "crossflow.exe");
        if (!File.Exists(executable)) throw new InvalidOperationException("缺少发现内核，请重新构建或安装客户端。");
        var info = new ProcessStartInfo(executable) { WorkingDirectory = AppContext.BaseDirectory };
        foreach (var argument in new[] { "discover", "--seconds", "3" }) info.ArgumentList.Add(argument);
        return info;
    }

    private async Task<DiscoveryResult> ScanCoreAsync(CancellationToken cancellationToken)
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
            if (!child.Start()) throw new InvalidOperationException("无法启动附近设备扫描。");
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
            if (child.ExitCode != 0) throw new InvalidOperationException("附近设备扫描失败，请检查网络和防火墙后重试。");
            return Parse(await output.ConfigureAwait(false));
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        { throw new InvalidOperationException("扫描超时，请稍后刷新附近设备。"); }
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
            else throw new InvalidOperationException("扫描清理尚未完成，请重试停止或关闭。");
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
            if (output.Length + count > MaxOutputBytes) throw new InvalidOperationException("发现结果超过安全大小限制。");
            output.Write(buffer, 0, count);
        }
    }

    private static async Task DrainErrorsAsync(Stream source, CancellationToken token)
    {
        // No stderr strings are interpreted as candidate/status or retained.
        byte[] buffer = new byte[4096];
        while (await source.ReadAsync(buffer, token).ConfigureAwait(false) != 0) { }
    }

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
