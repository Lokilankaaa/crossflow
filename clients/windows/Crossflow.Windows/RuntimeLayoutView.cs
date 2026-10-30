using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Crossflow.Windows;

internal sealed partial class MainWindow
{
    private bool runtimeBusy;
    private SavedPeer? runtimePeer;
    private RuntimeLayoutDocument? runtimeLayout;
    private DisplayClient? localDisplays, remoteDisplays;
    private readonly Dictionary<string, string> runtimeScreenNames = new(StringComparer.Ordinal);
    private Button? applyRuntime, refreshScreens;
    private (long Generation, string Error)? shownSessionError;
    private bool CanEditLayout => !closing && !runtimeBusy && !sessionAction && !pairingBusy && !session.Snapshot.ProcessRunning;

    private async Task<bool> StopDisplayReadsAsync()
    {
        var pending = new List<Task<bool>>();
        if (localDisplays is not null) pending.Add(localDisplays.StopAsync());
        if (remoteDisplays is not null) pending.Add(remoteDisplays.StopAsync());
        return (await Task.WhenAll(pending)).All(x => x);
    }

    private async void PrepareRuntimeLayout(SavedPeer peer)
    {
        if (!CanEditLayout || !coreReady || busy) return;
        runtimeBusy = true; UpdateSessionUi();
        string origin = activePage;
        if (peerStatus is not null) peerStatus.Text = "正在读取两端屏幕，请保持 Mac 接收开启…";
        Message("正在读取两端屏幕。读取完成后可排列屏幕，输入仍保持关闭。");
        try
        {
            if (!await StopDisplayReadsAsync()) throw new InvalidOperationException("上次屏幕读取尚未退出，请稍后重试。");
            if (closing) return;
            localDisplays = new(); remoteDisplays = new(peer.Id);
            var localTask = localDisplays.ReadAsync(); var remoteTask = remoteDisplays.ReadAsync();
            await Task.WhenAll(localTask, remoteTask);
            if (closing) return;
            var detected = RuntimeLayouts.Create(peer.Id, await localTask, await remoteTask);
            RuntimeLayoutDocument? restored = null;
            try { restored = await Task.Run(() => RuntimeLayouts.Restore(detected)); }
            catch { /* An invalid saved preset never replaces newly observed displays. */ }
            var candidate = restored ?? detected;
            try { await Task.Run(() => CoreBridge.Validate(candidate.Layout)); }
            catch when (restored is not null) { candidate = detected; await Task.Run(() => CoreBridge.Validate(candidate.Layout)); restored = null; }
            if (closing) return;
            runtimePeer = peer; runtimeLayout = candidate; layout = candidate.Layout; sample = false; revision++;
            selected = layout.Screens.FirstOrDefault()?.Id;
            runtimeScreenNames.Clear();
            foreach (var group in layout.Screens.GroupBy(x => x.DeviceId))
            {
                int index = 0;
                foreach (var screen in group) runtimeScreenNames[screen.Id] = $"{(group.Key == "local" ? "Windows" : peer.Name)} · 屏幕 {++index}";
            }
            if (activePage == origin) ShowLayout();
            Message(restored is null ? "已读取两端全部屏幕。拖动屏幕使相邻边缘贴合，再点击“应用并连接”。" : "已恢复此设备的屏幕排列，并核对当前屏幕信息。点击“应用并连接”后仍需手动开始共享。");
        }
        catch (Exception error)
        {
            string message = "无法读取两端屏幕。请在 Mac 开启接收，确认两端已更新到支持跨屏的版本，再重试。";
            if (error is BridgeValidationException) message = "当前显示器排列无法用于跨屏。请检查是否使用了镜像显示或重叠屏幕，改为扩展显示后重新读取。";
            if (error is InvalidOperationException && error.Message.Contains("清理")) message = error.Message;
            Message(message, true); if (peerStatus is not null) peerStatus.Text = message;
        }
        finally { runtimeBusy = false; UpdateSessionUi(); }
    }

    private async void ApplyRuntimeLayout()
    {
        if (!CanEditLayout || busy || !coreReady || runtimeLayout is null || runtimePeer is null) return;
        runtimeBusy = true; UpdateSessionUi();
        try
        {
            var config = RuntimeLayouts.WithLayout(runtimeLayout, layout);
            await Task.Run(() => CoreBridge.Validate(config.Layout));
            if (closing) return;
            string path = await Task.Run(() => RuntimeLayouts.SaveSnapshot(config));
            if (closing) return;
            await session.ConnectPeerAsync(runtimePeer.Id, runtimeLayoutPath: path);
            Message("已应用屏幕排列并开始连接。就绪后点击“开始跨屏共享”；鼠标越过相邻边缘时，键盘跟随目标设备。");
        }
        catch (Exception error) { Message(Friendly(error), true); }
        finally { runtimeBusy = false; UpdateSessionUi(); }
    }

    private void AddRuntimeToolbar(StackPanel toolbar)
    {
        refreshScreens = Action(runtimePeer is null ? "选择设备与屏幕" : "重新读取屏幕", "runtime-detect-screens", () =>
        {
            if (runtimePeer is null) ShowConnection(); else PrepareRuntimeLayout(runtimePeer);
        });
        applyRuntime = Action("应用并连接", "runtime-apply-connect", ApplyRuntimeLayout);
        applyRuntime.Background = theme.Brush("accent_subtle");
        toolbar.Children.Add(refreshScreens); toolbar.Children.Add(applyRuntime);
        UpdateRuntimeControls();
    }

    private void UpdateRuntimeControls()
    {
        bool editable = CanEditLayout && !busy;
        if (refreshScreens is not null) refreshScreens.IsEnabled = editable && coreReady;
        if (applyRuntime is not null) applyRuntime.IsEnabled = editable && coreReady && runtimePeer is not null && runtimeLayout is not null && !sample;
        if (propertiesScroll is not null) propertiesScroll.IsEnabled = editable;
        if (!editable && dragOriginal is not null) CancelDrag();
    }
}
