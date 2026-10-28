using System.Diagnostics;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

/// Explicit non-GUI fixtures; no discovery sockets or OS input are opened.
internal static class DiscoverySelfTest
{
    private const string Valid = """
        {"state":"browsing","candidates":[{"device_id":"0123456789abcdef0123456789abcdef","friendly_name":"测试 Mac","platform":"macos","protocol":2,"compatible":true,"trusted":false,"addresses":["192.168.1.2:24800","[fe80::123%7]:24800"]}],"capacity_limited":false,"error":null}
        """;

    internal static async Task FixtureAsync(string mode)
    {
        Console.OutputEncoding = new UTF8Encoding(false);
        if (mode == "silent") { await Task.Delay(TimeSpan.FromMinutes(1)); return; }
        if (mode == "oversized") { await Console.Out.WriteAsync(new string('x', DiscoveryClient.MaxOutputBytes + 1)); await Console.Out.FlushAsync(); await Task.Delay(TimeSpan.FromMinutes(1)); return; }
        if (mode == "stderr") { await Console.Error.WriteAsync(new string('e', 131072)); await Console.Error.FlushAsync(); }
        if (mode == "invalid") await Console.Out.WriteAsync("{}");
        else await Console.Out.WriteAsync(Valid);
        await Console.Out.FlushAsync();
        if (mode == "nonzero") Environment.ExitCode = 1;
    }

