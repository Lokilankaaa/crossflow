using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using Windows.Foundation;
using Windows.System;

namespace Crossflow.Windows;

internal sealed class MainWindow : Window
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
    private readonly StackPanel peersPanel = new() { Spacing = 6 };
    private readonly TextBlock peerHint = new() { TextWrapping = TextWrapping.Wrap };
    private readonly ToggleSwitch passphraseToggle = new();
    private readonly StackPanel passphraseBoxPanel = new() { Spacing = 6, Visibility = Visibility.Collapsed };
    private readonly PasswordBox passphraseBox = new();
    private readonly Button applyPassphraseButton = new();
    private readonly StackPanel screenListPanel = new() { Spacing = 6 };

    private readonly InfoBar notice = new() { IsOpen = false, IsClosable = true };
    private readonly Canvas canvas = new();
    private readonly StackPanel inspector = new() { Spacing = 12, Padding = new Thickness(16) };
    private readonly TextBlock footerStatus = new();

    private Grid? editorGrid;
    private Border? propertiesPanel;
    private ScrollViewer? propertiesScroll;
    private Button? undoLayout, redoLayout;
    private bool compact;

    private readonly PeerMeshController peerMesh = new();
    private PeerMeshSnapshot currentSnap = PeerMeshSnapshot.Initial;
    private PeerSharedLayout? activeSharedLayout;
    private string? activePassphrase;
    private bool coreReady, busy, closing, allowClose;
    private string? lastShownError;

    private readonly LayoutEditHistory layoutHistory = new();
    private LayoutDocument layout = new([]);
    private readonly Dictionary<string, string> screenNames = new(StringComparer.Ordinal);
    private string? selectedScreenId;
    private long revision;
    private double scale = 1, offsetX, offsetY;
    private string? lastDrawnFocus;
    private string? lastLayoutSignature;
    private ScreenLayout? dragOriginal, dragPreview;
    private Point dragStart;
    private Border? dragVisual;
    private readonly DragCaptureOwnership dragCapture = new();

    internal MainWindow(Theme theme)
    {
        this.theme = theme;
        peerMesh.Changed += snap => DispatcherQueue.TryEnqueue(() => OnPeerMeshUpdate(snap));

        Title = "Crossflow";
        root.RequestedTheme = ElementTheme.Light;
        root.Background = theme.Brush("background");
        root.ColumnDefinitions.Add(new() { Width = new GridLength(276) });
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

    private bool CanEditLayout => !closing && !busy && coreReady && activeSharedLayout is not null;

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
        statusTitle.Text = "正在启动";
        statusHeader.Children.Add(statusDot);
        statusHeader.Children.Add(statusTitle);

        statusDetail.FontSize = 12;
        statusDetail.Foreground = theme.Brush("text_secondary");
        statusDetail.Text = "正在初始化局域网自动发现…";

        var statusCardContent = new StackPanel { Spacing = 8 };
        statusCardContent.Children.Add(statusHeader);
        statusCardContent.Children.Add(statusDetail);

        primaryAction.Background = theme.Brush("accent");
        primaryAction.Foreground = new SolidColorBrush(Microsoft.UI.Colors.White);
        primaryAction.BorderThickness = new Thickness(0);
        primaryAction.CornerRadius = new CornerRadius(9);
        primaryAction.Padding = new Thickness(12, 9, 12, 9);
        primaryAction.HorizontalAlignment = HorizontalAlignment.Stretch;
        primaryAction.Visibility = Visibility.Collapsed;
        AutomationProperties.SetAutomationId(primaryAction, "sidebar-session-primary");
        primaryAction.Click += async (_, _) => await OnPrimaryActionClickAsync();
        statusCardContent.Children.Add(primaryAction);

        sidebar.Children.Add(new Border
        {
            Child = statusCardContent,
            Background = theme.Brush("background"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(12),
            Padding = new Thickness(14),
            Margin = new Thickness(0, 0, 0, 6)
        });

        // LAN Devices section
        sidebar.Children.Add(new TextBlock
        {
            Text = "局域网设备",
            FontSize = 12,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text_secondary"),
            Margin = new Thickness(2, 6, 0, 2)
        });

        peerHint.FontSize = 12;
        peerHint.Foreground = theme.Brush("text_secondary");
        peerHint.Text = "在另一台电脑打开 Crossflow 即可自动发现并连接。";
        peerHint.Margin = new Thickness(2, 0, 0, 4);
        sidebar.Children.Add(peerHint);
        sidebar.Children.Add(peersPanel);

        sidebar.Children.Add(Divider());

        // Security Mode (optional passphrase)
        sidebar.Children.Add(new TextBlock
        {
            Text = "连接安全",
            FontSize = 12,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text_secondary"),
            Margin = new Thickness(2, 0, 0, 2)
        });

        passphraseToggle.Header = null;
        passphraseToggle.OffContent = "局域网自动连接（免密码）";
        passphraseToggle.OnContent = "仅限共享密码连接";
        passphraseToggle.FontSize = 12;
        passphraseToggle.Toggled += async (_, _) =>
        {
            passphraseBoxPanel.Visibility = passphraseToggle.IsOn ? Visibility.Visible : Visibility.Collapsed;
            if (!passphraseToggle.IsOn && activePassphrase is not null)
            {
                activePassphrase = null;
                passphraseBox.Password = "";
                await RestartPeerAsync();
            }
        };
        sidebar.Children.Add(passphraseToggle);

        passphraseBox.PlaceholderText = "输入两台设备一致的密码";
        passphraseBox.FontSize = 12;
        applyPassphraseButton.Content = "应用密码并重连";
        applyPassphraseButton.HorizontalAlignment = HorizontalAlignment.Stretch;
        applyPassphraseButton.Background = theme.Brush("background");
        applyPassphraseButton.BorderBrush = theme.Brush("border");
        applyPassphraseButton.BorderThickness = new Thickness(1);
        applyPassphraseButton.CornerRadius = new CornerRadius(8);
        applyPassphraseButton.Padding = new Thickness(10, 7, 10, 7);
        applyPassphraseButton.Click += async (_, _) =>
        {
            string secret = passphraseBox.Password.Trim();
            if (string.IsNullOrEmpty(secret))
            {
                ShowNotice("请输入共享密码，或关闭开关恢复免密码自动连接。", isError: true);
                return;
            }
            activePassphrase = secret;
            ClearNotice();
            await RestartPeerAsync();
        };
        passphraseBoxPanel.Children.Add(passphraseBox);
        passphraseBoxPanel.Children.Add(applyPassphraseButton);
        sidebar.Children.Add(passphraseBoxPanel);

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
        workspace.RowDefinitions.Add(new() { Height = GridLength.Auto });
        workspace.RowDefinitions.Add(new() { Height = GridLength.Auto });
        workspace.RowDefinitions.Add(new() { Height = new GridLength(1, GridUnitType.Star) });
        workspace.RowDefinitions.Add(new() { Height = GridLength.Auto });

        var header = new Grid { Margin = new Thickness(0, 0, 0, 14) };
        header.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        header.ColumnDefinitions.Add(new() { Width = GridLength.Auto });

        var titleStack = new StackPanel { Spacing = 4 };
        titleStack.Children.Add(Text("屏幕排列", "title"));
        titleStack.Children.Add(Text("拖动任意屏幕贴合相邻边缘，松手后自动同步到双方设备。", "caption", true));
        header.Children.Add(titleStack);

        var tools = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center };
        undoLayout = ToolButton("撤销", "layout-undo", () => RestoreLayoutHistory(false));
        redoLayout = ToolButton("重做", "layout-redo", () => RestoreLayoutHistory(true));
        var fitButton = ToolButton("适应画布", "fit-layout", Draw);
        tools.Children.Add(undoLayout);
        tools.Children.Add(redoLayout);
        tools.Children.Add(fitButton);
        Grid.SetColumn(tools, 1);
        header.Children.Add(tools);
        Grid.SetRow(header, 0);
        workspace.Children.Add(header);

        notice.Margin = new Thickness(0, 0, 0, 12);
        Grid.SetRow(notice, 1);
        workspace.Children.Add(notice);

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
        footerStatus.Text = "快捷键：Ctrl + Alt + 空格 暂停/恢复 · Ctrl + Alt + Esc 紧急暂停";
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

            Draw();
            Inspect();
            await RestartPeerAsync();
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
            try
            {
                await peerMesh.StopAsync();
                allowClose = true;
                Close();
            }
            catch
            {
                closing = false;
                ShowNotice("停止对等服务时发生错误，请重试。", isError: true);
            }
        };
    }

    private async Task RestartPeerAsync()
    {
        if (closing) return;
        try
        {
            await peerMesh.StartAsync(activePassphrase);
        }
        catch (Exception ex)
        {
            ShowNotice(ex is InvalidOperationException ? ex.Message : "无法启动局域网自动发现服务。", isError: true);
        }
    }

    private async Task OnPrimaryActionClickAsync()
    {
        if (closing) return;
        switch (currentSnap.State)
        {
            case "sharing":
                await peerMesh.PauseAsync();
                break;
            case "paused":
                await peerMesh.ResumeAsync();
                break;
            default:
                await RestartPeerAsync();
                break;
        }
    }

    private void OnPeerMeshUpdate(PeerMeshSnapshot snap)
    {
        currentSnap = snap;

        statusTitle.Text = snap.State switch
        {
            "sharing" => "跨屏共享中",
            "paused" => "已连接 · 已暂停",
            "searching" => "正在发现设备",
            "starting" => "正在启动",
            "error" => "连接异常",
            "stopped" => "已停止",
            _ => "等待中"
        };

        statusDot.Fill = snap.State switch
        {
            "sharing" => new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 22, 163, 74)),
            "paused" or "searching" or "starting" => theme.Brush("accent"),
            "error" => new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 220, 38, 38)),
            _ => theme.Brush("text_secondary")
        };

        string focus = FocusDescription(snap);
        statusDetail.Text = snap.Error is not null ? snap.Error
            : snap.State == "sharing" ? (focus.Length > 0 ? focus : $"已连接 {snap.PeerName ?? "远端设备"}（{snap.PeerAddress}）")
            : snap.State == "paused" ? $"已连接 {snap.PeerName ?? "远端设备"} · 点击下方按钮恢复共享"
            : snap.State == "searching"
                ? (snap.AuthMode == "passphrase" ? "已启用共享密码 · 正在自动搜索匹配设备…" : "正在自动搜索同一局域网中的 Crossflow 设备…")
            : "正在初始化…";

        if (snap.State == "sharing")
        {
            primaryAction.Visibility = Visibility.Visible;
            primaryAction.Content = "暂停共享";
            primaryAction.Background = theme.Brush("background");
            primaryAction.Foreground = theme.Brush("text");
            primaryAction.BorderBrush = theme.Brush("border");
            primaryAction.BorderThickness = new Thickness(1);
            primaryAction.IsEnabled = !closing;
        }
        else if (snap.State == "paused")
        {
            primaryAction.Visibility = Visibility.Visible;
            primaryAction.Content = "恢复共享";
            primaryAction.Background = theme.Brush("accent");
            primaryAction.Foreground = new SolidColorBrush(Microsoft.UI.Colors.White);
            primaryAction.BorderThickness = new Thickness(0);
            primaryAction.IsEnabled = !closing;
        }
        else if (snap.State is "error" or "stopped")
        {
            primaryAction.Visibility = Visibility.Visible;
            primaryAction.Content = "重新连接";
            primaryAction.Background = theme.Brush("accent");
            primaryAction.Foreground = new SolidColorBrush(Microsoft.UI.Colors.White);
            primaryAction.BorderThickness = new Thickness(0);
            primaryAction.IsEnabled = !closing;
        }
        else
        {
            primaryAction.Visibility = Visibility.Collapsed;
        }

        if (snap.Error is not null && snap.Error != lastShownError)
        {
            lastShownError = snap.Error;
            ShowNotice(snap.Error, isError: true);
        }
        else if (snap.Error is null && lastShownError is not null)
        {
            lastShownError = null;
            ClearNotice();
        }

        RenderDiscoveredPeers(snap);
        SyncCanvasFromSnapshot(snap);
        UpdateControls();
    }

    private void RenderDiscoveredPeers(PeerMeshSnapshot snap)
    {
        peersPanel.Children.Clear();
        peerHint.Visibility = snap.DiscoveredPeers.Count == 0 && snap.PeerName is null
            ? Visibility.Visible
            : Visibility.Collapsed;

        var items = snap.DiscoveredPeers.ToList();
        if (items.Count == 0 && snap.PeerName is not null && snap.PeerDeviceId is not null)
        {
            items.Add(new DiscoveredPeerInfo(snap.PeerDeviceId[..Math.Min(32, snap.PeerDeviceId.Length)], snap.PeerName, "peer", true));
        }

        foreach (var peer in items)
        {
            var row = new Grid { ColumnSpacing = 8 };
            row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });

            row.Children.Add(new FontIcon
            {
                Glyph = peer.Platform == "macos" ? "\uE7F8" : "\uE7F4",
                FontSize = 15,
                Foreground = peer.Connected
                    ? new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 22, 163, 74))
                    : theme.Brush("text_secondary"),
                VerticalAlignment = VerticalAlignment.Center
            });

            var info = new StackPanel { Spacing = 1 };
            info.Children.Add(new TextBlock
            {
                Text = peer.Name,
                FontSize = 13,
                FontWeight = peer.Connected ? Microsoft.UI.Text.FontWeights.SemiBold : Microsoft.UI.Text.FontWeights.Normal,
                Foreground = theme.Brush("text"),
                TextTrimming = TextTrimming.CharacterEllipsis
            });
            string sub = peer.Connected && snap.PeerAddress is not null
                ? snap.PeerAddress
                : peer.Platform == "macos" ? "macOS · 局域网" : "Windows · 局域网";
            info.Children.Add(new TextBlock { Text = sub, FontSize = 11, Foreground = theme.Brush("text_secondary") });
            Grid.SetColumn(info, 1);
            row.Children.Add(info);

            var badge = new TextBlock
            {
                Text = peer.Connected ? "已连接" : "已发现",
                FontSize = 11,
                Foreground = peer.Connected
                    ? new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 22, 163, 74))
                    : theme.Brush("text_secondary"),
                VerticalAlignment = VerticalAlignment.Center
            };
            Grid.SetColumn(badge, 2);
            row.Children.Add(badge);

            peersPanel.Children.Add(new Border
            {
                Child = row,
                Padding = new Thickness(10, 8, 10, 8),
                Background = theme.Brush(peer.Connected ? "accent_subtle" : "background"),
                BorderBrush = theme.Brush(peer.Connected ? "accent" : "border"),
                BorderThickness = new Thickness(1),
                CornerRadius = new CornerRadius(9)
            });
        }
    }

    private void SyncCanvasFromSnapshot(PeerMeshSnapshot snap)
    {
        if (dragOriginal is not null) return;

        string? nextFocus = snap.RouteEnabled ? snap.FocusScreenId : null;
        if (snap.SharedLayout is { Screens.Count: > 0 } shared)
        {
            string sig = string.Join("|", shared.Screens.Select(s => $"{s.Id}:{s.X:R},{s.Y:R},{s.Width:R},{s.Height:R}"));
            if (sig != lastLayoutSignature)
            {
                lastLayoutSignature = sig;
                activeSharedLayout = shared;
                layout = new LayoutDocument(shared.Screens.Select(s => new ScreenLayout(s.Id, s.DeviceId, s.X, s.Y, s.Width, s.Height)).ToList());
                RebuildScreenNames(shared, snap);
                if (!layout.Screens.Any(s => s.Id == selectedScreenId))
                    selectedScreenId = layout.Screens.FirstOrDefault()?.Id;
                lastDrawnFocus = nextFocus;
                RefreshScreenList();
                Draw();
                Inspect();
                return;
            }
        }
        else
        {
            activeSharedLayout = null;
            string localSig = "local:" + string.Join("|", snap.LocalDisplays.Select(d => $"{d.NativeId}:{d.X:R},{d.Y:R},{d.Width:R},{d.Height:R}"));
            if (localSig != lastLayoutSignature)
            {
                lastLayoutSignature = localSig;
                layout = new LayoutDocument(snap.LocalDisplays.Select((d, i) =>
                    new ScreenLayout($"local-{i + 1}", snap.LocalDeviceId ?? "local", d.X, d.Y, d.Width, d.Height)).ToList());
                screenNames.Clear();
                for (int i = 0; i < snap.LocalDisplays.Count; i++)
                    screenNames[$"local-{i + 1}"] = $"本机 · 屏幕 {i + 1}";
                selectedScreenId = layout.Screens.FirstOrDefault()?.Id;
                lastDrawnFocus = null;
                RefreshScreenList();
                Draw();
                Inspect();
                return;
            }
        }

        if (nextFocus != lastDrawnFocus)
        {
            lastDrawnFocus = nextFocus;
            if (layout.Screens.Count > 0) Draw();
        }
    }

    private void RebuildScreenNames(PeerSharedLayout shared, PeerMeshSnapshot snap)
    {
        screenNames.Clear();
        foreach (var group in shared.Screens.GroupBy(s => s.DeviceId))
        {
            string dev = DeviceName(group.Key, snap);
            int idx = 0;
            foreach (var s in group)
                screenNames[s.Id] = $"{dev} · 屏幕 {++idx}";
        }
    }

    private string DeviceName(string id, PeerMeshSnapshot? snap = null)
    {
        snap ??= currentSnap;
        if (id == snap.LocalDeviceId || id == "local") return snap.LocalName is { Length: > 0 } ln ? $"{ln} (本机)" : "本机 Windows";
        if (id == snap.PeerDeviceId) return snap.PeerName ?? "远端设备";
        return id.Length > 8 ? id[..8] : id;
    }

    private string ScreenName(string id) => screenNames.TryGetValue(id, out var name) ? name : id;

    private string FocusDescription(PeerMeshSnapshot snap)
    {
        if (!snap.RouteEnabled) return "";
        string dev = snap.FocusDeviceId is null ? "正在定位" : DeviceName(snap.FocusDeviceId, snap);
        string? scr = snap.FocusScreenId is { } id && screenNames.TryGetValue(id, out var n) ? n : null;
        return "当前控制：" + (scr ?? dev);
    }

    private void UpdateControls()
    {
        bool editable = CanEditLayout;
        if (undoLayout is not null) undoLayout.IsEnabled = editable && layoutHistory.CanUndo;
        if (redoLayout is not null) redoLayout.IsEnabled = editable && layoutHistory.CanRedo;
        if (propertiesScroll is not null) propertiesScroll.IsEnabled = editable;
        if (!editable && dragOriginal is not null) CancelDrag();
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
            screenListPanel.Children.Add(Text("正在读取显示器…", "caption", true));
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
        if (!CanEditLayout || activeSharedLayout is not { } shared) return false;
        busy = true;
        UpdateControls();
        long attempt = ++revision;
        try
        {
            await Task.Run(() => CoreBridge.Validate(candidate));
            if (attempt != revision) return false;

            var byId = candidate.Screens.ToDictionary(s => s.Id, StringComparer.Ordinal);
            var updatedShared = shared with
            {
                Screens = shared.Screens.Select(s =>
                    byId.TryGetValue(s.Id, out var moved)
                        ? s with { X = moved.X, Y = moved.Y }
                        : s).ToList()
            };

            if (recordHistory) layoutHistory.Record(layout);
            activeSharedLayout = updatedShared;
            layout = candidate;
            lastLayoutSignature = string.Join("|", updatedShared.Screens.Select(s => $"{s.Id}:{s.X:R},{s.Y:R},{s.Width:R},{s.Height:R}"));
            ClearNotice();
            await peerMesh.ApplyLayoutAsync(updatedShared);
            return true;
        }
        catch (Exception error)
        {
            ShowNotice(error is BridgeValidationException ? "屏幕位置不能重叠，请贴合边缘摆放。" : "排列应用失败，请重试。", isError: true);
            return false;
        }
        finally
        {
            busy = false;
            Draw();
            Inspect();
            RefreshScreenList();
            UpdateControls();
        }
    }

    private async void RestoreLayoutHistory(bool forward)
    {
        if (!CanEditLayout || layoutHistory.Peek(forward) is not { } candidate) return;
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
            var empty = Text("正在自动发现局域网内的 Crossflow 设备…", "body", true);
            empty.TextAlignment = TextAlignment.Center;
            empty.MaxWidth = 360;
            Canvas.SetLeft(empty, Math.Max(24, (w - 360) / 2));
            Canvas.SetTop(empty, Math.Max(48, (h - 60) / 2));
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
        var focusId = currentSnap.RouteEnabled ? currentSnap.FocusScreenId : null;

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
                if (!CanEditLayout || !e.GetCurrentPoint(canvas).Properties.IsLeftButtonPressed) return;
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
                if (!CanEditLayout || e.Key is not (VirtualKey.Left or VirtualKey.Right or VirtualKey.Up or VirtualKey.Down)) return;
                selectedScreenId = screen.Id;
                e.Handled = true;
                double step = (Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift) & global::Windows.UI.Core.CoreVirtualKeyStates.Down) != 0 ? 50 : 10;
                var next = screen with { X = screen.X + (e.Key == VirtualKey.Left ? -step : e.Key == VirtualKey.Right ? step : 0), Y = screen.Y + (e.Key == VirtualKey.Up ? -step : e.Key == VirtualKey.Down ? step : 0) };
                await Commit(Replace(next));
            };
        }

        if (activeSharedLayout is null)
        {
            var banner = new Border
            {
                Background = theme.Brush("surface"),
                BorderBrush = theme.Brush("border"),
                BorderThickness = new Thickness(1),
                CornerRadius = new CornerRadius(999),
                Padding = new Thickness(14, 6, 14, 6),
                Child = Text("正在自动发现局域网内的 Crossflow 设备…", "caption", true)
            };
            Canvas.SetLeft(banner, 16);
            Canvas.SetTop(banner, 14);
            canvas.Children.Add(banner);
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
        dragPreview = original with { X = Math.Round(x), Y = Math.Round(y) };
        Canvas.SetLeft(dragVisual, dragPreview.X * scale + offsetX);
        Canvas.SetTop(dragVisual, dragPreview.Y * scale + offsetY);
    }

    private async void DragEnd(object sender, PointerRoutedEventArgs e)
    {
        if (!dragCapture.Matches(e.Pointer.PointerId)) return;
        dragCapture.End();
        var next = dragPreview;
        var orig = dragOriginal;
        dragOriginal = null;
        dragPreview = null;
        dragVisual = null;
        canvas.ReleasePointerCaptures();
        if (next is not null && orig is not null && (next.X != orig.X || next.Y != orig.Y))
            await Commit(Replace(next));
        else
            Draw();
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

        var save = new Button { Content = "应用并同步", HorizontalAlignment = HorizontalAlignment.Stretch, IsEnabled = CanEditLayout };
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

    private Border Divider() => new() { Height = 1, Background = theme.Brush("border"), Margin = new Thickness(0, 8, 0, 8) };
}
