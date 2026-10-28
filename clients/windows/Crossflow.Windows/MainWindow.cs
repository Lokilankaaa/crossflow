using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using Windows.Storage;
using Windows.Storage.Pickers;
using Windows.System;
using Windows.Foundation;

namespace Crossflow.Windows;

internal sealed class MainWindow : Window
{
    private readonly Theme theme;
    private readonly Grid root = new();
    private readonly Grid workspace = new();
    private readonly ScrollViewer workspaceScroll = new();
    private readonly StackPanel sidebar;
    private readonly ScrollViewer sidebarScroll = new() { HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
    private readonly StackPanel deviceList = new() { Spacing = 12 };
    private readonly TextBlock sidebarState = new() { TextWrapping = TextWrapping.Wrap };
    private readonly Button sidebarPrimary = new(), sidebarStop = new();
    private readonly Dictionary<string, Button> navigationButtons = new();
    private string activePage = "layout";
    private readonly StackPanel compactNavigation = new();
    private Grid? editorGrid;
    private Border? screenBoard, propertiesPanel;
    private ScrollViewer? propertiesScroll;
    private StackPanel? layoutToolbar;
    private ScrollViewer? toolbarScroll;
    private bool compact;
    private readonly Canvas canvas = new();
    private readonly StackPanel inspector = new();
    private readonly InfoBar notice = new() { IsOpen = true, IsClosable = false };
    private readonly TextBlock status = new();
    private TextBlock? connectionStatus;
    private TextBlock? sessionDetail;
    private Button? connectSession, armSession, pauseSession, stopSession, identityPicker;
    private TextBox? hostInput, identityInput;
    private readonly SessionController session = new();
    private readonly DiscoveryClient discovery = new();
    private readonly PeerClient peers = new();
    private Task<bool> peerPageCleanup = Task.FromResult(true);
    private TextBlock? peerStatus;
    private StackPanel? savedPeers;
    private Button? refreshPeers;
    private readonly List<Button> peerConnectButtons = [];
    private long peerGeneration;
    private TextBlock? discoveryStatus;
    private StackPanel? nearbyDevices;
    private Button? refreshDiscovery;
    private long discoveryGeneration;
    private string targetHost = "", identityDirectory = "";
    private bool sessionAction, closing, allowClose;
    private readonly Button export = new();
    private LayoutDocument layout = new([]);
    private string? selected;
    private bool coreReady, busy, sample;
    private long revision;
    private double scale = 1, offsetX, offsetY;
    private ScreenLayout? dragOriginal;
    private Point dragStart;
    private ScreenLayout? dragPreview;
    private Border? dragVisual;

    internal MainWindow(Theme theme)
    {
        this.theme = theme;
        session.Changed += _ => DispatcherQueue.TryEnqueue(UpdateSessionUi);
        Title = "Crossflow";
        root.RequestedTheme = ElementTheme.Light;
        root.Background = theme.Brush("background");
        root.RowDefinitions.Add(new() { Height = GridLength.Auto });
        root.RowDefinitions.Add(new() { Height = new GridLength(1, GridUnitType.Star) });
        root.ColumnDefinitions.Add(new() { Width = new GridLength(theme.Number("window", "sidebar_width")) });
        root.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        sidebar = new StackPanel { Spacing = 6, Padding = new Thickness(18, 26, 18, 20), Background = theme.Brush("surface") };
        var brand = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 10, Margin = new Thickness(8, 0, 0, 22) };
        brand.Children.Add(new FontIcon { Glyph = "\uE71B", FontSize = 26, Foreground = theme.Brush("accent") });
        brand.Children.Add(new TextBlock { Text = "Crossflow", FontSize = 23, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, Foreground = theme.Brush("text") });
        sidebar.Children.Add(brand);
        sidebar.Children.Add(Navigation("工作台", "\uE7F4", "layout", ShowLayout));
        sidebar.Children.Add(Navigation("设备", "\uE772", "connection", ShowConnection));
        sidebar.Children.Add(Navigation("输入", "\uE765", "input", ShowInput));
        sidebar.Children.Add(Navigation("设置", "\uE713", "settings", ShowSettings));
        sidebar.Children.Add(Divider());
        sidebarState.FontSize = 13; sidebarState.Foreground = theme.Brush("text"); sidebarState.Margin = new Thickness(8, 2, 0, 8);
        sidebar.Children.Add(sidebarState);
        sidebarPrimary.Background = theme.Brush("accent"); sidebarPrimary.Foreground = new SolidColorBrush(Microsoft.UI.Colors.White);
        sidebarPrimary.BorderThickness = new Thickness(0); sidebarPrimary.CornerRadius = new CornerRadius(10); sidebarPrimary.Padding = new Thickness(12, 10, 12, 10); sidebarPrimary.HorizontalAlignment = HorizontalAlignment.Stretch;
        AutomationProperties.SetAutomationId(sidebarPrimary, "sidebar-session-primary");
        sidebarPrimary.Click += (_, _) => { var snapshot = session.Snapshot; if (!snapshot.ProcessRunning) ShowConnection(); else RunSessionAction(snapshot.State == "sharing" ? session.PauseAsync : session.ArmAsync); };
        sidebar.Children.Add(sidebarPrimary);
        sidebarStop.Content = "停止连接"; sidebarStop.Background = theme.Brush("surface"); sidebarStop.BorderThickness = new Thickness(0); sidebarStop.HorizontalAlignment = HorizontalAlignment.Stretch;
        AutomationProperties.SetAutomationId(sidebarStop, "sidebar-session-stop");
        sidebarStop.Click += (_, _) => RunSessionAction(async () => { if (!await session.StopAsync()) throw new InvalidOperationException("共享内核尚未退出，请再次停止。"); });
        sidebar.Children.Add(sidebarStop);
        sidebar.Children.Add(Divider());
        sidebar.Children.Add(Text("设备与屏幕", "caption", true)); sidebar.Children.Add(deviceList);
        sidebarScroll.Content = sidebar;
        var sidebarBorder = new Border { Child = sidebarScroll, BorderThickness = new Thickness(0, 0, 1, 0), BorderBrush = theme.Brush("border"), Background = theme.Brush("surface") };
        Grid.SetRowSpan(sidebarBorder, 2); root.Children.Add(sidebarBorder);
        compactNavigation.Orientation = Orientation.Horizontal;
        compactNavigation.Spacing = theme.Number("spacing", "large");
        compactNavigation.Padding = new Thickness(16, 12, 16, 12);
        compactNavigation.Background = theme.Brush("surface");
        compactNavigation.Visibility = Visibility.Collapsed;
        var menu = new MenuFlyout();
        foreach (var (title, id, open) in new (string, string, Action)[] {
            ("工作台", "compact-nav-layout", ShowLayout),
            ("输入", "compact-nav-input", ShowInput),
            ("设备", "compact-nav-connection", ShowConnection),
            ("设置", "compact-nav-settings", ShowSettings) })
        {
            var item = new MenuFlyoutItem { Text = title };
            AutomationProperties.SetAutomationId(item, id);
            item.Click += (_, _) => { if (!busy) open(); }; menu.Items.Add(item);
        }
        var navigation = new Button { Content = "导航", Flyout = menu };
        AutomationProperties.SetAutomationId(navigation, "compact-navigation");
        compactNavigation.Children.Add(navigation); compactNavigation.Children.Add(Text("Crossflow", "section"));
        Grid.SetColumn(compactNavigation, 1); root.Children.Add(compactNavigation);
        workspace.Margin = new Thickness(theme.Number("spacing", "section"));
        workspaceScroll.Content = workspace;
        workspaceScroll.HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled;
        workspaceScroll.HorizontalContentAlignment = HorizontalAlignment.Stretch;
        workspaceScroll.VerticalContentAlignment = VerticalAlignment.Stretch;
        Grid.SetColumn(workspaceScroll, 1); Grid.SetRow(workspaceScroll, 1); root.Children.Add(workspaceScroll);
        root.SizeChanged += (_, _) => ApplyAdaptiveLayout();
        Content = root;
        AppWindow.Resize(new global::Windows.Graphics.SizeInt32((int)theme.Number("window", "width"), (int)theme.Number("window", "height")));
        AutomationProperties.SetAutomationId(canvas, "layout-canvas");
        AutomationProperties.SetName(canvas, "屏幕排列画布");
        canvas.Background = theme.Brush("background");
        canvas.SizeChanged += (_, _) => { if (dragOriginal is null) Draw(); };
        canvas.PointerMoved += DragMove;
        canvas.PointerReleased += DragEnd;
        canvas.PointerCanceled += (_, _) => CancelDrag();
        canvas.PointerCaptureLost += (_, _) => { if (dragOriginal is not null) CancelDrag(); };
        ShowLayout();
        root.Loaded += async (_, _) =>
        {
            double density = root.XamlRoot.RasterizationScale;
            var work = Microsoft.UI.Windowing.DisplayArea.GetFromWindowId(AppWindow.Id, Microsoft.UI.Windowing.DisplayAreaFallback.Primary).WorkArea;
            AppWindow.Resize(new global::Windows.Graphics.SizeInt32(Math.Min(work.Width, (int)(theme.Number("window", "width") * density)), Math.Min(work.Height, (int)(theme.Number("window", "height") * density))));
            try { await Task.Run(() => CoreBridge.Request(new { op = "environment" })); coreReady = true; }
            catch { coreReady = false; }
            UpdateStatus();
        };
        AppWindow.Changed += (_, change) =>
        {
            if (!change.DidSizeChange || root.XamlRoot is null) return;
            double density = root.XamlRoot.RasterizationScale;
            var work = Microsoft.UI.Windowing.DisplayArea.GetFromWindowId(AppWindow.Id, Microsoft.UI.Windowing.DisplayAreaFallback.Primary).WorkArea;
            int minimumWidth = Math.Min(work.Width, (int)(theme.Number("window", "minimum_width") * density)), minimumHeight = Math.Min(work.Height, (int)(theme.Number("window", "minimum_height") * density));
            if (AppWindow.Size.Width < minimumWidth || AppWindow.Size.Height < minimumHeight) AppWindow.Resize(new global::Windows.Graphics.SizeInt32(Math.Max(minimumWidth, AppWindow.Size.Width), Math.Max(minimumHeight, AppWindow.Size.Height)));
        };
        AppWindow.Closing += async (_, args) =>
        {
            if (allowClose) return;
            args.Cancel = true;
            if (closing) return;
            closing = true; UpdateSessionUi();
            try
            {
                var readsStopped = await Task.WhenAll(discovery.StopAsync(), peers.StopAsync());
                bool scanStopped = readsStopped.All(stopped => stopped);
                bool sessionStopped = await session.StopAsync();
                if (scanStopped && sessionStopped) { allowClose = true; Close(); return; }
                ShowConnection();
                if (sessionDetail is not null) sessionDetail.Text = "后台进程尚未退出。请再次停止连接，确认后再关闭窗口。";
            }
            catch { ShowConnection(); if (sessionDetail is not null) sessionDetail.Text = "未能确认共享停止，请重试停止连接。"; }
            closing = false; UpdateSessionUi();
        };
    }