    private static DiscoveryClient Client(string mode, int milliseconds = 3000, Func<Stream, Stream>? output = null) => new(() =>
    {
        var info = new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, "Crossflow.Windows.exe"));
        info.ArgumentList.Add("--discovery-fixture"); info.ArgumentList.Add(mode);
        return info;
    }, TimeSpan.FromMilliseconds(milliseconds), output);

    private sealed class LateCloseStream(Stream inner) : Stream
    {
        internal readonly TaskCompletionSource Release = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal readonly TaskCompletionSource ReadStarted = new(TaskCreationOptions.RunContinuationsAsynchronously);
        internal readonly TaskCompletionSource Closed = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
        public override async ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken cancellationToken = default)
        {
            ReadStarted.TrySetResult();
            // Deliberately ignore cancellation, like a late-completing pipe read.
            await Release.Task.ConfigureAwait(false);
            return 0;
        }
        protected override void Dispose(bool disposing)
        {
            if (disposing) { Release.Task.GetAwaiter().GetResult(); inner.Dispose(); Closed.TrySetResult(); }
            base.Dispose(disposing);
        }
        public override void Flush() => throw new NotSupportedException();
        public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
    }

    internal static async Task RunAsync(string path)
    {
        var checks = new Dictionary<string, bool>();
        try
        {
            var result = DiscoveryClient.Parse(Encoding.UTF8.GetBytes(Valid));
            checks["untrusted_scoped_candidates"] = result.Devices.Count == 1 && result.Devices[0].Addresses.Contains("[fe80::123%7]:24800");
            checks["trusted_rejected"] = Reject(Valid.Replace("\"trusted\":false", "\"trusted\":true"));
            checks["duplicate_fields_rejected"] = Reject(Valid.Replace("\"state\":\"browsing\"", "\"state\":\"browsing\",\"state\":\"browsing\""));
            checks["unknown_fields_rejected"] = Reject(Valid.Replace("\"error\":null", "\"error\":null,\"unexpected\":1"));
            checks["unsafe_address_rejected"] = Reject(Valid.Replace("192.168.1.2:24800", "127.0.0.1:24800")) && Reject(Valid.Replace("fe80::123%7", "fe80::123"));
            checks["bidi_name_rejected"] = Reject(Valid.Replace("测试 Mac", "\u202ehidden"));
            checks["nonready_and_error_rejected"] = Reject(Valid.Replace("browsing", "starting")) && Reject(Valid.Replace("\"error\":null", "\"error\":\"mdns_start_failed\""));
            try { DiscoveryClient.Parse([0xff]); checks["invalid_utf8_rejected"] = false; } catch (InvalidOperationException) { checks["invalid_utf8_rejected"] = true; }
            using var json = JsonDocument.Parse(Valid);
            var candidate = json.RootElement.GetProperty("candidates")[0];
            checks["duplicate_candidates_rejected"] = Reject(JsonSerializer.Serialize(new { state = "browsing", candidates = new[] { candidate, candidate }, capacity_limited = false, error = (string?)null }));
            checks["candidate_cap_rejected"] = Reject(JsonSerializer.Serialize(new { state = "browsing", candidates = Enumerable.Repeat(candidate, 65), capacity_limited = false, error = (string?)null }));
            foreach (string mode in new[] { "valid", "stderr" })
            {
                var client = Client(mode);
                try { checks[mode + "_process"] = (await client.ScanAsync()).Devices.Count == 1; }
                finally { checks[mode + "_reaped"] = await client.StopAsync(); }
            }
            foreach (string mode in new[] { "invalid", "nonzero", "oversized", "silent" })
            {
                var client = Client(mode, mode == "silent" ? 900 : 3000);
                var timer = Stopwatch.StartNew();
                try { await client.ScanAsync(); checks[mode + "_fails"] = false; }
                catch (InvalidOperationException) { checks[mode + "_fails"] = true; }
                finally { checks[mode + "_reaped"] = await client.StopAsync(); }
                checks[mode + "_bounded"] = timer.Elapsed < TimeSpan.FromSeconds(7);
            }
            var cancelled = Client("silent");
            var scan = cancelled.ScanAsync();
            await Task.Delay(100);
            cancelled.Cancel();
            try { await scan; checks["cancelled"] = false; } catch (OperationCanceledException) { checks["cancelled"] = true; }
            checks["cancel_reaped"] = await cancelled.StopAsync();
            LateCloseStream? delayed = null;
            var late = Client("valid", 900, stream => delayed = new LateCloseStream(stream));
            try
            {
                var timer = Stopwatch.StartNew();
                try { await late.ScanAsync().WaitAsync(TimeSpan.FromSeconds(4)); checks["late_pipe_fails"] = false; }
                catch (InvalidOperationException) { checks["late_pipe_fails"] = true; }
                checks["late_pipe_scan_bounded"] = timer.Elapsed < TimeSpan.FromSeconds(3);
                checks["late_pipe_read_started"] = delayed?.ReadStarted.Task.IsCompleted == true;
                timer.Restart();
                checks["late_pipe_stop_not_falsely_confirmed"] = !await late.StopAsync().WaitAsync(TimeSpan.FromSeconds(2));
                checks["late_pipe_stop_bounded"] = timer.Elapsed < TimeSpan.FromSeconds(2);
                try { _ = late.ScanAsync(); checks["late_pipe_no_second_scan"] = false; }
                catch (InvalidOperationException) { checks["late_pipe_no_second_scan"] = true; }
            }
            finally
            {
                delayed?.Release.TrySetResult();
                if (delayed is not null) await delayed.Closed.Task.WaitAsync(TimeSpan.FromSeconds(2));
                checks["late_pipe_reaped_after_release"] = await late.StopAsync();
            }
            File.WriteAllText(path, JsonSerializer.Serialize(new { ok = checks.Values.All(v => v), actual_os_input = false, actual_discovery = false, checks }, CoreBridge.Json));
            Environment.ExitCode = checks.Values.All(v => v) ? 0 : 1;
        }
        catch (Exception error)
        {
            File.WriteAllText(path, JsonSerializer.Serialize(new { ok = false, actual_os_input = false, actual_discovery = false, checks, error = error.Message }, CoreBridge.Json));
            Environment.ExitCode = 1;
        }
    }

    private static bool Reject(string value)
    {
        try { DiscoveryClient.Parse(Encoding.UTF8.GetBytes(value)); return false; }
        catch (InvalidOperationException) { return true; }
    }
}
