using System.Diagnostics;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Crossflow.Windows;

internal sealed record NetworkChannelInfo(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("label")] string Label,
    [property: JsonPropertyName("kind")] string Kind,
    [property: JsonPropertyName("ip")] string? Ip,
    [property: JsonPropertyName("interface_name")] string? InterfaceName);

internal sealed record PeerDisplayInfo(
    [property: JsonPropertyName("native_id")] string NativeId,
    [property: JsonPropertyName("wire_id")] uint WireId,
    [property: JsonPropertyName("x")] double X,
    [property: JsonPropertyName("y")] double Y,
    [property: JsonPropertyName("width")] double Width,
    [property: JsonPropertyName("height")] double Height);

internal sealed record DiscoveredPeerInfo(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("platform")] string Platform,
    [property: JsonPropertyName("connected")] bool Connected);

internal sealed record PeerSharedScreen(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("device_id")] string DeviceId,
    [property: JsonPropertyName("native_id")] string NativeId,
    [property: JsonPropertyName("x")] double X,
    [property: JsonPropertyName("y")] double Y,
    [property: JsonPropertyName("width")] double Width,
    [property: JsonPropertyName("height")] double Height);

internal sealed record PeerSharedLayout(
    [property: JsonPropertyName("schema")] int Schema,
    [property: JsonPropertyName("source_device_id")] string SourceDeviceId,
    [property: JsonPropertyName("receiver_device_id")] string ReceiverDeviceId,
    [property: JsonPropertyName("screens")] IReadOnlyList<PeerSharedScreen> Screens);

internal sealed record TransferStatusInfo(
    [property: JsonPropertyName("direction")] string Direction,
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("bytes_done")] ulong BytesDone,
    [property: JsonPropertyName("bytes_total")] ulong BytesTotal,
    [property: JsonPropertyName("files_done")] int FilesDone,
    [property: JsonPropertyName("files_total")] int FilesTotal,
    [property: JsonPropertyName("saved_dir")] string? SavedDir,
    [property: JsonPropertyName("error")] string? Error);

internal sealed record ManualPeerInfo(
    [property: JsonPropertyName("address")] string Address,
    [property: JsonPropertyName("label")] string? Label,
    [property: JsonPropertyName("channel")] string Channel);

internal sealed record PeerInfo(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("platform")] string Platform,
    [property: JsonPropertyName("connected")] bool Connected,
    [property: JsonPropertyName("focused")] bool Focused,
    [property: JsonPropertyName("slot")] int? Slot,
    [property: JsonPropertyName("address")] string? Address,
    [property: JsonPropertyName("active_channel_label")] string? ActiveChannelLabel,
    [property: JsonPropertyName("preferred_channel")] string PreferredChannel);

internal sealed record HotkeyConfigInfo(
    [property: JsonPropertyName("toggle_sharing")] string? ToggleSharing,
    [property: JsonPropertyName("lock_screen")] string? LockScreen,
    [property: JsonPropertyName("quick_jump_next")] string? QuickJumpNext,
    [property: JsonPropertyName("quick_jump_home")] string? QuickJumpHome,
    [property: JsonPropertyName("quick_jump_1")] string? QuickJump1,
    [property: JsonPropertyName("quick_jump_2")] string? QuickJump2,
    [property: JsonPropertyName("emergency_stop")] string? EmergencyStop)
{
    internal static HotkeyConfigInfo Defaults => new(
        "Ctrl+Alt+S",
        "Ctrl+Alt+L",
        "Ctrl+Alt+Tab",
        "Ctrl+Alt+H",
        "Ctrl+Alt+1",
        "Ctrl+Alt+2",
        "Ctrl+Alt+Shift+Esc");
}

internal sealed record PeerMeshSnapshot(
    string State,
    string? LocalDeviceId,
    string? LocalName,
    string AuthMode,
    bool PassphraseConfigured,
    bool SmartKeymap,
    bool ClipboardSync,
    bool InputStateSync,
    bool ScreenLocked,
    HotkeyConfigInfo Hotkeys,
    string SelectedChannel,
    string? DirectPeer,
    string? ActiveChannelLabel,
    IReadOnlyList<NetworkChannelInfo> NetworkChannels,
    IReadOnlyList<ManualPeerInfo> ManualPeers,
    IReadOnlyList<PeerInfo> Peers,
    bool RouteEnabled,
    string? FocusDeviceId,
    string? FocusScreenId,
    string? PeerDeviceId,
    string? PeerName,
    string? PeerAddress,
    string? ListenEndpoint,
    IReadOnlyList<PeerDisplayInfo> LocalDisplays,
    IReadOnlyList<DiscoveredPeerInfo> DiscoveredPeers,
    PeerSharedLayout? SharedLayout,
    TransferStatusInfo? Transfer,
    string? Error,
    bool ProcessRunning,
    long Generation)
{
    internal static PeerMeshSnapshot Initial => new(
        "idle", null, null, "open", false, true, true, true, false, HotkeyConfigInfo.Defaults, "auto", null, null, [], [], [],
        false, null, null, null, null, null, null,
        [], [], null, null, null, false, 0);
}

