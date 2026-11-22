using Microsoft.UI.Xaml;

namespace Crossflow.Windows;

internal sealed partial class MainWindow
{
    private bool runtimeBusy;
    private SavedPeer? runtimePeer;
    private RuntimeLayoutDocument? runtimeLayout;
    private DisplayClient? localDisplays, remoteDisplays;
    private readonly Dictionary<string, string> runtimeScreenNames = new(StringComparer.Ordinal);
    private (long Generation, string Error)? shownSessionError;

    private bool CanEditLayout => !closing && !runtimeBusy && !sessionAction && !pairingBusy && !session.Snapshot.ProcessRunning;

    private string FocusDescription(SessionSnapshot state)
    {
        if (!state.RouteEnabled) return "";
        string device = state.FocusDeviceId == "local" ? "本机 Windows"
            : state.FocusDeviceId is null ? "正在定位"
            : state.FocusDeviceId == runtimePeer?.Id ? runtimePeer.Name : "远端设备";
        string? screen = state.FocusScreenId is { } id && runtimeScreenNames.TryGetValue(id, out var name) ? name : null;
        return "当前控制：" + (screen is null ? device : $"{device}（{screen}）");
    }

    private async Task<bool> StopDisplayReadsAsync()
    {
        var pending = new List<Task<bool>>();
        if (localDisplays is not null) pending.Add(localDisplays.StopAsync());
        if (remoteDisplays is not null) pending.Add(remoteDisplays.StopAsync());
        return (await Task.WhenAll(pending)).All(x => x);
    }

    private async void PrepareRuntimeLayout(SavedPeer peer, bool silentIfOffline)
    {
        if (!CanEditLayout || !coreReady || busy) return;
        runtimeBusy = true;
        UpdateSessionUi();
        Draw();
        try
        {
            if (!await StopDisplayReadsAsync())
                throw new InvalidOperationException("上次屏幕读取尚未结束，请稍后重试。");
            if (closing) return;

            localDisplays = new();
            remoteDisplays = new(peer.Id);
            var localTask = localDisplays.ReadAsync();
            var remoteTask = remoteDisplays.ReadAsync();
            await Task.WhenAll(localTask, remoteTask);
            if (closing) return;

            var detected = RuntimeLayouts.Create(peer.Id, await localTask, await remoteTask);
            RuntimeLayoutDocument? restored = null;
            try { restored = await Task.Run(() => RuntimeLayouts.Restore(detected)); }
            catch { }

            var candidate = restored ?? detected;
            try { await Task.Run(() => CoreBridge.Validate(candidate.Layout)); }
            catch when (restored is not null)
            {
                candidate = detected;
                await Task.Run(() => CoreBridge.Validate(candidate.Layout));
                restored = null;
            }
            if (closing) return;

            runtimePeer = peer;
            runtimeLayout = candidate;
            layout = candidate.Layout;
            layoutHistory.Clear();
            revision++;
            selectedScreenId = layout.Screens.FirstOrDefault()?.Id;
            runtimeScreenNames.Clear();
            foreach (var group in layout.Screens.GroupBy(x => x.DeviceId))
            {
                int index = 0;
                foreach (var screen in group)
                    runtimeScreenNames[screen.Id] = $"{(group.Key == "local" ? "Windows" : peer.Name)} · 屏幕 {++index}";
            }
            ClearNotice();
            RenderPairedDevices();
            RefreshScreenList();
            Draw();
            Inspect();
        }
        catch (Exception error)
        {
            if (!silentIfOffline)
            {
                string message = error is BridgeValidationException
                    ? "检测到镜像或重叠显示器，请改为扩展显示后刷新屏幕。"
                    : $"暂时连不上 {peer.Name}，请确认 Mac 已打开 Crossflow 后点击「刷新屏幕」。";
                ShowNotice(message, isError: true);
            }
        }
        finally
        {
            runtimeBusy = false;
            UpdateSessionUi();
            Draw();
        }
    }

    private async void ApplyRuntimeLayout()
    {
        if (!CanEditLayout || busy || !coreReady || runtimeLayout is null || runtimePeer is null) return;
        runtimeBusy = true;
        UpdateSessionUi();
        try
        {
            var config = RuntimeLayouts.WithLayout(runtimeLayout, layout);
            await Task.Run(() => CoreBridge.Validate(config.Layout));
            if (closing) return;
            string path = await Task.Run(() => RuntimeLayouts.SaveSnapshot(config));
            if (closing) return;
            ClearNotice();
            await session.StartRoutedAsync(runtimePeer.Id, path);
        }
        catch (Exception error)
        {
            ShowNotice(Friendly(error), isError: true);
        }
        finally
        {
            runtimeBusy = false;
            UpdateSessionUi();
        }
    }

    private void UpdateRuntimeControls()
    {
        bool editable = CanEditLayout && !busy;
        if (undoLayout is not null) undoLayout.IsEnabled = editable && coreReady && layoutHistory.CanUndo;
        if (redoLayout is not null) redoLayout.IsEnabled = editable && coreReady && layoutHistory.CanRedo;
        if (refreshScreensButton is not null) refreshScreensButton.IsEnabled = editable && coreReady;
        if (propertiesScroll is not null) propertiesScroll.IsEnabled = editable;
        if (!editable && dragOriginal is not null) CancelDrag();
    }
}