    private TextBlock Text(string value, string size = "body", bool secondary = false) => new() { Text = value, FontSize = theme.Number("typography", size), Foreground = theme.Brush(secondary ? "text_secondary" : "text"), TextWrapping = TextWrapping.Wrap };
    private static string DeviceName(string id) => id switch { "windows-workstation" => "Windows 台式机", "macbook-air" => "MacBook Air", _ => id };
    private static string ScreenName(string id) => id switch { "windows-main" => "主显示器", "windows-portrait" => "竖屏", "mac-external" => "外接显示器", "mac-built-in" => "内建显示器", _ => id };
    private Border Divider() => new() { Height = 1, Background = theme.Brush("border"), Margin = new Thickness(0, 16, 0, 16) };
    private Button Navigation(string title, string glyph, string page, Action open)
    {
        var content = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 14 };
        content.Children.Add(new FontIcon { Glyph = glyph, FontSize = 18 }); content.Children.Add(Text(title));
        var button = Action("", "nav-" + page, open); button.Content = content;
        button.HorizontalContentAlignment = HorizontalAlignment.Left; button.BorderThickness = new Thickness(0); button.CornerRadius = new CornerRadius(10); button.Padding = new Thickness(12, 11, 12, 11);
        navigationButtons[page] = button; return button;
    }
    private void SelectPage(string name)
    {
        activePage = name;
        foreach (var entry in navigationButtons) entry.Value.Background = theme.Brush(entry.Key == name ? "accent_subtle" : "surface");
    }
    private void RefreshDevices()
    {
        deviceList.Children.Clear();
        if (layout.Screens.Count == 0) { deviceList.Children.Add(Text("添加屏幕后，会按所属设备分组。", "caption", true)); return; }
        deviceList.Children.Add(Text(sample ? "示例设备 · 尚未发现" : "布局中的设备 · 非连接状态", "caption", true));
        int groupIndex = 0;
        foreach (var group in layout.Screens.GroupBy(screen => screen.DeviceId))
        {
            var color = theme.Brush(groupIndex++ % 2 == 0 ? "accent" : "device_secondary");
            var groupPanel = new StackPanel { Spacing = 4 };
            var heading = Text($"{DeviceName(group.Key)} · {group.Count()} 块屏幕", "caption"); heading.MaxWidth = Math.Max(80, theme.Number("window", "sidebar_width") - 42); heading.Foreground = color; heading.Margin = new Thickness(6, 8, 0, 4); groupPanel.Children.Add(heading);
            foreach (var screen in group)
            {
                var select = Action(ScreenName(screen.Id), "device-screen-" + screen.Id, () => { selected = screen.Id; ShowLayout(); });
                select.HorizontalContentAlignment = HorizontalAlignment.Left; select.BorderThickness = new Thickness(0); select.Padding = new Thickness(12, 8, 8, 8); select.CornerRadius = new CornerRadius(8);
                select.Background = theme.Brush(selected == screen.Id ? "accent_subtle" : "surface");
                var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
                line.Children.Add(new FontIcon { Glyph = screen.Height > screen.Width ? "\uE8EA" : "\uE7F4", FontSize = 16, Foreground = color });
                var label = Text(ScreenName(screen.Id), "caption"); label.MaxWidth = Math.Max(60, theme.Number("window", "sidebar_width") - 82); label.TextWrapping = TextWrapping.NoWrap; label.TextTrimming = TextTrimming.CharacterEllipsis;
                line.Children.Add(label); select.Content = line; groupPanel.Children.Add(select);
            }
            deviceList.Children.Add(groupPanel);
        }
    }
    private Button Action(string title, string id, Action run)
    {
        var button = new Button { Content = title, HorizontalAlignment = HorizontalAlignment.Stretch, FontSize = theme.Number("typography", "body") };
        AutomationProperties.SetAutomationId(button, id);
        button.Click += (_, _) => { if (!busy) run(); };
        return button;
    }
    private StackPanel Page(string title, string subtitle)
    {
        ResetWorkspace();
        connectionStatus = null;
        sessionDetail = null; connectSession = null; armSession = null; pauseSession = null; stopSession = null; identityPicker = null; hostInput = null; identityInput = null;
        var page = new StackPanel { Spacing = theme.Number("spacing", "large") };
        page.Children.Add(Text(title, "title")); page.Children.Add(Text(subtitle, secondary: true));
        workspace.Children.Add(page); ApplyAdaptiveLayout(); return page;
    }
    private void ResetWorkspace()
    {
        discoveryGeneration++;
        discovery.Cancel();
        discoveryStatus = null; nearbyDevices = null; refreshDiscovery = null;
        peerGeneration++;
        peers.Cancel();
        peerPageCleanup = peers.StopAsync();
        peerStatus = null; savedPeers = null; refreshPeers = null; peerConnectButtons.Clear();
        if (dragOriginal is not null) CancelDrag();
        // A disconnected WinUI element's Parent may be null while its old
        // container still owns it. Release via the containers we created,
        // before clearing those references or detaching the page tree.
        layoutToolbar?.Children.Clear();
        if (screenBoard is not null) screenBoard.Child = null;
        if (propertiesScroll is not null) propertiesScroll.Content = null;
        if (workspace.Children.IndexOf(notice) >= 0) workspace.Children.Remove(notice);
        if (workspace.Children.IndexOf(status) >= 0) workspace.Children.Remove(status);
        workspace.Children.Clear(); workspace.RowDefinitions.Clear();
        editorGrid = null; screenBoard = null; propertiesPanel = null;
        propertiesScroll = null; layoutToolbar = null; toolbarScroll = null;
    }
    private void ShowConnection()
    {
        SelectPage("connection");
        var page = Page("设备", "选择已配对设备连接。新发现的设备需要完成配对后才能共享。");
        connectionStatus = Text("尚未连接", "section"); page.Children.Add(connectionStatus);
        var controls = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        armSession = Action("开始共享", "session-arm", () => RunSessionAction(session.ArmAsync));
        pauseSession = Action("暂停", "session-pause", () => RunSessionAction(session.PauseAsync));
        stopSession = Action("停止连接", "session-stop", () => RunSessionAction(async () => { if (!await session.StopAsync()) throw new InvalidOperationException("共享内核尚未退出，请重试停止连接。"); }));
        foreach (var button in new[] { armSession, pauseSession, stopSession }) controls.Children.Add(button);
        page.Children.Add(controls);
        sessionDetail = Text("连接后保持暂停，需要手动开始共享。", secondary: true); page.Children.Add(sessionDetail);
        var savedHeading = new Grid();
        savedHeading.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        savedHeading.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        savedHeading.Children.Add(Text("已配对设备", "section"));
        refreshPeers = Action("刷新列表", "peers-refresh", StartPeers);
        Grid.SetColumn(refreshPeers, 1); savedHeading.Children.Add(refreshPeers); page.Children.Add(savedHeading);
        peerStatus = Text("正在读取已配对设备…", "caption", true); page.Children.Add(peerStatus);
        savedPeers = new StackPanel(); AutomationProperties.SetAutomationId(savedPeers, "saved-peers"); page.Children.Add(savedPeers);
        page.Children.Add(Divider());
        page.Children.Add(Text("附近设备", "section"));
        var heading = new Grid();
        heading.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        heading.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        discoveryStatus = Text("准备查找附近设备…", secondary: true);
        discoveryStatus.VerticalAlignment = VerticalAlignment.Center; heading.Children.Add(discoveryStatus);
        refreshDiscovery = Action("刷新", "discovery-refresh", StartDiscovery);
        refreshDiscovery.Margin = new Thickness(16, 0, 0, 0); Grid.SetColumn(refreshDiscovery, 1); heading.Children.Add(refreshDiscovery);
        page.Children.Add(heading);
        nearbyDevices = new StackPanel { Spacing = 0 };
        AutomationProperties.SetAutomationId(nearbyDevices, "nearby-devices");
        page.Children.Add(nearbyDevices);
        page.Children.Add(Text("首次连接需要在两台设备上确认配对。附近的新设备配对尚未接入；已配对设备可从上方列表连接。", "caption", true));
        var diagnostics = new StackPanel { Spacing = 14, Padding = new Thickness(4, 12, 4, 12) };
        diagnostics.Children.Add(Text("开发诊断 · 仅用于已有身份文件的实验连接。本轮最多运行一小时。", secondary: true));
        hostInput = new TextBox { Header = "目标 IP 和端口", PlaceholderText = "192.168.1.20:24800", Text = targetHost, MaxLength = 128 };
        AutomationProperties.SetAutomationId(hostInput, "session-host");
        hostInput.TextChanged += (sender, _) => targetHost = ((TextBox)sender).Text;
        diagnostics.Children.Add(hostInput);
        identityInput = new TextBox { Header = "配对身份文件夹", Text = identityDirectory, PlaceholderText = "选择现有配对文件夹" };
        AutomationProperties.SetAutomationId(identityInput, "session-identity");
        identityInput.TextChanged += (sender, _) => identityDirectory = ((TextBox)sender).Text;
        diagnostics.Children.Add(identityInput);
        identityPicker = Action("选择身份文件夹", "session-pick-identity", async () =>
        {
            var picker = new FolderPicker(); picker.FileTypeFilter.Add("*");
            WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
            try { var folder = await picker.PickSingleFolderAsync(); if (folder is not null) { identityDirectory = folder.Path; if (identityInput is not null) identityInput.Text = identityDirectory; } }
            catch { if (sessionDetail is not null) sessionDetail.Text = "无法选择文件夹，请粘贴现有配对文件夹路径。"; }
        });
        diagnostics.Children.Add(identityPicker);
        connectSession = Action("连接", "session-connect", () => RunSessionAction(() => session.ConnectAsync(targetHost, identityDirectory)));
        diagnostics.Children.Add(connectSession);
        diagnostics.Children.Add(Text("当前实验连接仅共享鼠标移动。键盘、鼠标点击、滚轮与屏幕边缘自动切换尚未接入此入口。布局编辑仍是草稿。", "caption", true));
        var advanced = new Expander { Header = "开发诊断", Content = diagnostics, IsExpanded = false, HorizontalAlignment = HorizontalAlignment.Stretch, HorizontalContentAlignment = HorizontalAlignment.Stretch };
        AutomationProperties.SetAutomationId(advanced, "developer-diagnostics");
        page.Children.Add(advanced);
        UpdateSessionUi();
        if (!closing) { StartPeers(); StartDiscovery(); }
    }
    private async void StartPeers()
    {
        if (closing || activePage != "connection" || peerStatus is null) return;
        long generation = ++peerGeneration;
        if (refreshPeers is not null) refreshPeers.IsEnabled = false;
        savedPeers?.Children.Clear(); peerConnectButtons.Clear();
        peerStatus.Text = "正在读取已配对设备…";
        try
        {
            await peerPageCleanup;
            if (generation != peerGeneration || closing) return;
            if (!await peers.StopAsync()) throw new InvalidOperationException("设备列表读取尚未退出，请稍后重试。");
            if (generation != peerGeneration || closing) return;
            var result = await peers.ReadAsync();
            if (generation != peerGeneration || closing || savedPeers is null || peerStatus is null) return;
            peerStatus.Text = result.Peers.Count == 0 ? "尚无已配对设备" : $"已保存 {result.Peers.Count} 台设备 · 在线状态由连接后确认";
            if (result.InvalidRecords != 0)
                peerStatus.Text = $"有 {result.InvalidRecords} 条已保存记录无法验证，已隐藏。{(result.Peers.Count == 0 ? "目前没有可连接的有效记录。" : $"以下 {result.Peers.Count} 条记录可尝试连接。")}";
            foreach (var peer in result.Peers)
            {
                var row = new Grid { Padding = new Thickness(0, 16, 0, 16), ColumnSpacing = 16 };
                row.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
                row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
                var description = new StackPanel { Spacing = 5 };
                var name = Text(peer.Name, "section"); name.TextWrapping = TextWrapping.NoWrap; name.TextTrimming = TextTrimming.CharacterEllipsis;
                description.Children.Add(name); description.Children.Add(Text("已保存配对信息", "caption", true));
                row.Children.Add(description);
                var connect = Action("连接", "peer-connect-" + peer.Id, () => RunSessionAction(() => session.ConnectPeerAsync(peer.Id)));
                connect.VerticalAlignment = VerticalAlignment.Center;
                peerConnectButtons.Add(connect); Grid.SetColumn(connect, 1); row.Children.Add(connect);
                savedPeers.Children.Add(new Border { Child = row, BorderBrush = theme.Brush("border"), BorderThickness = new Thickness(0, 0, 0, 1) });
            }
            UpdateSessionUi();
        }
        catch (OperationCanceledException) { }
        catch (Exception error)
        {
            if (generation == peerGeneration && !closing && peerStatus is not null)
                peerStatus.Text = error is InvalidOperationException ? error.Message : "无法读取已配对设备，请刷新重试。";
        }
        finally
        {
            if (generation == peerGeneration && !closing && refreshPeers is not null) refreshPeers.IsEnabled = true;
        }
    }
    private async void StartDiscovery()
    {
        if (closing || activePage != "connection" || discoveryStatus is null) return;
        long generation = ++discoveryGeneration;
        if (refreshDiscovery is not null) refreshDiscovery.IsEnabled = false;
        nearbyDevices?.Children.Clear();
        discoveryStatus.Text = "正在查找附近设备…";
        try
        {
            // Re-entering the page cancels and reaps the previous scan first.
            if (!await discovery.StopAsync()) throw new InvalidOperationException("上一次扫描尚未退出，请稍后重试。");
            if (generation != discoveryGeneration || closing) return;
            var result = await discovery.ScanAsync();
            if (generation != discoveryGeneration || closing || nearbyDevices is null || discoveryStatus is null) return;
            discoveryStatus.Text = result.Devices.Count == 0 ? "暂未发现设备" : $"发现 {result.Devices.Count} 台附近设备";
            if (result.CapacityLimited) discoveryStatus.Text += " · 结果较多，部分设备未显示";
            if (result.Devices.Count == 0)
                nearbyDevices.Children.Add(Text("请在另一台设备上打开支持发现的 Crossflow，并确认两台设备处于同一局域网。", secondary: true));
            foreach (var device in result.Devices)
            {
                var row = new Grid { Padding = new Thickness(0, 20, 0, 20), ColumnSpacing = 16 };
                row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
                row.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
                row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
                row.Children.Add(new FontIcon { Glyph = "\uE7F4", FontSize = 24, Foreground = theme.Brush("text_secondary"), VerticalAlignment = VerticalAlignment.Center });
                var description = new StackPanel { Spacing = 5 };
                var name = Text(device.Name, "section"); name.TextWrapping = TextWrapping.NoWrap; name.TextTrimming = TextTrimming.CharacterEllipsis;
                description.Children.Add(name);
                string platform = device.Platform switch { "windows" => "Windows", "macos" => "macOS", "linux" => "Linux", "ios" => "iOS", "ipados" => "iPadOS", _ => device.Platform };
                description.Children.Add(Text($"{platform} · 未配对{(device.Compatible ? "" : " · 版本不兼容")}", "caption", true));
                Grid.SetColumn(description, 1); row.Children.Add(description);
                var pair = new Button { Content = device.Compatible ? "配对待接入" : "需要更新", IsEnabled = false, VerticalAlignment = VerticalAlignment.Center };
                AutomationProperties.SetAutomationId(pair, "pair-unavailable-" + device.Id);
                Grid.SetColumn(pair, 2); row.Children.Add(pair);
                nearbyDevices.Children.Add(new Border { Child = row, BorderThickness = new Thickness(0, 0, 0, 1), BorderBrush = theme.Brush("border") });
            }
        }
        catch (OperationCanceledException) { /* Navigation/close invalidates this generation. */ }
        catch (Exception error)
        {
            if (generation == discoveryGeneration && !closing && discoveryStatus is not null)
                discoveryStatus.Text = error is InvalidOperationException ? error.Message : "扫描失败，请检查网络后刷新。";
        }
        finally
        {
            if (generation == discoveryGeneration && !closing && refreshDiscovery is not null) refreshDiscovery.IsEnabled = true;
        }
    }
    private async void RunSessionAction(Func<Task> operation)
    {
        if (sessionAction || closing) return;
        sessionAction = true; UpdateSessionUi();
        string? error = null;
        try { await operation(); }
        catch (Exception failure) { error = failure is InvalidOperationException ? failure.Message : "无法完成连接操作，请检查设备与已保存的配对记录。"; }
        finally { sessionAction = false; UpdateSessionUi(); if (error is not null && sessionDetail is not null) sessionDetail.Text = error; }
    }
    private void UpdateSessionUi()
    {
        var state = session.Snapshot;
        string title = state.State switch { "starting" => "正在启动", "connecting" => "正在连接或重连", "paused" => "已连接 · 已暂停", "sharing" => "鼠标共享中", "stopping" => "正在停止", "stopped" => "已停止", "error" => "连接出现错误", _ => "尚未连接" };
        if (connectionStatus is not null) connectionStatus.Text = title;
        sidebarState.Text = title;
        sidebarPrimary.Content = !state.ProcessRunning ? "连接设备" : state.State == "sharing" ? "暂停共享" : state.CanArm ? "开始共享" : "等待就绪";
        sidebarPrimary.IsEnabled = !sessionAction && !closing && (!state.ProcessRunning || state.CanArm || (state.State == "sharing" && !state.CommandPending && !state.StopRequested));
        sidebarStop.IsEnabled = state.ProcessRunning && !sessionAction && !closing;
        sidebarStop.Visibility = state.ProcessRunning ? Visibility.Visible : Visibility.Collapsed;
        if (connectSession is not null) connectSession.IsEnabled = !state.ProcessRunning && !sessionAction && !closing;
        foreach (var button in peerConnectButtons) button.IsEnabled = !state.ProcessRunning && !sessionAction && !closing;
        if (armSession is not null) armSession.IsEnabled = state.CanArm && !sessionAction && !closing;
        if (pauseSession is not null) pauseSession.IsEnabled = state.State == "sharing" && !state.CommandPending && !state.StopRequested && !sessionAction && !closing;
        if (stopSession is not null) stopSession.IsEnabled = state.ProcessRunning && !sessionAction && !closing;
        if (hostInput is not null) hostInput.IsEnabled = !state.ProcessRunning && !sessionAction && !closing;
        if (identityInput is not null) identityInput.IsEnabled = !state.ProcessRunning && !sessionAction && !closing;
        if (identityPicker is not null) identityPicker.IsEnabled = !state.ProcessRunning && !sessionAction && !closing;
        if (sessionDetail is not null)
            sessionDetail.Text = state.StopRequested ? "已请求停止，正在等待内核退出确认。" : state.Error is not null ? (state.State == "error" ? state.Error : "内核拒绝了本次操作，状态已更新。请确认就绪后再试。") :
                state.State == "paused" ? (state.CaptureReady ? "已就绪。点击“开始共享”才会转发鼠标移动；重连后也需要手动开始。" : "已连接，正在准备鼠标输入。") :
                state.State == "sharing" ? "正在转发鼠标移动。点击“暂停”可立即请求暂停；关闭窗口会停止连接。" :
                state.ProcessRunning ? "正在等待共享内核报告状态。" : "打开客户端不会自动连接或启动输入。";
        status.Text = $"{(coreReady ? theme.Copy("core_ready") : theme.Copy("core_unavailable"))}  ·  {title}  ·  布局尚未应用运行时";
    }
    private void ShowInput()
    {
        SelectPage("input");
        var page = Page(theme.Copy("input_title"), "当前连接入口仅共享鼠标移动，其余输入设置尚未接入。");
        page.Children.Add(new ToggleSwitch { Header = "Ctrl / Cmd 映射", IsEnabled = false });
        page.Children.Add(new ToggleSwitch { Header = "边缘阻尼", IsEnabled = false });
        page.Children.Add(Text("打开此窗口不会启动键鼠共享，也不会改变系统输入。", secondary: true));
    }
    private void ShowSettings()
    {
        SelectPage("settings");
        var page = Page("设置", "本地运行，无需账号。共享状态由统一 Rust 内核提供。");
        page.Children.Add(Text("每轮会话最长一小时。连接恢复后保持暂停，需要你手动开始共享。", secondary: true));
        page.Children.Add(Text("会话日志保存在当前用户的本地应用数据目录 / Crossflow / Logs。身份文件仅交给内核读取。", secondary: true));
        page.Children.Add(Text("屏幕布局可独立编辑与导出，目前尚未应用到实际鼠标共享。", secondary: true));
    }
    private void ShowLayout()
    {
        SelectPage("layout");
        ResetWorkspace();
        connectionStatus = null;
        sessionDetail = null; connectSession = null; armSession = null; pauseSession = null; stopSession = null; identityPicker = null; hostInput = null; identityInput = null;
        foreach (var length in new[] { GridLength.Auto, GridLength.Auto, GridLength.Auto, new GridLength(1, GridUnitType.Star), GridLength.Auto }) workspace.RowDefinitions.Add(new() { Height = length });
        var heading = new StackPanel { Spacing = 8, Margin = new Thickness(0, 0, 0, 20) };
        heading.Children.Add(Text(theme.Copy("layout_title"), "title")); heading.Children.Add(Text(theme.Copy("layout_subtitle"), secondary: true));
        workspace.Children.Add(heading);
        var toolbar = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 0, 0, 16) };
        layoutToolbar = toolbar;
        toolbar.Children.Add(Action("添加屏幕", "add-screen", AddScreen));
        toolbar.Children.Add(Action("导入布局", "import-layout", Import));
        export.Content = "导出布局"; export.FontSize = theme.Number("typography", "body");
        AutomationProperties.SetAutomationId(export, "export-layout");
        export.Click -= ExportClick; export.Click += ExportClick;
        toolbar.Children.Add(export);
        toolbar.Children.Add(Action("显示示例", "load-sample", LoadSample));
        toolbar.Children.Add(Action("适应画布", "fit-layout", Draw));
        toolbarScroll = new ScrollViewer { Content = toolbar, HorizontalScrollMode = ScrollMode.Enabled, VerticalScrollMode = ScrollMode.Disabled, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled };
        Grid.SetRow(toolbarScroll, 1); workspace.Children.Add(toolbarScroll);
        notice.Margin = new Thickness(0, 0, 0, 16); Grid.SetRow(notice, 2); workspace.Children.Add(notice);
        var editor = new Grid { ColumnSpacing = 16 };
        editorGrid = editor;
        editor.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        editor.ColumnDefinitions.Add(new() { Width = new GridLength(theme.Number("window", "inspector_width")) });
        var board = new Border { Background = theme.Brush("background"), Child = canvas };
        screenBoard = board;
        editor.Children.Add(board);
        inspector.Spacing = 12; inspector.Padding = new Thickness(16);
        var scroll = new ScrollViewer { Content = inspector, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        propertiesScroll = scroll;
        var properties = new Border { Child = scroll, Background = theme.Brush("surface"), BorderThickness = new Thickness(1, 0, 0, 0), BorderBrush = theme.Brush("border") };
        propertiesPanel = properties;
        Grid.SetColumn(properties, 1); editor.Children.Add(properties);
        Grid.SetRow(editor, 3); workspace.Children.Add(editor);
        status.FontSize = theme.Number("typography", "caption"); status.Foreground = theme.Brush("text_secondary"); status.Margin = new Thickness(0, 16, 0, 0); status.TextWrapping = TextWrapping.Wrap;
        Grid.SetRow(status, 4); workspace.Children.Add(status);
        ApplyAdaptiveLayout(); UpdateStatus(); Draw(); Inspect(); RefreshDevices();
    }
    private void ApplyAdaptiveLayout()
    {
        // XAML sizes are logical units. A small high-DPI work area must not
        // lose the canvas to fixed desktop columns or an oversized minimum.
        bool nextCompact = root.ActualWidth > 0 && (root.ActualWidth < 960 || root.ActualHeight < 560);
        if (nextCompact != compact && dragOriginal is not null) CancelDrag();
        compact = nextCompact;
        root.ColumnDefinitions[0].Width = new GridLength(compact ? 0 : theme.Number("window", "sidebar_width"));
        sidebar.Visibility = compact ? Visibility.Collapsed : Visibility.Visible;
        sidebarScroll.Visibility = compact ? Visibility.Collapsed : Visibility.Visible;
        compactNavigation.Visibility = compact ? Visibility.Visible : Visibility.Collapsed;
        workspace.Margin = new Thickness(theme.Number("spacing", compact ? "large" : "section"));
        workspaceScroll.VerticalScrollBarVisibility = compact || activePage != "layout" ? ScrollBarVisibility.Auto : ScrollBarVisibility.Disabled;
        workspaceScroll.VerticalScrollMode = compact || activePage != "layout" ? ScrollMode.Enabled : ScrollMode.Disabled;
        if (layoutToolbar is not null) layoutToolbar.Orientation = Orientation.Horizontal;
        if (toolbarScroll is not null) toolbarScroll.HorizontalScrollBarVisibility = compact ? ScrollBarVisibility.Auto : ScrollBarVisibility.Disabled;
        if (editorGrid is null || screenBoard is null || propertiesPanel is null || propertiesScroll is null) return;
        editorGrid.ColumnDefinitions.Clear(); editorGrid.RowDefinitions.Clear();
        editorGrid.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        editorGrid.ColumnSpacing = compact ? 0 : theme.Number("spacing", "large");
        editorGrid.RowSpacing = compact ? theme.Number("spacing", "large") : 0;
        if (compact)
        {
            editorGrid.RowDefinitions.Add(new() { Height = new GridLength(280) });
            editorGrid.RowDefinitions.Add(new() { Height = GridLength.Auto });
        }
        else editorGrid.ColumnDefinitions.Add(new() { Width = new GridLength(theme.Number("window", "inspector_width")) });
        Grid.SetColumn(propertiesPanel, compact ? 0 : 1); Grid.SetRow(propertiesPanel, compact ? 1 : 0);
        propertiesScroll.VerticalScrollBarVisibility = compact ? ScrollBarVisibility.Disabled : ScrollBarVisibility.Auto;
        propertiesScroll.VerticalScrollMode = compact ? ScrollMode.Disabled : ScrollMode.Enabled;
        workspace.RowDefinitions[3].Height = compact ? GridLength.Auto : new GridLength(1, GridUnitType.Star);
    }
    private void UpdateStatus()
    {
        UpdateSessionUi();
        export.IsEnabled = coreReady && !busy && layout.Screens.Count > 0;
        Message(theme.Copy("apply_unavailable") + (sample ? " " + theme.Copy("sample_notice") : ""));
    }
    private void Message(string text, bool error = false)
    {
        notice.Title = theme.Copy("draft_label"); notice.Message = text;
        notice.Severity = error ? InfoBarSeverity.Error : InfoBarSeverity.Informational; notice.IsOpen = true;
    }
    private async Task<bool> Commit(LayoutDocument candidate, bool isSample = false)
    {
        if (busy || !coreReady) { Message("布局内核未就绪，请确认客户端已完整构建。", true); return false; }
        busy = true; export.IsEnabled = false; long attempt = ++revision;
        try
        {
            await Task.Run(() => CoreBridge.Validate(candidate));
            if (attempt != revision) return false;
            layout = candidate; sample = isSample;
            if (!layout.Screens.Any(x => x.Id == selected)) selected = layout.Screens.FirstOrDefault()?.Id;
            return true;
        }
        catch (Exception error) { Message(Friendly(error), true); return false; }
        finally { busy = false; export.IsEnabled = coreReady && layout.Screens.Count > 0; Draw(); Inspect(); RefreshDevices(); }
    }
    private static string Friendly(Exception error) => error is InvalidOperationException ? error.Message : "无法完成操作，请检查布局文件或输入内容后重试。";
    private void Draw()
    {
        canvas.Children.Clear(); double w = canvas.ActualWidth, h = canvas.ActualHeight;
        canvas.Clip = new RectangleGeometry { Rect = new Rect(0, 0, Math.Max(0, w), Math.Max(0, h)) };
        for (int x = 24; x < w; x += 32) for (int y = 24; y < h; y += 32)
        { var dot = new Ellipse { Width = 2, Height = 2, Fill = theme.Brush("border"), IsHitTestVisible = false }; Canvas.SetLeft(dot, x); Canvas.SetTop(dot, y); canvas.Children.Add(dot); }
        if (layout.Screens.Count == 0)
        { var empty = Text("从你的第一块屏幕开始\n\n添加屏幕，或打开示例了解排列方式。", secondary: true); empty.MaxWidth = 300; Canvas.SetLeft(empty, 32); Canvas.SetTop(empty, 48); canvas.Children.Add(empty); return; }
        double minX = layout.Screens.Min(s => s.X), minY = layout.Screens.Min(s => s.Y);
        // Normalize before subtracting extents, so valid large finite coordinates
        // cannot overflow the native canvas transform. Rust remains the validator.
        double bound = layout.Screens.Max(s => new[] { Math.Abs(s.X), Math.Abs(s.Y), Math.Abs(s.X + s.Width), Math.Abs(s.Y + s.Height), s.Width, s.Height }.Max());
        double spanX = layout.Screens.Max(s => (s.X + s.Width) / bound) - minX / bound, spanY = layout.Screens.Max(s => (s.Y + s.Height) / bound) - minY / bound;
        double normalizedScale = Math.Min(Math.Max(1, w - 64) / spanX, Math.Max(1, h - 96) / spanY);
        scale = normalizedScale / bound;
        offsetX = (w - spanX * normalizedScale) / 2 - minX / bound * normalizedScale; offsetY = (h - spanY * normalizedScale) / 2 - minY / bound * normalizedScale;
        if (!double.IsFinite(scale) || scale <= 0 || !double.IsFinite(offsetX) || !double.IsFinite(offsetY))
        { var unavailable = Text("布局坐标超出画布预览范围，仍可编辑属性与导出。", secondary: true); unavailable.MaxWidth = 300; Canvas.SetLeft(unavailable, 24); Canvas.SetTop(unavailable, 32); canvas.Children.Add(unavailable); return; }
        var devices = layout.Screens.Select(s => s.DeviceId).Distinct().ToList();
        foreach (var screen in layout.Screens)
        {
            bool smallTile = screen.Height * scale <= 100;
            double padding = smallTile ? 8 : 12;
            double textWidth = Math.Max(1, screen.Width * scale - padding * 2 - 4);
            var label = new StackPanel { Spacing = smallTile ? 4 : 6, VerticalAlignment = VerticalAlignment.Center };
            var screenLabel = Text(ScreenName(screen.Id), "section"); screenLabel.FontSize = smallTile ? 14 : theme.Number("typography", "section"); screenLabel.MaxWidth = textWidth; screenLabel.TextTrimming = TextTrimming.CharacterEllipsis; screenLabel.TextWrapping = TextWrapping.NoWrap;
            var deviceLabel = Text(DeviceName(screen.DeviceId), "caption", true); deviceLabel.FontSize = smallTile ? 10 : theme.Number("typography", "caption"); deviceLabel.MaxWidth = textWidth; deviceLabel.TextTrimming = TextTrimming.CharacterEllipsis; deviceLabel.TextWrapping = TextWrapping.NoWrap;
            label.Children.Add(screenLabel); label.Children.Add(deviceLabel);
            var resolution = Text($"{screen.Width:g} × {screen.Height:g}", "caption", true); resolution.FontSize = smallTile ? 10 : theme.Number("typography", "caption"); resolution.MaxWidth = textWidth; resolution.TextWrapping = TextWrapping.NoWrap;
            label.Children.Add(resolution);
            var color = theme.Brush(devices.IndexOf(screen.DeviceId) % 2 == 0 ? "accent" : "device_secondary").Color;
            var tint = new SolidColorBrush(global::Windows.UI.Color.FromArgb(22, color.R, color.G, color.B));
            var button = new Button { Content = label, Padding = new Thickness(padding), HorizontalAlignment = HorizontalAlignment.Stretch, VerticalAlignment = VerticalAlignment.Stretch, HorizontalContentAlignment = HorizontalAlignment.Center, VerticalContentAlignment = VerticalAlignment.Center, Background = tint, BorderThickness = new Thickness(0), CornerRadius = new CornerRadius(9) };
            AutomationProperties.SetName(button, $"屏幕 {screen.Id}，设备 {screen.DeviceId}。使用方向键移动。");
            AutomationProperties.SetAutomationId(button, "screen-" + screen.Id);
            var tile = new Border { Width = screen.Width * scale, Height = screen.Height * scale, Child = button, CornerRadius = new CornerRadius(10), BorderThickness = new Thickness(selected == screen.Id ? 2 : 1), BorderBrush = new SolidColorBrush(color) };
            Canvas.SetLeft(tile, screen.X * scale + offsetX); Canvas.SetTop(tile, screen.Y * scale + offsetY); canvas.Children.Add(tile);
            button.AddHandler(UIElement.PointerPressedEvent, new PointerEventHandler((_, e) =>
            {
                if (busy || !coreReady || !e.GetCurrentPoint(canvas).Properties.IsLeftButtonPressed) return;
                selected = screen.Id; Inspect(); RefreshDevices(); button.Focus(FocusState.Pointer);
                dragOriginal = screen; dragPreview = screen; dragVisual = tile; dragStart = e.GetCurrentPoint(canvas).Position;
                canvas.CapturePointer(e.Pointer); e.Handled = true;
            }), true);
            button.KeyDown += async (_, e) =>
            {
                if (busy || !coreReady || e.Key is not (VirtualKey.Left or VirtualKey.Right or VirtualKey.Up or VirtualKey.Down)) return;
                selected = screen.Id; e.Handled = true;
                double step = (Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift) & global::Windows.UI.Core.CoreVirtualKeyStates.Down) != 0 ? 50 : 10;
                var next = screen with { X = screen.X + (e.Key == VirtualKey.Left ? -step : e.Key == VirtualKey.Right ? step : 0), Y = screen.Y + (e.Key == VirtualKey.Up ? -step : e.Key == VirtualKey.Down ? step : 0) };
                if (await Commit(Replace(next), sample)) UpdateStatus();
                foreach (var child in canvas.Children.OfType<Border>()) if (child.Child is Button b && AutomationProperties.GetAutomationId(b) == "screen-" + selected) b.Focus(FocusState.Keyboard);
            };
        }
    }
    private LayoutDocument Replace(ScreenLayout screen) => new(layout.Screens.Select(s => s.Id == selected ? screen : s).ToList());
    private void DragMove(object sender, PointerRoutedEventArgs e)
    {
        if (dragOriginal is not { } original || dragVisual is null) return;
        var point = e.GetCurrentPoint(canvas).Position;
        double x = original.X + (point.X - dragStart.X) / scale, y = original.Y + (point.Y - dragStart.Y) / scale;
        if (!double.IsFinite(x) || !double.IsFinite(y)) return;
        double snap = 8 / scale;
        foreach (var other in layout.Screens.Where(s => s.Id != original.Id))
        {
            foreach (double edge in new[] { other.X - original.Width, other.X, other.X + other.Width, other.X + other.Width - original.Width }) if (Math.Abs(x - edge) < snap) { x = edge; break; }
            foreach (double edge in new[] { other.Y - original.Height, other.Y, other.Y + other.Height, other.Y + other.Height - original.Height }) if (Math.Abs(y - edge) < snap) { y = edge; break; }
        }
        dragPreview = original with { X = x, Y = y }; Canvas.SetLeft(dragVisual, x * scale + offsetX); Canvas.SetTop(dragVisual, y * scale + offsetY);
    }
    private async void DragEnd(object sender, PointerRoutedEventArgs e)
    {
        var next = dragPreview; dragOriginal = null; dragPreview = null; dragVisual = null; canvas.ReleasePointerCaptures();
        if (next is not null && await Commit(Replace(next), sample)) UpdateStatus();
    }
    private void CancelDrag() { dragOriginal = null; dragPreview = null; dragVisual = null; canvas.ReleasePointerCaptures(); Draw(); }
    private void Inspect()
    {
        var screen = layout.Screens.FirstOrDefault(s => s.Id == selected);
        inspector.Children.Clear(); inspector.Children.Add(Text(screen is null ? "屏幕属性" : ScreenName(screen.Id), "section"));
        if (screen is null) { inspector.Children.Add(Text("选择一块屏幕，编辑位置与尺寸。", secondary: true)); return; }
        var id = new TextBox { Header = "屏幕标识", Text = screen.Id };
        var device = new TextBox { Header = "所属设备标识", Text = screen.DeviceId };
        AutomationProperties.SetAutomationId(id, "property-id"); AutomationProperties.SetAutomationId(device, "property-device");
        inspector.Children.Add(id); inspector.Children.Add(device);
        NumberBox Number(string title, string aid, double value)
        { var box = new NumberBox { Header = title, Value = value, SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Compact }; AutomationProperties.SetAutomationId(box, aid); inspector.Children.Add(box); return box; }
        var x = Number("水平位置", "property-x", screen.X); var y = Number("垂直位置", "property-y", screen.Y);
        var width = Number("宽度", "property-width", screen.Width); var height = Number("高度", "property-height", screen.Height);
        inspector.Children.Add(Action("保存属性", "save-properties", async () =>
        {
            var candidate = screen with { Id = id.Text, DeviceId = device.Text, X = x.Value, Y = y.Value, Width = width.Value, Height = height.Value };
            if (await Commit(Replace(candidate), sample)) { selected = candidate.Id; UpdateStatus(); Inspect(); Draw(); }
        }));
        inspector.Children.Add(Action("移除屏幕", "remove-screen", async () =>
        {
            var candidate = new LayoutDocument(layout.Screens.Where(s => s.Id != screen.Id).ToList());
            if (candidate.Screens.Count == 0) { layout = candidate; selected = null; sample = false; ++revision; UpdateStatus(); Draw(); Inspect(); RefreshDevices(); }
            else if (await Commit(candidate, sample)) UpdateStatus();
        }));
        inspector.Children.Add(Text("穿越预览", "section"));
        inspector.Children.Add(Text("从屏幕中心穿过所选边缘，仅预览布局。", "caption", true));
        var direction = new ComboBox { ItemsSource = new[] { "右", "左", "上", "下" }, SelectedIndex = 0, HorizontalAlignment = HorizontalAlignment.Stretch };
        AutomationProperties.SetAutomationId(direction, "crossing-direction"); inspector.Children.Add(direction);
        inspector.Children.Add(Action("预览穿越", "preview-crossing", async () =>
        {
            busy = true; export.IsEnabled = false; long current = revision; var snapshot = layout; var edge = (string)direction.SelectedItem;
            try { var result = await Task.Run(() => CoreBridge.Crossing(snapshot, screen, edge)); if (current != revision) return; var crossing = result.GetProperty("crossing"); Message(crossing.ValueKind == JsonValueKind.Null ? "这条路径没有相邻屏幕，光标会留在当前设备。" : "预览目标：" + crossing.GetProperty("target_screen_id").GetString() + "。不会移动系统光标。"); }
            catch (Exception error) { Message(Friendly(error), true); }
            finally { busy = false; export.IsEnabled = coreReady && layout.Screens.Count > 0; }
        }));
    }
    private async void AddScreen()
    {
        var id = new TextBox { Header = "屏幕名称", Text = "屏幕 " + (layout.Screens.Count + 1) };
        var device = new ComboBox { Header = "所属设备（可新增）", IsEditable = true, ItemsSource = layout.Screens.Select(screen => screen.DeviceId).Distinct().ToList(), Text = layout.Screens.FirstOrDefault()?.DeviceId ?? "我的设备", HorizontalAlignment = HorizontalAlignment.Stretch };
        var fields = new StackPanel { Spacing = 12 }; fields.Children.Add(id); fields.Children.Add(device);
        var dialog = new ContentDialog { XamlRoot = root.XamlRoot, Title = "添加屏幕", Content = fields, PrimaryButtonText = "添加", CloseButtonText = "取消", DefaultButton = ContentDialogButton.Primary };
        if (await dialog.ShowAsync() != ContentDialogResult.Primary) return;
        double right = layout.Screens.Count == 0 ? 0 : layout.Screens.Max(s => s.X + s.Width);
        var candidate = new LayoutDocument([.. layout.Screens, new(id.Text, device.Text, right, 0, 1920, 1080)]);
        if (await Commit(candidate, sample)) { selected = id.Text; UpdateStatus(); Draw(); Inspect(); }
    }
    private async void LoadSample()
    {
        if (!coreReady) { Message("布局内核未就绪，请确认客户端已完整构建。", true); return; }
        long current = revision;
        try { var candidate = await Task.Run(() => CoreBridge.Parse(File.ReadAllText(System.IO.Path.Combine(AppContext.BaseDirectory, "sample-layout.json")))); if (current != revision) return; if (await Commit(candidate, true)) UpdateStatus(); }
        catch (Exception error) { Message(Friendly(error), true); }
    }
    private async void Import()
    {
        if (!coreReady) { Message("布局内核未就绪，请确认客户端已完整构建。", true); return; }
        long current = revision;
        var picker = new FileOpenPicker(); picker.FileTypeFilter.Add(".json");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        try
        {
            var file = await picker.PickSingleFileAsync(); if (file is null) return;
            var properties = await file.GetBasicPropertiesAsync(); if (properties.Size > 1_048_576) { Message("文件过大，请选择小于 1 MB 的布局文件。", true); return; }
            var text = await FileIO.ReadTextAsync(file);
            var candidate = await Task.Run(() => CoreBridge.Parse(text));
            if (current != revision) { Message("布局已发生变化，请重新导入文件。", true); return; }
            if (await Commit(candidate)) UpdateStatus();
        }
        catch (Exception error) { Message(Friendly(error), true); }
    }
    private async void ExportClick(object sender, RoutedEventArgs e)
    {
        if (busy || !coreReady || layout.Screens.Count == 0) return;
        busy = true; export.IsEnabled = false; var snapshot = layout; long current = revision;
        try
        {
            await Task.Run(() => CoreBridge.Validate(snapshot)); if (current != revision) return;
            var picker = new FileSavePicker { SuggestedFileName = "Crossflow-布局" }; picker.FileTypeChoices.Add("屏幕布局", new[] { ".json" });
            WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
            var file = await picker.PickSaveFileAsync(); if (file is null) return;
            await FileIO.WriteTextAsync(file, JsonSerializer.Serialize(snapshot, CoreBridge.Json)); Message("布局已导出，尚未应用到键鼠共享。");
        }
        catch (Exception error) { Message(Friendly(error), true); }
        finally { busy = false; export.IsEnabled = coreReady && layout.Screens.Count > 0; }
    }
}
