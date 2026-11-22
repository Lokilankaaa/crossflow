using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using Windows.Foundation;
using Windows.System;

namespace Crossflow.Windows;

internal sealed partial class MainWindow : Window
{
    private readonly Theme theme;
    private readonly Grid root = new();
    private readonly Grid workspace = new();
    private readonly ScrollViewer sidebarScroll = new()
    {
        HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled,
        VerticalScrollBarVisibility = ScrollBarVisibility.Auto
    };
    private readonly StackPanel sidebar = new() { Spacing = 8 };
    private readonly Ellipse statusDot = new() { Width = 9, Height = 9, VerticalAlignment = VerticalAlignment.Center };
    private readonly TextBlock statusTitle = new() { FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap };
    private readonly TextBlock statusDetail = new() { TextWrapping = TextWrapping.Wrap };
    private readonly Button primaryAction = new();
    private readonly Button stopAction = new();
    private readonly StackPanel savedPeersPanel = new() { Spacing = 6 };
    private readonly TextBlock peerHint = new() { TextWrapping = TextWrapping.Wrap };
    private readonly Button refreshPeersButton = new();
    private readonly Button addDeviceButton = new();
    private readonly StackPanel screenListPanel = new() { Spacing = 6 };

    private readonly InfoBar notice = new() { IsOpen = false, IsClosable = true };
    private readonly Canvas canvas = new();
    private readonly StackPanel inspector = new() { Spacing = 12, Padding = new Thickness(16) };
    private readonly TextBlock footerStatus = new();

    private Grid? editorGrid;
    private Border? propertiesPanel;
    private ScrollViewer? propertiesScroll;
    private Button? undoLayout, redoLayout, refreshScreensButton;
    private bool compact;

    private readonly SessionController session = new();
    private readonly DiscoveryClient discovery = new();
    private readonly PeerClient peers = new();
    private readonly PairingController pairing = new();

    private List<SavedPeer> pairedDevices = [];
    private ContentDialog? activeDialog;
    private long peerGeneration, discoveryGeneration, pairingGeneration;
    private bool pairingBusy, sessionAction, closing, allowClose;
    private bool coreReady, busy;

    private readonly LayoutEditHistory layoutHistory = new();
    private LayoutDocument layout = new([]);
    private string? selectedScreenId;
    private long revision;
    private double scale = 1, offsetX, offsetY;
    private string? lastDrawnFocus;
    private ScreenLayout? dragOriginal, dragPreview;
    private Point dragStart;
    private Border? dragVisual;
    private readonly DragCaptureOwnership dragCapture = new();

