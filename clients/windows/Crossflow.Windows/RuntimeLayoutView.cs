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
    private Button? applyRuntime, refreshScreens, adjustLayout;
    private (long Generation, string Error)? shownSessionError;
    private bool CanEditLayout => !closing && !runtimeBusy && !sessionAction && !pairingBusy && !session.Snapshot.ProcessRunning;

    private string FocusDescription(SessionSnapshot state)
    {
        if (!state.RouteEnabled) return "";
        string device = state.FocusDeviceId == "local" ? "本机" :
            state.FocusDeviceId is null ? "正在定位" :
            state.FocusDeviceId == runtimePeer?.Id ? runtimePeer.Name : "远端设备";
        string? screen = state.FocusScreenId is { } id && runtimeScreenNames.TryGetValue(id, out var name) ? name : null;
        return "当前键鼠焦点：" + (screen is null ? device : $"{device}（{screen}）");
    }

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
            runtimePeer = peer; runtimeLayout = candidate; layout = candidate.Layout; layoutHistory.Clear(); revision++;
            selected = layout.Screens.FirstOrDefault()?.Id;
            runtimeScreenNames.Clear();
            foreach (var group in layout.Screens.GroupBy(x => x.DeviceId))
            {
                int index = 0;
                foreach (var screen in group) runtimeScreenNames[screen.Id] = $"{(group.Key == "local" ? "Windows" : peer.Name)} · 屏幕 {++index}";
            }
            if (activePage == origin) ShowLayout();
            Message(restored is null ? "已读取两端全部屏幕。拖动屏幕使相邻边缘贴合，再点击“开启跨屏共享”。" : "已恢复屏幕排列并核对实际屏幕，点击“开启跨屏共享”即可使用。");
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
            await session.StartRoutedAsync(runtimePeer.Id, path);
            Message("正在开启跨屏共享，首次连接就绪后立即生效。越过相邻边缘即可切换设备；断线重连后保持暂停。");
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
        applyRuntime = Action("开启跨屏共享", "runtime-apply-connect", ApplyRuntimeLayout);
        applyRuntime.Background = theme.Brush("accent_subtle");
        adjustLayout = Action("停止并调整排列", "runtime-stop-edit", StopAndEditLayout);
        toolbar.Children.Add(refreshScreens); toolbar.Children.Add(applyRuntime); toolbar.Children.Add(adjustLayout);
        UpdateRuntimeControls();
    }

    private void UpdateRuntimeControls()
    {
        bool editable = CanEditLayout && !busy;
        if (undoLayout is not null) undoLayout.IsEnabled = editable && coreReady && layoutHistory.CanUndo;
        if (redoLayout is not null) redoLayout.IsEnabled = editable && coreReady && layoutHistory.CanRedo;
        if (refreshScreens is not null) refreshScreens.IsEnabled = editable && coreReady;
        if (applyRuntime is not null) applyRuntime.IsEnabled = editable && coreReady && runtimePeer is not null && runtimeLayout is not null;
        if (adjustLayout is not null)
        {
            adjustLayout.Visibility = session.Snapshot.ProcessRunning ? Visibility.Visible : Visibility.Collapsed;
            adjustLayout.IsEnabled = !closing && !sessionAction && !runtimeBusy;
        }
        if (propertiesScroll is not null) propertiesScroll.IsEnabled = editable;
        if (!editable && dragOriginal is not null) CancelDrag();
    }

    private async void StopAndEditLayout()
    {
        if (closing || sessionAction || runtimeBusy) return;
        session.CancelPendingStart(); sessionAction = true; UpdateSessionUi();
        try
        {
            if (!await session.StopAsync()) throw new InvalidOperationException("共享尚未停止，请重试“停止并调整排列”。");
            if (!closing) ShowLayout();
        }
        catch (Exception error) { Message(Friendly(error), true); }
        finally
        {
            sessionAction = false; UpdateSessionUi();
            if (!closing && !session.Snapshot.ProcessRunning) Message("已停止共享，现在可拖动屏幕。调整后点击“开启跨屏共享”即可重新使用。");
        }
    }
}