/// Manages one packaged `crossflow.exe peer` symmetric P2P LAN mesh process.
internal sealed class PeerMeshController
{
    private readonly object sync = new();
    private readonly SemaphoreSlim commands = new(1, 1);
    private Run? current;
    private long generation;
    private PeerMeshSnapshot snapshot = PeerMeshSnapshot.Initial;

    internal event Action<PeerMeshSnapshot>? Changed;
    internal PeerMeshSnapshot Snapshot { get { lock (sync) return snapshot; } }

    private sealed class Run(Process process, long generation)
    {
        internal readonly Process Process = process;
        internal readonly long Generation = generation;
        internal readonly SemaphoreSlim Writes = new(1, 1);
        internal int StopRequested;
        internal Task Completion = Task.CompletedTask;
    }

    internal async Task StartAsync(string? deviceName = null)
    {
        await commands.WaitAsync();
        try
        {
            await StopLockedAsync();
            string logDir = Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                "Crossflow", "Logs");
            Directory.CreateDirectory(logDir);
            string logPath = Path.Combine(logDir, $"peer-{DateTime.UtcNow:yyyyMMdd-HHmmss}-{Guid.NewGuid():N}.jsonl");

            string executable = Path.Combine(AppContext.BaseDirectory, "crossflow.exe");
            if (!File.Exists(executable))
                throw new InvalidOperationException("缺少随客户端提供的共享内核（crossflow.exe），请重新构建或安装。");

            var info = new ProcessStartInfo(executable)
            {
                WorkingDirectory = AppContext.BaseDirectory,
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardInput = true,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                StandardInputEncoding = new UTF8Encoding(false),
                StandardOutputEncoding = new UTF8Encoding(false, true),
                StandardErrorEncoding = new UTF8Encoding(false, false)
            };
            info.ArgumentList.Add("peer");
            info.ArgumentList.Add("--bind");
            info.ArgumentList.Add("0.0.0.0:24800");
            info.ArgumentList.Add("--seconds");
            info.ArgumentList.Add("86400");
            info.ArgumentList.Add("--max-hz");
            info.ArgumentList.Add("1000");
            info.ArgumentList.Add("--log");
            info.ArgumentList.Add(logPath);
            if (!string.IsNullOrWhiteSpace(deviceName))
            {
                info.ArgumentList.Add("--name");
                info.ArgumentList.Add(deviceName.Trim());
            }
            info.Environment.Remove("CROSSFLOW_LAN_PASSPHRASE");

            var process = new Process { StartInfo = info };
            Run run;
            lock (sync)
            {
                run = new Run(process, ++generation);
                current = run;
                snapshot = PeerMeshSnapshot.Initial with
                {
                    State = "starting",
                    ProcessRunning = true,
                    Generation = generation
                };
            }
            Publish();

            try
            {
                await Task.Run(() =>
                {
                    if (!process.Start())
                        throw new InvalidOperationException("无法启动 Crossflow 对等网络内核。");
                });
                process.StandardInput.AutoFlush = true;
                run.Completion = Task.Run(() => MonitorAsync(run));
            }
            catch
            {
                lock (sync)
                {
                    if (current == run)
                    {
                        current = null;
                        snapshot = snapshot with
                        {
                            State = "error",
                            Error = "无法启动共享内核，请检查客户端安装。",
                            ProcessRunning = false
                        };
                    }
                }
                process.Dispose();
                Publish();
                throw;
            }
        }
        finally
        {
            commands.Release();
        }
    }

    internal Task PauseAsync() => SendCommandAsync(new { op = "pause" });
    internal Task ResumeAsync() => SendCommandAsync(new { op = "resume" });
    internal Task ApplyLayoutAsync(PeerSharedLayout layout) => SendCommandAsync(new { op = "apply_layout", layout });
    internal Task SetChannelAsync(string channel, string? directPeer) =>
        SendCommandAsync(new { op = "set_channel", channel, direct_peer = string.IsNullOrWhiteSpace(directPeer) ? null : directPeer.Trim() });
    internal Task SetPeerChannelAsync(string deviceId, string channel) =>
        SendCommandAsync(new { op = "set_peer_channel", device_id = deviceId, channel });
    internal Task AddManualPeerAsync(string address, string? label = null, string channel = "auto") =>
        SendCommandAsync(new { op = "add_manual_peer", address = address.Trim(), label = string.IsNullOrWhiteSpace(label) ? null : label.Trim(), channel });
    internal Task RemoveManualPeerAsync(string address) =>
        SendCommandAsync(new { op = "remove_manual_peer", address = address.Trim() });
    internal Task SetPassphraseAsync(string? passphrase) =>
        SendCommandAsync(new { op = "set_passphrase", passphrase = string.IsNullOrWhiteSpace(passphrase) ? null : passphrase.Trim() });
    internal Task SetKeymapAsync(bool enabled) =>
        SendCommandAsync(new { op = "set_keymap", enabled });
    internal Task SetClipboardAsync(bool enabled) =>
        SendCommandAsync(new { op = "set_clipboard", enabled });
    internal Task SetInputStateSyncAsync(bool enabled) =>
        SendCommandAsync(new { op = "set_input_state_sync", enabled });
    internal Task SetHotkeysAsync(HotkeyConfigInfo hotkeys) =>
        SendCommandAsync(new { op = "set_hotkeys", hotkeys });
    internal Task SetHotkeySuspendedAsync(bool suspended) =>
        SendCommandAsync(new { op = "set_hotkey_suspended", suspended });
    internal Task SetScreenLockAsync(bool locked) =>
        SendCommandAsync(new { op = "set_screen_lock", locked });
    internal Task QuickJumpAsync(string? targetDeviceId) =>
        SendCommandAsync(new { op = "quick_jump", target_device_id = targetDeviceId });
    internal Task SendFilesAsync(IReadOnlyList<string> paths, string? targetDeviceId = null) =>
        SendCommandAsync(new { op = "send_files", target_device_id = targetDeviceId, paths });
    internal Task CancelTransferAsync() =>
        SendCommandAsync(new { op = "cancel_transfer" });

    private async Task SendCommandAsync(object payload)
    {
        Run? run;
        lock (sync) run = current;
        if (run is null) return;
        string json = JsonSerializer.Serialize(payload);
        await run.Writes.WaitAsync();
        try
        {
            if (Volatile.Read(ref run.StopRequested) != 0) return;
            await run.Process.StandardInput.WriteLineAsync(json);
            await run.Process.StandardInput.FlushAsync();
        }
        catch { }
        finally { run.Writes.Release(); }
    }

    internal async Task<bool> StopAsync()
    {
        await commands.WaitAsync();
        try { return await StopLockedAsync(); }
        finally { commands.Release(); }
    }

    private async Task<bool> StopLockedAsync()
    {
        Run? run;
        lock (sync) run = current;
        if (run is null) return true;
        if (Interlocked.Exchange(ref run.StopRequested, 1) == 0)
        {
            await run.Writes.WaitAsync();
            try
            {
                await run.Process.StandardInput.WriteLineAsync("{\"op\":\"stop\"}");
                await run.Process.StandardInput.FlushAsync();
            }
            catch { }
            try { run.Process.StandardInput.Close(); } catch { }
            run.Writes.Release();
        }
        try { await run.Completion.WaitAsync(TimeSpan.FromSeconds(4)); }
        catch (TimeoutException)
        {
            try { if (!run.Process.HasExited) run.Process.Kill(entireProcessTree: true); } catch { }
            try { await run.Completion.WaitAsync(TimeSpan.FromSeconds(2)); } catch { return false; }
        }
        lock (sync) return current != run;
    }

    private async Task MonitorAsync(Run run)
    {
        var stderrTask = Task.Run(async () =>
        {
            try
            {
                while (await run.Process.StandardError.ReadLineAsync() is not null) { }
            }
            catch { }
        });

        try
        {
            while (await run.Process.StandardOutput.ReadLineAsync() is { } line)
            {
                if (string.IsNullOrWhiteSpace(line)) continue;
                try
                {
                    var parsed = JsonSerializer.Deserialize<PeerStatusWire>(line);
                    if (parsed is null || parsed.Protocol != 3) continue;
                    lock (sync)
                    {
                        if (current != run) return;
                        snapshot = new PeerMeshSnapshot(
                            parsed.State ?? "searching",
                            parsed.LocalDeviceId,
                            parsed.LocalName,
                            parsed.AuthMode ?? "open",
                            parsed.PassphraseConfigured,
                            parsed.SmartKeymap,
                            parsed.ClipboardSync,
                            parsed.InputStateSync,
                            parsed.ScreenLocked,
                            parsed.Hotkeys ?? HotkeyConfigInfo.Defaults,
                            parsed.SelectedChannel ?? "auto",
                            parsed.DirectPeer,
                            parsed.ActiveChannelLabel,
                            parsed.NetworkChannels ?? [],
                            parsed.ManualPeers ?? [],
                            parsed.Peers ?? [],
                            parsed.RouteEnabled,
                            parsed.FocusDeviceId,
                            parsed.FocusScreenId,
                            parsed.PeerDeviceId,
                            parsed.PeerName,
                            parsed.PeerAddress,
                            parsed.ListenEndpoint,
                            parsed.LocalDisplays ?? [],
                            parsed.DiscoveredPeers ?? [],
                            parsed.SharedLayout,
                            parsed.Transfer,
                            parsed.Error,
                            ProcessRunning: parsed.State is not ("stopped" or "error"),
                            run.Generation);
                    }
                    Publish();
                }
                catch { }
            }
            await run.Process.WaitForExitAsync();
            await stderrTask;
        }
        catch { }
        finally
        {
            bool stoppedByUser = Volatile.Read(ref run.StopRequested) != 0;
            lock (sync)
            {
                if (current == run)
                {
                    current = null;
                    snapshot = snapshot with
                    {
                        State = stoppedByUser ? "stopped" : (snapshot.State == "error" ? "error" : "stopped"),
                        RouteEnabled = false,
                        FocusDeviceId = null,
                        FocusScreenId = null,
                        ProcessRunning = false
                    };
                }
            }
            try { run.Process.Dispose(); } catch { }
            Publish();
        }
    }

    private void Publish()
    {
        PeerMeshSnapshot currentSnapshot;
        lock (sync) currentSnapshot = snapshot;
        Changed?.Invoke(currentSnapshot);
    }

    private sealed class PeerStatusWire
    {
        [JsonPropertyName("protocol")] public int Protocol { get; set; }
        [JsonPropertyName("state")] public string? State { get; set; }
        [JsonPropertyName("local_device_id")] public string? LocalDeviceId { get; set; }
        [JsonPropertyName("local_name")] public string? LocalName { get; set; }
        [JsonPropertyName("auth_mode")] public string? AuthMode { get; set; }
        [JsonPropertyName("passphrase_configured")] public bool PassphraseConfigured { get; set; }
        [JsonPropertyName("smart_keymap")] public bool SmartKeymap { get; set; } = true;
        [JsonPropertyName("clipboard_sync")] public bool ClipboardSync { get; set; } = true;
        [JsonPropertyName("input_state_sync")] public bool InputStateSync { get; set; } = true;
        [JsonPropertyName("screen_locked")] public bool ScreenLocked { get; set; }
        [JsonPropertyName("hotkeys")] public HotkeyConfigInfo? Hotkeys { get; set; }
        [JsonPropertyName("selected_channel")] public string? SelectedChannel { get; set; }
        [JsonPropertyName("direct_peer")] public string? DirectPeer { get; set; }
        [JsonPropertyName("active_channel_label")] public string? ActiveChannelLabel { get; set; }
        [JsonPropertyName("network_channels")] public List<NetworkChannelInfo>? NetworkChannels { get; set; }
        [JsonPropertyName("manual_peers")] public List<ManualPeerInfo>? ManualPeers { get; set; }
        [JsonPropertyName("peers")] public List<PeerInfo>? Peers { get; set; }
        [JsonPropertyName("route_enabled")] public bool RouteEnabled { get; set; }
        [JsonPropertyName("focus_device_id")] public string? FocusDeviceId { get; set; }
        [JsonPropertyName("focus_screen_id")] public string? FocusScreenId { get; set; }
        [JsonPropertyName("peer_device_id")] public string? PeerDeviceId { get; set; }
        [JsonPropertyName("peer_name")] public string? PeerName { get; set; }
        [JsonPropertyName("peer_address")] public string? PeerAddress { get; set; }
        [JsonPropertyName("listen_endpoint")] public string? ListenEndpoint { get; set; }
        [JsonPropertyName("local_displays")] public List<PeerDisplayInfo>? LocalDisplays { get; set; }
        [JsonPropertyName("discovered_peers")] public List<DiscoveredPeerInfo>? DiscoveredPeers { get; set; }
        [JsonPropertyName("shared_layout")] public PeerSharedLayout? SharedLayout { get; set; }
        [JsonPropertyName("transfer")] public TransferStatusInfo? Transfer { get; set; }
        [JsonPropertyName("error")] public string? Error { get; set; }
    }
}