    internal MainWindow(Theme theme)
    {
        this.theme = theme;
        session.Changed += _ => DispatcherQueue.TryEnqueue(UpdateSessionUi);

        Title = "Crossflow";
        root.RequestedTheme = ElementTheme.Light;
        root.Background = theme.Brush("background");
        root.ColumnDefinitions.Add(new() { Width = new GridLength(272) });
        root.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });

        BuildSidebar();
        BuildWorkspace();

        root.SizeChanged += (_, _) => ApplyAdaptiveLayout();
        Content = root;
        AppWindow.Resize(new global::Windows.Graphics.SizeInt32(
            (int)theme.Number("window", "width"),
            (int)theme.Number("window", "height")));

        InitCanvasEvents();
        InitWindowLifecycle();
    }

    private void BuildSidebar()
    {
        sidebar.Padding = new Thickness(18, 22, 18, 20);
        sidebar.Background = theme.Brush("surface");

        // Brand
        var brand = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 10, Margin = new Thickness(2, 0, 0, 12) };
        brand.Children.Add(new FontIcon { Glyph = "\uE71B", FontSize = 22, Foreground = theme.Brush("accent") });
        brand.Children.Add(new TextBlock
        {
            Text = "Crossflow",
            FontSize = 21,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text")
        });
        sidebar.Children.Add(brand);

        // Status Card
        var statusHeader = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        statusDot.Fill = theme.Brush("text_secondary");
        statusTitle.FontSize = 14;
        statusTitle.Foreground = theme.Brush("text");
        statusTitle.Text = "未连接";
        statusHeader.Children.Add(statusDot);
        statusHeader.Children.Add(statusTitle);

        statusDetail.FontSize = 12;
        statusDetail.Foreground = theme.Brush("text_secondary");
        statusDetail.Text = "正在初始化…";

        var statusCardContent = new StackPanel { Spacing = 8 };
        statusCardContent.Children.Add(statusHeader);
        statusCardContent.Children.Add(statusDetail);

        primaryAction.Background = theme.Brush("accent");
        primaryAction.Foreground = new SolidColorBrush(Microsoft.UI.Colors.White);
        primaryAction.BorderThickness = new Thickness(0);
        primaryAction.CornerRadius = new CornerRadius(9);
        primaryAction.Padding = new Thickness(12, 10, 12, 10);
        primaryAction.HorizontalAlignment = HorizontalAlignment.Stretch;
        primaryAction.Content = "开启跨屏共享";
        AutomationProperties.SetAutomationId(primaryAction, "sidebar-session-primary");
        primaryAction.Click += (_, _) => OnPrimaryActionClick();
        statusCardContent.Children.Add(primaryAction);

        stopAction.Content = "停止连接";
        stopAction.Background = theme.Brush("background");
        stopAction.Foreground = theme.Brush("text");
        stopAction.BorderBrush = theme.Brush("border");
        stopAction.BorderThickness = new Thickness(1);
        stopAction.CornerRadius = new CornerRadius(9);
        stopAction.Padding = new Thickness(12, 8, 12, 8);
        stopAction.HorizontalAlignment = HorizontalAlignment.Stretch;
        stopAction.Visibility = Visibility.Collapsed;
        AutomationProperties.SetAutomationId(stopAction, "sidebar-session-stop");
        stopAction.Click += (_, _) => RunSessionAction(async () =>
        {
            session.CancelPendingStart();
            if (!await session.StopAsync())
                throw new InvalidOperationException("共享内核尚未退出，请再次点击停止。");
        });
        statusCardContent.Children.Add(stopAction);

        var statusCard = new Border
        {
            Child = statusCardContent,
            Background = theme.Brush("background"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(12),
            Padding = new Thickness(14),
            Margin = new Thickness(0, 0, 0, 8)
        };
        sidebar.Children.Add(statusCard);

        // Paired devices section (directly in sidebar, no page switching!)
        var peerHeader = new Grid { Margin = new Thickness(2, 6, 0, 2) };
        peerHeader.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        peerHeader.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        peerHeader.Children.Add(new TextBlock
        {
            Text = "已配对设备",
            FontSize = 12,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text_secondary"),
            VerticalAlignment = VerticalAlignment.Center
        });
        refreshPeersButton.Content = new FontIcon { Glyph = "\uE72C", FontSize = 13 };
        refreshPeersButton.Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        refreshPeersButton.BorderThickness = new Thickness(0);
        refreshPeersButton.Padding = new Thickness(6);
        ToolTipService.SetToolTip(refreshPeersButton, "刷新已配对设备");
        AutomationProperties.SetAutomationId(refreshPeersButton, "peers-refresh");
        refreshPeersButton.Click += (_, _) => _ = RefreshPeersAsync(autoLoadScreens: false);
        Grid.SetColumn(refreshPeersButton, 1);
        peerHeader.Children.Add(refreshPeersButton);
        sidebar.Children.Add(peerHeader);

        peerHint.FontSize = 12;
        peerHint.Foreground = theme.Brush("text_secondary");
        peerHint.Margin = new Thickness(2, 0, 0, 4);
        sidebar.Children.Add(peerHint);

        AutomationProperties.SetAutomationId(savedPeersPanel, "saved-peers");
        sidebar.Children.Add(savedPeersPanel);

        var addContent = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Center };
        addContent.Children.Add(new FontIcon { Glyph = "\uE710", FontSize = 13 });
        addContent.Children.Add(new TextBlock { Text = "添加设备", FontSize = 13 });
        addDeviceButton.Content = addContent;
        addDeviceButton.HorizontalAlignment = HorizontalAlignment.Stretch;
        addDeviceButton.Background = theme.Brush("background");
        addDeviceButton.BorderBrush = theme.Brush("border");
        addDeviceButton.BorderThickness = new Thickness(1);
        addDeviceButton.CornerRadius = new CornerRadius(9);
        addDeviceButton.Padding = new Thickness(10, 8, 10, 8);
        addDeviceButton.Margin = new Thickness(0, 4, 0, 0);
        AutomationProperties.SetAutomationId(addDeviceButton, "add-device");
        addDeviceButton.Click += (_, _) => OpenAddDeviceDialog();
        sidebar.Children.Add(addDeviceButton);

        sidebar.Children.Add(Divider());

        // Screens list
        sidebar.Children.Add(new TextBlock
        {
            Text = "屏幕",
            FontSize = 12,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text_secondary"),
            Margin = new Thickness(2, 0, 0, 2)
        });
        sidebar.Children.Add(screenListPanel);

        sidebarScroll.Content = sidebar;
        var sidebarBorder = new Border
        {
            Child = sidebarScroll,
            BorderThickness = new Thickness(0, 0, 1, 0),
            BorderBrush = theme.Brush("border"),
            Background = theme.Brush("surface")
        };
        Grid.SetColumn(sidebarBorder, 0);
        root.Children.Add(sidebarBorder);
    }

    private void BuildWorkspace()
    {
        workspace.Margin = new Thickness(24, 22, 24, 16);
        workspace.RowDefinitions.Add(new() { Height = GridLength.Auto }); // header + tools
        workspace.RowDefinitions.Add(new() { Height = GridLength.Auto }); // notice
        workspace.RowDefinitions.Add(new() { Height = new GridLength(1, GridUnitType.Star) }); // canvas + inspector
        workspace.RowDefinitions.Add(new() { Height = GridLength.Auto }); // footer

        // Top header bar with inline compact tools
        var header = new Grid { Margin = new Thickness(0, 0, 0, 14) };
        header.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        header.ColumnDefinitions.Add(new() { Width = GridLength.Auto });

        var titleStack = new StackPanel { Spacing = 4 };
        titleStack.Children.Add(Text("屏幕排列", "title"));
        titleStack.Children.Add(Text("拖动屏幕贴合相邻边缘；鼠标越过贴合边即可跨设备控制。", "caption", true));
        header.Children.Add(titleStack);

        var tools = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center };
        refreshScreensButton = ToolButton("刷新屏幕", "runtime-detect-screens", () =>
        {
            if (runtimePeer is not null) PrepareRuntimeLayout(runtimePeer, silentIfOffline: false);
            else _ = RefreshPeersAsync(autoLoadScreens: true);
        });
        undoLayout = ToolButton("撤销", "layout-undo", () => RestoreLayoutHistory(false));
        redoLayout = ToolButton("重做", "layout-redo", () => RestoreLayoutHistory(true));
        var fitButton = ToolButton("适应画布", "fit-layout", Draw);
        tools.Children.Add(refreshScreensButton);
        tools.Children.Add(undoLayout);
        tools.Children.Add(redoLayout);
        tools.Children.Add(fitButton);
        Grid.SetColumn(tools, 1);
        header.Children.Add(tools);
        Grid.SetRow(header, 0);
        workspace.Children.Add(header);

        // Notice bar (only visible when needed)
        notice.Margin = new Thickness(0, 0, 0, 12);
        Grid.SetRow(notice, 1);
        workspace.Children.Add(notice);

        // Main editor: canvas + right inspector
        var editor = new Grid { ColumnSpacing = 16 };
        editorGrid = editor;
        editor.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        editor.ColumnDefinitions.Add(new() { Width = new GridLength(240) });

        var board = new Border
        {
            Background = theme.Brush("surface"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(12),
            Child = canvas
        };
        editor.Children.Add(board);

        var scroll = new ScrollViewer { Content = inspector, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        propertiesScroll = scroll;
        var properties = new Border
        {
            Child = scroll,
            Background = theme.Brush("surface"),
            BorderThickness = new Thickness(1),
            BorderBrush = theme.Brush("border"),
            CornerRadius = new CornerRadius(12)
        };
        propertiesPanel = properties;
        Grid.SetColumn(properties, 1);
        editor.Children.Add(properties);
        Grid.SetRow(editor, 2);
        workspace.Children.Add(editor);

        footerStatus.FontSize = 12;
        footerStatus.Foreground = theme.Brush("text_secondary");
        footerStatus.Margin = new Thickness(2, 12, 0, 0);
        footerStatus.Text = "快捷键：Ctrl + Alt + 空格 暂停/恢复 · Ctrl + Alt + Esc 紧急停止";
        Grid.SetRow(footerStatus, 3);
        workspace.Children.Add(footerStatus);

        Grid.SetColumn(workspace, 1);
        root.Children.Add(workspace);
    }

    private void InitCanvasEvents()
    {
        AutomationProperties.SetAutomationId(canvas, "layout-canvas");
        AutomationProperties.SetName(canvas, "屏幕排列画布");
        canvas.Background = theme.Brush("background");
        canvas.SizeChanged += (_, _) => { if (dragOriginal is null) Draw(); };
        canvas.AddHandler(UIElement.PointerMovedEvent, new PointerEventHandler(DragMove), true);
        canvas.AddHandler(UIElement.PointerReleasedEvent, new PointerEventHandler(DragEnd), true);
        canvas.AddHandler(UIElement.PointerCanceledEvent, new PointerEventHandler((_, e) =>
        {
            if (dragCapture.Matches(e.Pointer.PointerId)) CancelDrag();
        }), true);
        canvas.AddHandler(UIElement.PointerCaptureLostEvent, new PointerEventHandler((_, e) =>
        {
            if (dragCapture.Lost(e.OriginalSource, canvas, e.Pointer.PointerId) && dragOriginal is not null) CancelDrag();
        }), true);
    }

    private void InitWindowLifecycle()
    {
        root.Loaded += async (_, _) =>
        {
            double density = root.XamlRoot.RasterizationScale;
            var work = Microsoft.UI.Windowing.DisplayArea.GetFromWindowId(
                AppWindow.Id, Microsoft.UI.Windowing.DisplayAreaFallback.Primary).WorkArea;
            AppWindow.Resize(new global::Windows.Graphics.SizeInt32(
                Math.Min(work.Width, (int)(theme.Number("window", "width") * density)),
                Math.Min(work.Height, (int)(theme.Number("window", "height") * density))));

            try
            {
                await Task.Run(() => CoreBridge.Request(new { op = "environment" }));
                coreReady = true;
            }
            catch
            {
                coreReady = false;
                ShowNotice("布局内核未就绪，请重新构建客户端。", isError: true);
            }

            UpdateSessionUi();
            Draw();
            Inspect();
            // Auto-load paired devices and restore screen layout immediately on launch!
            await RefreshPeersAsync(autoLoadScreens: true);
        };

        AppWindow.Changed += (_, change) =>
        {
            if (!change.DidSizeChange || root.XamlRoot is null) return;
            double density = root.XamlRoot.RasterizationScale;
            var work = Microsoft.UI.Windowing.DisplayArea.GetFromWindowId(
                AppWindow.Id, Microsoft.UI.Windowing.DisplayAreaFallback.Primary).WorkArea;
            int minW = Math.Min(work.Width, (int)(860 * density));
            int minH = Math.Min(work.Height, (int)(560 * density));
            if (AppWindow.Size.Width < minW || AppWindow.Size.Height < minH)
                AppWindow.Resize(new global::Windows.Graphics.SizeInt32(Math.Max(minW, AppWindow.Size.Width), Math.Max(minH, AppWindow.Size.Height)));
        };

        AppWindow.Closing += async (_, args) =>
        {
            if (allowClose) return;
            args.Cancel = true;
            if (closing) return;
            closing = true;
            session.CancelPendingStart();
            UpdateSessionUi();
            pairingGeneration++;
            pairing.Cancel();
            activeDialog?.Hide();
            try
            {
                var readsStopped = await Task.WhenAll(discovery.StopAsync(), peers.StopAsync(), pairing.StopAsync(), StopDisplayReadsAsync());
                bool sessionStopped = await session.StopAsync();
                if (readsStopped.All(x => x) && sessionStopped)
                {
                    allowClose = true;
                    Close();
                    return;
                }
                ShowNotice("后台进程尚未完全退出，请再次点击关闭。", isError: true);
            }
            catch
            {
                ShowNotice("停止共享时发生错误，请重试。", isError: true);
            }
            closing = false;
            UpdateSessionUi();
        };
    }

    private void OnPrimaryActionClick()
    {
        var state = session.Snapshot;
        if (!state.ProcessRunning)
        {
            if (pairedDevices.Count == 0)
            {
                OpenAddDeviceDialog();
                return;
            }
            if (runtimePeer is null)
            {
                PrepareRuntimeLayout(pairedDevices[0], silentIfOffline: false);
                return;
            }
            if (runtimeLayout is null)
            {
                PrepareRuntimeLayout(runtimePeer, silentIfOffline: false);
                return;
            }
            ApplyRuntimeLayout();
            return;
        }

        RunSessionAction(state.State == "sharing" ? session.PauseAsync : session.ArmAsync);
    }

    private async Task RefreshPeersAsync(bool autoLoadScreens)
    {
        if (closing) return;
        long gen = ++peerGeneration;
        refreshPeersButton.IsEnabled = false;
        peerHint.Text = "正在读取…";
        try
        {
            if (!await peers.StopAsync()) throw new InvalidOperationException("上次设备读取尚未完成。");
            if (gen != peerGeneration || closing) return;
            var result = await peers.ReadAsync();
            if (gen != peerGeneration || closing) return;

            pairedDevices = result.Peers.ToList();
            if (runtimePeer is not null && !pairedDevices.Any(p => p.Id == runtimePeer.Id))
            {
                runtimePeer = null;
                runtimeLayout = null;
                layout = new([]);
            }
            if (runtimePeer is null && pairedDevices.Count > 0)
                runtimePeer = pairedDevices[0];

            peerHint.Text = pairedDevices.Count == 0
                ? "暂无已配对设备，点击下方添加。"
                : result.InvalidRecords > 0
                    ? $"已保存 {pairedDevices.Count} 台（另有 {result.InvalidRecords} 条无效记录）"
                    : "";
            peerHint.Visibility = string.IsNullOrEmpty(peerHint.Text) ? Visibility.Collapsed : Visibility.Visible;

            RenderPairedDevices();
            UpdateSessionUi();

            if (autoLoadScreens && runtimePeer is not null && !session.Snapshot.ProcessRunning && coreReady)
                PrepareRuntimeLayout(runtimePeer, silentIfOffline: true);
        }
        catch (OperationCanceledException) { }
        catch (Exception error)
        {
            if (gen == peerGeneration && !closing)
            {
                peerHint.Text = error is InvalidOperationException ? error.Message : "无法读取已配对设备";
                peerHint.Visibility = Visibility.Visible;
            }
        }
        finally
        {
            if (gen == peerGeneration && !closing) refreshPeersButton.IsEnabled = true;
        }
    }

    private void RenderPairedDevices()
    {
        savedPeersPanel.Children.Clear();
        foreach (var peer in pairedDevices)
        {
            bool isSelected = runtimePeer?.Id == peer.Id;
            var row = new Grid { ColumnSpacing = 10 };
            row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });

            row.Children.Add(new FontIcon
            {
                Glyph = "\uE7F4",
                FontSize = 16,
                Foreground = theme.Brush(isSelected ? "accent" : "text_secondary"),
                VerticalAlignment = VerticalAlignment.Center
            });
            var name = new TextBlock
            {
                Text = peer.Name,
                FontSize = 13,
                FontWeight = isSelected ? Microsoft.UI.Text.FontWeights.SemiBold : Microsoft.UI.Text.FontWeights.Normal,
                Foreground = theme.Brush("text"),
                TextTrimming = TextTrimming.CharacterEllipsis,
                VerticalAlignment = VerticalAlignment.Center
            };
            Grid.SetColumn(name, 1);
            row.Children.Add(name);

            if (isSelected)
            {
                var check = new FontIcon
                {
                    Glyph = "\uE73E",
                    FontSize = 13,
                    Foreground = theme.Brush("accent"),
                    VerticalAlignment = VerticalAlignment.Center
                };
                Grid.SetColumn(check, 2);
                row.Children.Add(check);
            }

            var btn = new Button
            {
                Content = row,
                HorizontalAlignment = HorizontalAlignment.Stretch,
                HorizontalContentAlignment = HorizontalAlignment.Stretch,
                Background = theme.Brush(isSelected ? "accent_subtle" : "background"),
                BorderBrush = theme.Brush(isSelected ? "accent" : "border"),
                BorderThickness = new Thickness(1),
                CornerRadius = new CornerRadius(9),
                Padding = new Thickness(10, 9, 10, 9),
                IsEnabled = CanEditLayout
            };
            AutomationProperties.SetAutomationId(btn, "peer-select-" + peer.Id);
            btn.Click += (_, _) =>
            {
                if (!CanEditLayout) return;
                runtimePeer = peer;
                RenderPairedDevices();
                PrepareRuntimeLayout(peer, silentIfOffline: false);
            };
            savedPeersPanel.Children.Add(btn);
        }
    }

    private async void OpenAddDeviceDialog()
    {
        if (closing || pairingBusy || runtimeBusy || session.Snapshot.ProcessRunning) return;
        if (!PairingController.IsAvailable)
        {
            ShowNotice("当前构建缺少配对组件（crossflow-enroll.exe）。", isError: true);
            return;
        }

        long gen = ++pairingGeneration;
        pairingBusy = true;
        UpdateSessionUi();

        var dialogContent = new StackPanel { Spacing = 14, MinWidth = 400, MaxWidth = 460 };
        var scanHeader = new Grid();
        scanHeader.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        scanHeader.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        var scanStatus = Text("正在查找同一局域网中的 Crossflow 设备…", "caption", true);
        scanStatus.VerticalAlignment = VerticalAlignment.Center;
        scanHeader.Children.Add(scanStatus);
        var rescanBtn = new Button { Content = "重新查找", FontSize = 12, Padding = new Thickness(10, 5, 10, 5) };
        Grid.SetColumn(rescanBtn, 1);
        scanHeader.Children.Add(rescanBtn);
        dialogContent.Children.Add(scanHeader);

        var nearbyList = new StackPanel { Spacing = 8 };
        dialogContent.Children.Add(nearbyList);

        // Step 2 panel (inline inside the SAME dialog — no nested popups or page jumps!)
        var codePanel = new StackPanel { Spacing = 12, Visibility = Visibility.Collapsed };
        var codeHeader = Text("", "section");
        var codeBox = new TextBox
        {
            Header = "输入对方屏幕上显示的 8 位配对码",
            PlaceholderText = "例如 12345678",
            MaxLength = 8,
            InputScope = new InputScope { Names = { new InputScopeName { NameValue = InputScopeNameValue.Number } } }
        };
        AutomationProperties.SetAutomationId(codeBox, "pairing-code");
        var pairStatus = Text("", "caption", true);
        var fingerprintText = Text("", "caption", true);
        fingerprintText.FontFamily = new FontFamily("Consolas");
        fingerprintText.Visibility = Visibility.Collapsed;
        var backBtn = new Button { Content = "返回设备列表", FontSize = 12 };
        codePanel.Children.Add(codeHeader);
        codePanel.Children.Add(codeBox);
        codePanel.Children.Add(pairStatus);
        codePanel.Children.Add(fingerprintText);
        codePanel.Children.Add(backBtn);
        dialogContent.Children.Add(codePanel);

        var dialog = new ContentDialog
        {
            XamlRoot = root.XamlRoot,
            RequestedTheme = ElementTheme.Light,
            Title = "添加设备",
            Content = dialogContent,
            CloseButtonText = "关闭",
            DefaultButton = ContentDialogButton.Primary
        };
        activeDialog = dialog;

        string? selectedEndpoint = null;
        bool authenticated = false, approvalAttempted = false, inFlight = false, allowDialogClose = false, ending = false;
        string? savedPeerId = null;

        async Task ScanAsync()
        {
            long dgen = ++discoveryGeneration;
            rescanBtn.IsEnabled = false;
            nearbyList.Children.Clear();
            scanStatus.Text = "正在扫描局域网设备…";
            try
            {
                if (!await discovery.StopAsync()) throw new InvalidOperationException("上次扫描尚未退出。");
                if (dgen != discoveryGeneration || ending || closing) return;
                var result = await discovery.ScanAsync();
                if (dgen != discoveryGeneration || ending || closing) return;
                scanStatus.Text = result.Devices.Count == 0
                    ? "暂未发现设备，请先在 Mac 点击「＋ 添加设备」显示配对码。"
                    : $"发现 {result.Devices.Count} 台可配对设备";
                foreach (var dev in result.Devices)
                {
                    var row = new Grid { ColumnSpacing = 12 };
                    row.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
                    row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
                    var info = new StackPanel { Spacing = 2 };
                    info.Children.Add(Text(dev.Name, "body"));
                    info.Children.Add(Text(dev.Compatible ? $"{dev.Platform} · 局域网可见" : "版本不兼容", "caption", true));
                    row.Children.Add(info);
                    var choose = new Button { Content = "选择配对", IsEnabled = dev.Compatible, VerticalAlignment = VerticalAlignment.Center };
                    choose.Click += (_, _) =>
                    {
                        string? ep = null;
                        foreach (var addr in dev.Addresses)
                        {
                            try { ep = PairingController.ValidateEndpoint(addr); break; }
                            catch (InvalidOperationException) { }
                        }
                        if (ep is null) { scanStatus.Text = "该设备无可用局域网地址，请重新查找。"; return; }
                        selectedEndpoint = ep;
                        authenticated = false;
                        codeBox.Text = "";
                        codeBox.IsEnabled = true;
                        codeBox.Visibility = Visibility.Visible;
                        fingerprintText.Visibility = Visibility.Collapsed;
                        codeHeader.Text = $"配对：{dev.Name}";
                        pairStatus.Text = "输入 Mac 弹窗中显示的 8 位数字后点击「验证」。";
                        scanHeader.Visibility = Visibility.Collapsed;
                        nearbyList.Visibility = Visibility.Collapsed;
                        codePanel.Visibility = Visibility.Visible;
                        dialog.PrimaryButtonText = "验证配对码";
                        dialog.IsPrimaryButtonEnabled = true;
                    };
                    Grid.SetColumn(choose, 1);
                    row.Children.Add(choose);
                    nearbyList.Children.Add(new Border
                    {
                        Child = row,
                        Padding = new Thickness(12, 10, 12, 10),
                        Background = theme.Brush("background"),
                        BorderBrush = theme.Brush("border"),
                        BorderThickness = new Thickness(1),
                        CornerRadius = new CornerRadius(10)
                    });
                }
            }
            catch (OperationCanceledException) { }
            catch (Exception error)
            {
                if (dgen == discoveryGeneration && !ending)
                    scanStatus.Text = error is InvalidOperationException ? error.Message : "扫描失败，请重试。";
            }
            finally
            {
                if (dgen == discoveryGeneration && !ending) rescanBtn.IsEnabled = true;
            }
        }

        rescanBtn.Click += (_, _) => _ = ScanAsync();
        backBtn.Click += async (_, _) =>
        {
            if (inFlight) return;
            pairing.Cancel();
            await pairing.StopAsync();
            selectedEndpoint = null;
            authenticated = false;
            codePanel.Visibility = Visibility.Collapsed;
            scanHeader.Visibility = Visibility.Visible;
            nearbyList.Visibility = Visibility.Visible;
            dialog.PrimaryButtonText = "";
            _ = ScanAsync();
        };

        dialog.PrimaryButtonClick += async (_, args) =>
        {
            args.Cancel = true;
            if (inFlight || ending || selectedEndpoint is null) return;
            inFlight = true;
            dialog.IsPrimaryButtonEnabled = false;
            backBtn.IsEnabled = false;
            try
            {
                if (!authenticated)
                {
                    string supplied = codeBox.Text.Trim();
                    if (supplied.Length != 8 || supplied.Any(c => c is < '0' or > '9'))
                        throw new InvalidOperationException("请输入 8 位数字配对码。");
                    codeBox.Text = "";
                    codeBox.IsEnabled = false;
                    pairStatus.Text = "正在验证配对码…";
                    var peer = await pairing.BeginAsync(selectedEndpoint, supplied);
                    if (ending || closing) return;
                    authenticated = true;
                    codeBox.Visibility = Visibility.Collapsed;
                    pairStatus.Text = $"已验证设备：{peer.Name}。点击「确认配对」并在 Mac 上同时确认。";
                    fingerprintText.Text = "设备标识：" + peer.Fingerprint[^12..];
                    fingerprintText.Visibility = Visibility.Visible;
                    dialog.PrimaryButtonText = "确认配对";
                    dialog.IsPrimaryButtonEnabled = true;
                }
                else
                {
                    approvalAttempted = true;
                    pairStatus.Text = "等待 Mac 端点击确认…";
                    savedPeerId = await pairing.ApproveAsync();
                    if (ending || closing) return;
                    allowDialogClose = true;
                    dialog.Hide();
                }
            }
            catch (OperationCanceledException)
            {
                if (!ending) pairStatus.Text = "配对已取消。";
            }
            catch (Exception error)
            {
                if (!ending)
                {
                    pairStatus.Text = error is InvalidOperationException ? error.Message : "配对失败，请返回重试。";
                    if (!authenticated) { codeBox.IsEnabled = true; dialog.IsPrimaryButtonEnabled = true; }
                }
            }
            finally
            {
                inFlight = false;
                backBtn.IsEnabled = !approvalAttempted;
            }
        };

        dialog.Closing += async (_, args) =>
        {
            if (allowDialogClose) return;
            args.Cancel = true;
            if (ending) return;
            ending = true;
            dialog.IsPrimaryButtonEnabled = false;
            var deferral = args.GetDeferral();
            try
            {
                discovery.Cancel();
                pairing.Cancel();
                await Task.WhenAll(discovery.StopAsync(), pairing.StopAsync());
                allowDialogClose = true;
                args.Cancel = false;
            }
            finally { deferral.Complete(); }
        };

        _ = ScanAsync();
        await dialog.ShowAsync();
        await Task.WhenAll(discovery.StopAsync(), pairing.StopAsync());

        if (activeDialog == dialog) activeDialog = null;
        pairingBusy = false;
        UpdateSessionUi();

        if (savedPeerId is not null || approvalAttempted)
        {
            await RefreshPeersAsync(autoLoadScreens: false);
            if (savedPeerId is not null && pairedDevices.FirstOrDefault(p => p.Id == savedPeerId) is { } matched)
            {
                runtimePeer = matched;
                RenderPairedDevices();
                ShowNotice($"已配对「{matched.Name}」，正在读取两端屏幕…");
                // Give Mac a moment to auto-start listener after pairing completes
                await Task.Delay(700);
                PrepareRuntimeLayout(matched, silentIfOffline: false);
            }
        }
    }

    private async void RunSessionAction(Func<Task> operation)
    {
        if (sessionAction || pairingBusy || runtimeBusy || closing) return;
        sessionAction = true;
        UpdateSessionUi();
        string? error = null;
        try { await operation(); }
        catch (Exception failure)
        {
            error = failure is InvalidOperationException ? failure.Message : "操作未能完成，请重试。";
        }
        finally
        {
            sessionAction = false;
            UpdateSessionUi();
            if (error is not null) ShowNotice(error, isError: true);
        }
    }

    private void UpdateSessionUi()
    {
        var state = session.Snapshot;
        string title = state.State switch
        {
            "starting" => "正在启动",
            "connecting" => "正在连接",
            "paused" => "已连接 · 已暂停",
            "sharing" => "跨屏共享中",
            "stopping" => "正在停止",
            "stopped" => "已停止",
            "error" => "连接出错",
            _ => runtimeLayout is not null ? "已就绪" : pairedDevices.Count > 0 ? "等待对方上线" : "未配对"
        };

        statusTitle.Text = title;
        statusDot.Fill = state.State switch
        {
            "sharing" => new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 22, 163, 74)),
            "paused" or "connecting" or "starting" => theme.Brush("accent"),
            "error" => new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 220, 38, 38)),
            _ => runtimeLayout is not null ? theme.Brush("accent") : theme.Brush("text_secondary")
        };

        string focus = FocusDescription(state);
        statusDetail.Text = state.StopRequested ? "正在停止并释放输入…"
            : state.Error is not null ? state.Error
            : state.State == "sharing" ? (focus.Length > 0 ? focus : "移动鼠标穿过相邻屏幕边缘即可跨屏")
            : state.State == "paused" ? (session.IsStartPending ? "已连上，正在自动开启共享…" : "连接已建立，点击下方按钮恢复共享")
            : state.State == "connecting" ? (runtimePeer is null ? "正在连接…" : $"正在连接 {runtimePeer.Name}，请确认 Mac 已打开 Crossflow")
            : runtimeBusy ? "正在读取两端屏幕…"
            : runtimeLayout is not null ? $"已就绪（{layout.Screens.Count} 块屏幕），点击开启即可跨屏"
            : pairedDevices.Count > 0 ? "打开 Mac 客户端后点击「刷新屏幕」"
            : "点击下方「添加设备」完成首次配对";

        if (!state.ProcessRunning)
        {
            primaryAction.Content = pairedDevices.Count == 0 ? "添加设备"
                : runtimeLayout is not null ? "开启跨屏共享"
                : "读取屏幕";
            primaryAction.IsEnabled = !busy && !runtimeBusy && !pairingBusy && !sessionAction && !closing && (pairedDevices.Count == 0 || coreReady);
        }
        else if (session.IsStartPending)
        {
            primaryAction.Content = "正在连接并开启…";
            primaryAction.IsEnabled = false;
        }
        else if (state.State == "sharing")
        {
            primaryAction.Content = "暂停共享";
            primaryAction.IsEnabled = !state.CommandPending && !state.StopRequested && !sessionAction && !closing;
        }
        else
        {
            primaryAction.Content = state.CanArm ? "继续共享" : "等待就绪…";
            primaryAction.IsEnabled = state.CanArm && !sessionAction && !closing;
        }

        stopAction.Visibility = state.ProcessRunning ? Visibility.Visible : Visibility.Collapsed;
        stopAction.IsEnabled = state.ProcessRunning && !sessionAction && !closing;
        addDeviceButton.IsEnabled = CanEditLayout;
        refreshPeersButton.IsEnabled = CanEditLayout;

        foreach (var child in savedPeersPanel.Children.OfType<Button>())
            child.IsEnabled = CanEditLayout;

        if (state.Error is null) shownSessionError = null;
        else if (shownSessionError != (state.Generation, state.Error))
        {
            shownSessionError = (state.Generation, state.Error);
            ShowNotice(state.Error, isError: true);
        }

        string? nextFocus = state.RouteEnabled ? state.FocusScreenId : null;
        if (nextFocus != lastDrawnFocus)
        {
            lastDrawnFocus = nextFocus;
            if (layout.Screens.Count > 0 && dragOriginal is null) Draw();
        }

        UpdateRuntimeControls();
    }

    private void ShowNotice(string text, bool isError = false)
    {
        notice.Title = isError ? "提示" : "状态";
        notice.Message = text;
        notice.Severity = isError ? InfoBarSeverity.Error : InfoBarSeverity.Informational;
        notice.IsOpen = true;
    }

    private void ClearNotice() => notice.IsOpen = false;

    private void ApplyAdaptiveLayout()
    {
        bool nextCompact = root.ActualWidth > 0 && root.ActualWidth < 940;
        if (nextCompact != compact && dragOriginal is not null) CancelDrag();
        compact = nextCompact;
        if (editorGrid is null || propertiesPanel is null || propertiesScroll is null) return;
        editorGrid.ColumnDefinitions.Clear();
        editorGrid.RowDefinitions.Clear();
        editorGrid.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        if (compact)
        {
            editorGrid.RowDefinitions.Add(new() { Height = new GridLength(1, GridUnitType.Star) });
            editorGrid.RowDefinitions.Add(new() { Height = GridLength.Auto });
            editorGrid.ColumnSpacing = 0;
            editorGrid.RowSpacing = 12;
            Grid.SetColumn(propertiesPanel, 0);
            Grid.SetRow(propertiesPanel, 1);
        }
        else
        {
            editorGrid.ColumnDefinitions.Add(new() { Width = new GridLength(240) });
            editorGrid.ColumnSpacing = 16;
            editorGrid.RowSpacing = 0;
            Grid.SetColumn(propertiesPanel, 1);
            Grid.SetRow(propertiesPanel, 0);
        }
    }

    private void RefreshScreenList()
    {
        screenListPanel.Children.Clear();
        if (layout.Screens.Count == 0)
        {
            screenListPanel.Children.Add(Text("读取设备屏幕后在此显示。", "caption", true));
            return;
        }
        int groupIndex = 0;
        foreach (var group in layout.Screens.GroupBy(s => s.DeviceId))
        {
            var color = theme.Brush(groupIndex++ % 2 == 0 ? "accent" : "device_secondary");
            var heading = Text($"{DeviceName(group.Key)} · {group.Count()} 屏", "caption");
            heading.Foreground = color;
            heading.Margin = new Thickness(4, 6, 0, 2);
            screenListPanel.Children.Add(heading);
            foreach (var screen in group)
            {
                bool active = selectedScreenId == screen.Id;
                var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
                line.Children.Add(new FontIcon { Glyph = screen.Height > screen.Width ? "\uE8EA" : "\uE7F4", FontSize = 14, Foreground = color });
                line.Children.Add(new TextBlock
                {
                    Text = ScreenName(screen.Id),
                    FontSize = 12,
                    Foreground = theme.Brush("text"),
                    TextTrimming = TextTrimming.CharacterEllipsis
                });
                var btn = new Button
                {
                    Content = line,
                    HorizontalAlignment = HorizontalAlignment.Stretch,
                    HorizontalContentAlignment = HorizontalAlignment.Left,
                    BorderThickness = new Thickness(0),
                    CornerRadius = new CornerRadius(8),
                    Padding = new Thickness(10, 7, 10, 7),
                    Background = theme.Brush(active ? "accent_subtle" : "surface")
                };
                btn.Click += (_, _) =>
                {
                    selectedScreenId = screen.Id;
                    Draw();
                    Inspect();
                    RefreshScreenList();
                };
                screenListPanel.Children.Add(btn);
            }
        }
    }

    private async Task<bool> Commit(LayoutDocument candidate, bool recordHistory = true)
    {
        if (!CanEditLayout) { ShowNotice("请先点击左侧「停止连接」，再拖动屏幕排列。"); return false; }
        if (busy || !coreReady || runtimeLayout is null || runtimePeer is null) return false;
        busy = true;
        UpdateRuntimeControls();
        long attempt = ++revision;
        try
        {
            var config = RuntimeLayouts.WithLayout(runtimeLayout, candidate);
            await Task.Run(() => CoreBridge.Validate(candidate));
            if (attempt != revision) return false;
            await Task.Run(() => RuntimeLayouts.SavePreset(config));
            if (recordHistory) layoutHistory.Record(layout);
            layout = candidate;
            if (!layout.Screens.Any(x => x.Id == selectedScreenId))
                selectedScreenId = layout.Screens.FirstOrDefault()?.Id;
            ClearNotice();
            return true;
        }
        catch (Exception error)
        {
            ShowNotice(Friendly(error), isError: true);
            return false;
        }
        finally
        {
            busy = false;
            Draw();
            Inspect();
            RefreshScreenList();
            UpdateRuntimeControls();
        }
    }

    private async void RestoreLayoutHistory(bool forward)
    {
        if (!CanEditLayout || busy || !coreReady || layoutHistory.Peek(forward) is not { } candidate) return;
        var previous = layout;
        if (await Commit(candidate, false))
            layoutHistory.Complete(forward, previous);
    }

    private void Draw()
    {
        canvas.Children.Clear();
        double w = canvas.ActualWidth, h = canvas.ActualHeight;
        canvas.Clip = new RectangleGeometry { Rect = new Rect(0, 0, Math.Max(0, w), Math.Max(0, h)) };
        for (int x = 24; x < w; x += 32)
            for (int y = 24; y < h; y += 32)
            {
                var dot = new Ellipse { Width = 2, Height = 2, Fill = theme.Brush("border"), IsHitTestVisible = false };
                Canvas.SetLeft(dot, x);
                Canvas.SetTop(dot, y);
                canvas.Children.Add(dot);
            }

        if (layout.Screens.Count == 0)
        {
            string msg = pairedDevices.Count == 0
                ? "点击左侧「添加设备」完成配对，\n即可在此排列多台电脑的屏幕。"
                : runtimeBusy
                    ? "正在读取 Windows 与 Mac 的屏幕信息…"
                    : $"请确认 {runtimePeer?.Name ?? "Mac"} 已打开 Crossflow，\n然后点击右上角「刷新屏幕」。";
            var empty = Text(msg, "body", true);
            empty.TextAlignment = TextAlignment.Center;
            empty.MaxWidth = 340;
            Canvas.SetLeft(empty, Math.Max(24, (w - 340) / 2));
            Canvas.SetTop(empty, Math.Max(48, (h - 80) / 2));
            canvas.Children.Add(empty);
            return;
        }

        double minX = layout.Screens.Min(s => s.X), minY = layout.Screens.Min(s => s.Y);
        double bound = layout.Screens.Max(s => new[] { Math.Abs(s.X), Math.Abs(s.Y), Math.Abs(s.X + s.Width), Math.Abs(s.Y + s.Height), s.Width, s.Height }.Max());
        double spanX = layout.Screens.Max(s => (s.X + s.Width) / bound) - minX / bound;
        double spanY = layout.Screens.Max(s => (s.Y + s.Height) / bound) - minY / bound;
        double normalizedScale = Math.Min(Math.Max(1, w - 64) / spanX, Math.Max(1, h - 80) / spanY);
        scale = normalizedScale / bound;
        offsetX = (w - spanX * normalizedScale) / 2 - minX / bound * normalizedScale;
        offsetY = (h - spanY * normalizedScale) / 2 - minY / bound * normalizedScale;
        if (!double.IsFinite(scale) || scale <= 0) return;

        var deviceIds = layout.Screens.Select(s => s.DeviceId).Distinct().ToList();
        var focusId = session.Snapshot.RouteEnabled ? session.Snapshot.FocusScreenId : null;

        foreach (var screen in layout.Screens)
        {
            bool smallTile = screen.Height * scale <= 96;
            double padding = smallTile ? 6 : 12;
            double textWidth = Math.Max(1, screen.Width * scale - padding * 2 - 4);
            var label = new StackPanel { Spacing = smallTile ? 3 : 5, VerticalAlignment = VerticalAlignment.Center };
            var screenLabel = Text(ScreenName(screen.Id), "section");
            screenLabel.FontSize = smallTile ? 13 : 15;
            screenLabel.MaxWidth = textWidth;
            screenLabel.TextTrimming = TextTrimming.CharacterEllipsis;
            screenLabel.TextWrapping = TextWrapping.NoWrap;
            var deviceLabel = Text(DeviceName(screen.DeviceId), "caption", true);
            deviceLabel.FontSize = smallTile ? 10 : 12;
            deviceLabel.MaxWidth = textWidth;
            deviceLabel.TextTrimming = TextTrimming.CharacterEllipsis;
            deviceLabel.TextWrapping = TextWrapping.NoWrap;
            var resLabel = Text($"{screen.Width:g} × {screen.Height:g}", "caption", true);
            resLabel.FontSize = smallTile ? 10 : 11;
            resLabel.MaxWidth = textWidth;
            resLabel.TextWrapping = TextWrapping.NoWrap;
            label.Children.Add(screenLabel);
            label.Children.Add(deviceLabel);
            label.Children.Add(resLabel);

            bool isFocused = focusId == screen.Id;
            var color = theme.Brush(deviceIds.IndexOf(screen.DeviceId) % 2 == 0 ? "accent" : "device_secondary").Color;
            var tint = new SolidColorBrush(global::Windows.UI.Color.FromArgb(isFocused ? (byte)46 : (byte)22, color.R, color.G, color.B));
            var button = new ContentControl
            {
                Content = label,
                IsTabStop = true,
                UseSystemFocusVisuals = true,
                Padding = new Thickness(padding),
                HorizontalAlignment = HorizontalAlignment.Stretch,
                VerticalAlignment = VerticalAlignment.Stretch,
                HorizontalContentAlignment = HorizontalAlignment.Center,
                VerticalContentAlignment = VerticalAlignment.Center,
                Background = tint,
                CornerRadius = new CornerRadius(9)
            };
            AutomationProperties.SetAutomationId(button, "screen-" + screen.Id);
            var tile = new Border
            {
                Width = screen.Width * scale,
                Height = screen.Height * scale,
                Child = button,
                CornerRadius = new CornerRadius(10),
                BorderThickness = new Thickness(isFocused ? 3 : selectedScreenId == screen.Id ? 2 : 1),
                BorderBrush = new SolidColorBrush(color)
            };
            Canvas.SetLeft(tile, screen.X * scale + offsetX);
            Canvas.SetTop(tile, screen.Y * scale + offsetY);
            canvas.Children.Add(tile);

            button.AddHandler(UIElement.PointerPressedEvent, new PointerEventHandler((_, e) =>
            {
                selectedScreenId = screen.Id;
                Inspect();
                RefreshScreenList();
                if (!CanEditLayout)
                {
                    if (session.Snapshot.ProcessRunning)
                        ShowNotice("共享运行中已锁定排列；点击左侧「停止连接」即可拖动调整。");
                    return;
                }
                if (busy || !coreReady || !e.GetCurrentPoint(canvas).Properties.IsLeftButtonPressed) return;
                if (!canvas.CapturePointer(e.Pointer)) return;
                dragCapture.Begin(e.Pointer.PointerId);
                button.Focus(FocusState.Pointer);
                dragOriginal = screen;
                dragPreview = screen;
                dragVisual = tile;
                dragStart = e.GetCurrentPoint(canvas).Position;
                e.Handled = true;
            }), true);

            button.KeyDown += async (_, e) =>
            {
                if (!CanEditLayout || busy || !coreReady || e.Key is not (VirtualKey.Left or VirtualKey.Right or VirtualKey.Up or VirtualKey.Down)) return;
                selectedScreenId = screen.Id;
                e.Handled = true;
                double step = (Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift) & global::Windows.UI.Core.CoreVirtualKeyStates.Down) != 0 ? 50 : 10;
                var next = screen with { X = screen.X + (e.Key == VirtualKey.Left ? -step : e.Key == VirtualKey.Right ? step : 0), Y = screen.Y + (e.Key == VirtualKey.Up ? -step : e.Key == VirtualKey.Down ? step : 0) };
                await Commit(Replace(next));
            };
        }
    }

    private LayoutDocument Replace(ScreenLayout screen) =>
        new(layout.Screens.Select(s => s.Id == selectedScreenId ? screen : s).ToList());

    private void DragMove(object sender, PointerRoutedEventArgs e)
    {
        if (!dragCapture.Matches(e.Pointer.PointerId) || dragOriginal is not { } original || dragVisual is null) return;
        var point = e.GetCurrentPoint(canvas).Position;
        double x = original.X + (point.X - dragStart.X) / scale;
        double y = original.Y + (point.Y - dragStart.Y) / scale;
        if (!double.IsFinite(x) || !double.IsFinite(y)) return;
        double snap = 10 / scale;
        foreach (var other in layout.Screens.Where(s => s.Id != original.Id))
        {
            foreach (double edge in new[] { other.X - original.Width, other.X, other.X + other.Width, other.X + other.Width - original.Width })
                if (Math.Abs(x - edge) < snap) { x = edge; break; }
            foreach (double edge in new[] { other.Y - original.Height, other.Y, other.Y + other.Height, other.Y + other.Height - original.Height })
                if (Math.Abs(y - edge) < snap) { y = edge; break; }
        }
        dragPreview = original with { X = x, Y = y };
        Canvas.SetLeft(dragVisual, x * scale + offsetX);
        Canvas.SetTop(dragVisual, y * scale + offsetY);
    }

    private async void DragEnd(object sender, PointerRoutedEventArgs e)
    {
        if (!dragCapture.Matches(e.Pointer.PointerId)) return;
        dragCapture.End();
        var next = dragPreview;
        dragOriginal = null;
        dragPreview = null;
        dragVisual = null;
        canvas.ReleasePointerCaptures();
        if (next is not null) await Commit(Replace(next));
    }

    private void CancelDrag()
    {
        dragCapture.End();
        dragOriginal = null;
        dragPreview = null;
        dragVisual = null;
        canvas.ReleasePointerCaptures();
        Draw();
    }

    private void Inspect()
    {
        var screen = layout.Screens.FirstOrDefault(s => s.Id == selectedScreenId);
        inspector.Children.Clear();
        inspector.Children.Add(Text(screen is null ? "屏幕属性" : ScreenName(screen.Id), "section"));
        if (screen is null)
        {
            inspector.Children.Add(Text("点击画布中的任意屏幕查看或微调坐标。", "caption", true));
            return;
        }
        inspector.Children.Add(Text(DeviceName(screen.DeviceId), "caption", true));
        inspector.Children.Add(Text($"{screen.Width:g} × {screen.Height:g}", "caption", true));

        var x = new NumberBox { Header = "水平坐标 X", Value = screen.X, SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Compact };
        var y = new NumberBox { Header = "垂直坐标 Y", Value = screen.Y, SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Compact };
        AutomationProperties.SetAutomationId(x, "property-x");
        AutomationProperties.SetAutomationId(y, "property-y");
        inspector.Children.Add(x);
        inspector.Children.Add(y);

        var save = new Button { Content = "应用位置", HorizontalAlignment = HorizontalAlignment.Stretch };
        AutomationProperties.SetAutomationId(save, "save-properties");
        save.Click += async (_, _) =>
        {
            var candidate = screen with { X = x.Value, Y = y.Value };
            await Commit(Replace(candidate));
        };
        inspector.Children.Add(save);
    }

    private Button ToolButton(string title, string aid, Action run)
    {
        var button = new Button
        {
            Content = title,
            FontSize = 13,
            Padding = new Thickness(12, 6, 12, 6),
            Background = theme.Brush("surface"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(8)
        };
        AutomationProperties.SetAutomationId(button, aid);
        button.Click += (_, _) => { if (!busy) run(); };
        return button;
    }

    private TextBlock Text(string value, string size = "body", bool secondary = false) => new()
    {
        Text = value,
        FontSize = theme.Number("typography", size),
        Foreground = theme.Brush(secondary ? "text_secondary" : "text"),
        TextWrapping = TextWrapping.Wrap
    };

    private string DeviceName(string id) => id == "local" ? "这台 Windows" : runtimePeer?.Id == id ? runtimePeer.Name : id;
    private string ScreenName(string id) => runtimeScreenNames.TryGetValue(id, out var name) ? name : id;
    private Border Divider() => new() { Height = 1, Background = theme.Brush("border"), Margin = new Thickness(0, 8, 0, 8) };
    private static string Friendly(Exception error) => error is InvalidOperationException ? error.Message : "操作失败，请重试。";
}
