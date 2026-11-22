using System.Diagnostics;
using System.Runtime.InteropServices;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using Windows.ApplicationModel.DataTransfer;
using Windows.Foundation;
using Windows.Storage;
using Windows.Storage.Pickers;
using Windows.System;
using WinRT.Interop;

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

    // Network channel selector + Multi-IP manual peers list
    private readonly ComboBox channelCombo = new() { HorizontalAlignment = HorizontalAlignment.Stretch };
    private readonly StackPanel manualPeersListPanel = new() { Spacing = 4 };
    private readonly Button toggleAddManualButton = new();
    private readonly StackPanel addManualEditorPanel = new() { Spacing = 6, Visibility = Visibility.Collapsed };
    private readonly TextBox manualIpBox = new();
    private readonly TextBox manualLabelBox = new();
    private readonly Button confirmAddManualButton = new();
    private bool updatingChannelUi;
    private string lastChannelListSig = "";
    private string lastPeersListSig = "";
    private string lastManualPeersSig = "";
    private string? expandedPeerId;
    private bool peerChannelDropDownOpen;

    // Minimal password configuration (已配置 / 未配置)
    private readonly TextBlock passwordStateBadge = new();
    private readonly StackPanel passwordButtonsRow = new() { Orientation = Orientation.Horizontal, Spacing = 6 };
    private readonly Button editPasswordButton = new();
    private readonly Button clearPasswordButton = new();
    private readonly StackPanel passwordEditorPanel = new() { Spacing = 6, Visibility = Visibility.Collapsed };
    private readonly PasswordBox passwordBox = new();
    private readonly Button savePasswordButton = new();
    private readonly Button cancelPasswordButton = new();

    private readonly ToggleSwitch clipboardToggle = new();
    private readonly ToggleSwitch inputStateToggle = new();
    private readonly ToggleSwitch screenLockToggle = new();
    private readonly TextBlock serviceStateBadge = new();
    private readonly Button installServiceButton = new();
    private readonly Button uninstallServiceButton = new();
    private bool serviceBusy;
    private bool updatingClipboardUi;
    private bool updatingInputStateUi;
    private bool updatingScreenLockUi;

    // Customizable Hotkeys collapsible section
    private readonly Button hotkeysHeaderButton = new();
    private readonly StackPanel hotkeysPanel = new() { Spacing = 6, Visibility = Visibility.Collapsed };
    private readonly Dictionary<string, Button> hotkeyButtons = new(StringComparer.Ordinal);
    private readonly Dictionary<string, Button> hotkeyClearButtons = new(StringComparer.Ordinal);
    private string? recordingHotkeyField;
    private bool hotkeysExpanded;
    private TrayIcon? trayIcon;

    [DllImport("user32.dll")]
    private static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);

    [DllImport("user32.dll")]
    private static extern bool SetForegroundWindow(IntPtr hWnd);

    private readonly StackPanel screenListPanel = new() { Spacing = 6 };

    private readonly InfoBar notice = new() { IsOpen = false, IsClosable = true };
    private readonly Border transferBar = new() { Visibility = Visibility.Collapsed };
    private readonly TextBlock transferTitle = new() { FontSize = 12, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold };
    private readonly TextBlock transferDetail = new() { FontSize = 11 };
    private readonly ProgressBar transferProgress = new() { Minimum = 0, Maximum = 100, Height = 4 };
    private readonly Button transferActionButton = new();
    private string? transferSavedDir;
    private readonly Canvas canvas = new();
    private readonly TextBlock footerStatus = new();

    private Button? undoLayout, redoLayout;

    private readonly PeerMeshController peerMesh = new();
    private PeerMeshSnapshot currentSnap = PeerMeshSnapshot.Initial;
    private PeerSharedLayout? activeSharedLayout;
    private bool coreReady, busy, closing, allowClose, bootstrapped;
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

        Content = root;
        AppWindow.Resize(new global::Windows.Graphics.SizeInt32(
            (int)theme.Number("window", "width"),
            (int)theme.Number("window", "height")));
        string iconFile = System.IO.Path.Combine(AppContext.BaseDirectory, "Assets", "AppIcon.ico");
        if (File.Exists(iconFile))
            try { AppWindow.SetIcon(iconFile); } catch { }

        InitCanvasEvents();
        InitWindowLifecycle();
    }

    internal void StartInTray()
    {
        AppWindow.Hide();
        DispatcherQueue.TryEnqueue(async () => await BootstrapOnceAsync());
    }

    private async Task BootstrapOnceAsync()
    {
        if (bootstrapped) return;
        bootstrapped = true;
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
        await RestartPeerAsync();
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
        statusDetail.MinHeight = 36;
        statusDetail.Text = "正在初始化自动发现…";

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

        // LAN Devices section (multi-device list + manual IP list)
        sidebar.Children.Add(SectionLabel("设备列表"));
        peerHint.FontSize = 12;
        peerHint.Foreground = theme.Brush("text_secondary");
        peerHint.Text = "其他电脑打开 Crossflow 后自动发现并连接。";
        peerHint.Margin = new Thickness(2, 0, 0, 4);
        sidebar.Children.Add(peerHint);
        sidebar.Children.Add(peersPanel);

        // Manual IP list (ZeroTier / cross-subnet static peers)
        sidebar.Children.Add(manualPeersListPanel);
        toggleAddManualButton.Content = "+ 手动添加设备 IP";
        toggleAddManualButton.FontSize = 11.5;
        toggleAddManualButton.Foreground = theme.Brush("accent");
        toggleAddManualButton.Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        toggleAddManualButton.BorderThickness = new Thickness(0);
        toggleAddManualButton.Padding = new Thickness(2, 4, 2, 2);
        toggleAddManualButton.HorizontalAlignment = HorizontalAlignment.Left;
        toggleAddManualButton.Click += (_, _) =>
        {
            bool show = addManualEditorPanel.Visibility != Visibility.Visible;
            addManualEditorPanel.Visibility = show ? Visibility.Visible : Visibility.Collapsed;
            if (show) manualIpBox.Focus(FocusState.Programmatic);
        };
        sidebar.Children.Add(toggleAddManualButton);

        manualIpBox.PlaceholderText = "对方 IP（如 192.168.194.59）";
        manualIpBox.FontSize = 12;
        manualLabelBox.PlaceholderText = "备注名称（可选，如 办公室 Mac）";
        manualLabelBox.FontSize = 12;
        StyleCompactButton(confirmAddManualButton, "添加到列表");
        confirmAddManualButton.HorizontalAlignment = HorizontalAlignment.Stretch;
        confirmAddManualButton.Click += async (_, _) =>
        {
            string addr = manualIpBox.Text.Trim();
            if (string.IsNullOrEmpty(addr))
            {
                ShowNotice("请输入有效的 IPv4 地址（可选 :端口）。", isError: true);
                return;
            }
            ClearNotice();
            await peerMesh.AddManualPeerAsync(addr, manualLabelBox.Text.Trim(), currentSnap.SelectedChannel);
            manualIpBox.Text = "";
            manualLabelBox.Text = "";
            addManualEditorPanel.Visibility = Visibility.Collapsed;
        };
        addManualEditorPanel.Children.Add(manualIpBox);
        addManualEditorPanel.Children.Add(manualLabelBox);
        addManualEditorPanel.Children.Add(confirmAddManualButton);
        sidebar.Children.Add(addManualEditorPanel);

        sidebar.Children.Add(Divider());

        // Global Default Network Channel selector
        sidebar.Children.Add(SectionLabel("默认网络信道"));
        channelCombo.FontSize = 12;
        channelCombo.SelectionChanged += async (_, _) =>
        {
            if (updatingChannelUi) return;
            if (channelCombo.SelectedItem is ComboBoxItem { Tag: string channelId })
            {
                await peerMesh.SetChannelAsync(channelId, null);
            }
        };
        sidebar.Children.Add(channelCombo);

        sidebar.Children.Add(Divider());

        // Minimal Password Configuration (已配置 / 未配置)
        var passHeader = new Grid { Margin = new Thickness(2, 2, 0, 2) };
        passHeader.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        passHeader.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        passHeader.Children.Add(new TextBlock
        {
            Text = "连接密码",
            FontSize = 12,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text_secondary"),
            VerticalAlignment = VerticalAlignment.Center
        });
        passwordStateBadge.Text = "未配置";
        passwordStateBadge.FontSize = 11;
        passwordStateBadge.Foreground = theme.Brush("text_secondary");
        passwordStateBadge.VerticalAlignment = VerticalAlignment.Center;
        Grid.SetColumn(passwordStateBadge, 1);
        passHeader.Children.Add(passwordStateBadge);
        sidebar.Children.Add(passHeader);

        StyleCompactButton(editPasswordButton, "配置密码");
        StyleCompactButton(clearPasswordButton, "清除");
        clearPasswordButton.Visibility = Visibility.Collapsed;
        editPasswordButton.Click += (_, _) =>
        {
            passwordBox.Password = "";
            passwordEditorPanel.Visibility = Visibility.Visible;
            passwordButtonsRow.Visibility = Visibility.Collapsed;
            passwordBox.Focus(FocusState.Programmatic);
        };
        clearPasswordButton.Click += async (_, _) =>
        {
            passwordBox.Password = "";
            passwordEditorPanel.Visibility = Visibility.Collapsed;
            passwordButtonsRow.Visibility = Visibility.Visible;
            await peerMesh.SetPassphraseAsync(null);
        };
        passwordButtonsRow.Children.Add(editPasswordButton);
        passwordButtonsRow.Children.Add(clearPasswordButton);
        sidebar.Children.Add(passwordButtonsRow);

        passwordBox.PlaceholderText = "输入共享密码";
        passwordBox.FontSize = 12;
        StyleCompactButton(savePasswordButton, "保存");
        StyleCompactButton(cancelPasswordButton, "取消");
        savePasswordButton.Click += async (_, _) =>
        {
            string secret = passwordBox.Password.Trim();
            if (string.IsNullOrEmpty(secret))
            {
                ShowNotice("密码不能为空。", isError: true);
                return;
            }
            ClearNotice();
            passwordBox.Password = "";
            passwordEditorPanel.Visibility = Visibility.Collapsed;
            passwordButtonsRow.Visibility = Visibility.Visible;
            await peerMesh.SetPassphraseAsync(secret);
        };
        cancelPasswordButton.Click += (_, _) =>
        {
            passwordBox.Password = "";
            passwordEditorPanel.Visibility = Visibility.Collapsed;
            passwordButtonsRow.Visibility = Visibility.Visible;
        };
        var editActions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        editActions.Children.Add(savePasswordButton);
        editActions.Children.Add(cancelPasswordButton);
        passwordEditorPanel.Children.Add(passwordBox);
        passwordEditorPanel.Children.Add(editActions);
        sidebar.Children.Add(passwordEditorPanel);

        sidebar.Children.Add(Divider());

        // Sync toggles (Clipboard + Chinese/English IME & Caps Lock)
        sidebar.Children.Add(SectionLabel("共享同步"));
        clipboardToggle.Header = null;
        clipboardToggle.IsOn = true;
        clipboardToggle.OnContent = "剪贴板共享已开启";
        clipboardToggle.OffContent = "剪贴板共享已关闭";
        clipboardToggle.FontSize = 12;
        clipboardToggle.Toggled += async (_, _) =>
        {
            if (updatingClipboardUi) return;
            await peerMesh.SetClipboardAsync(clipboardToggle.IsOn);
        };
        sidebar.Children.Add(clipboardToggle);

        inputStateToggle.Header = null;
        inputStateToggle.IsOn = true;
        inputStateToggle.OnContent = "中英文与大小写同步已开启";
        inputStateToggle.OffContent = "中英文与大小写同步已关闭";
        inputStateToggle.FontSize = 12;
        inputStateToggle.Toggled += async (_, _) =>
        {
            if (updatingInputStateUi) return;
            await peerMesh.SetInputStateSyncAsync(inputStateToggle.IsOn);
        };
        sidebar.Children.Add(inputStateToggle);

        screenLockToggle.Header = null;
        screenLockToggle.IsOn = false;
        screenLockToggle.OnContent = "已锁定当前屏幕（防误触）";
        screenLockToggle.OffContent = "锁定当前屏幕（防误触）";
        screenLockToggle.FontSize = 12;
        screenLockToggle.Toggled += async (_, _) =>
        {
            if (updatingScreenLockUi) return;
            await peerMesh.SetScreenLockAsync(screenLockToggle.IsOn);
        };
        sidebar.Children.Add(screenLockToggle);

        sidebar.Children.Add(Divider());

        // Optional Windows LocalSystem Service Mode (UAC / Admin windows / Lock Screen)
        var svcHeader = new Grid { Margin = new Thickness(2, 2, 0, 0) };
        svcHeader.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        svcHeader.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        svcHeader.Children.Add(new TextBlock
        {
            Text = "系统服务模式",
            FontSize = 12,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text_secondary"),
            VerticalAlignment = VerticalAlignment.Center
        });
        serviceStateBadge.Text = "未安装";
        serviceStateBadge.FontSize = 11;
        serviceStateBadge.Foreground = theme.Brush("text_secondary");
        serviceStateBadge.VerticalAlignment = VerticalAlignment.Center;
        Grid.SetColumn(serviceStateBadge, 1);
        svcHeader.Children.Add(serviceStateBadge);
        sidebar.Children.Add(svcHeader);

        sidebar.Children.Add(new TextBlock
        {
            Text = "启用后可远程操作 UAC 弹窗、管理员窗口和锁屏界面",
            FontSize = 11,
            Foreground = theme.Brush("text_secondary"),
            TextWrapping = TextWrapping.Wrap,
            Margin = new Thickness(2, 0, 2, 2)
        });

        StyleCompactButton(installServiceButton, "安装并启用服务");
        StyleCompactButton(uninstallServiceButton, "卸载");
        uninstallServiceButton.Visibility = Visibility.Collapsed;
        installServiceButton.Click += async (_, _) => await RunElevatedServiceCommandAsync("install-service");
        uninstallServiceButton.Click += async (_, _) => await RunElevatedServiceCommandAsync("uninstall-service");
        var svcButtonsRow = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        svcButtonsRow.Children.Add(installServiceButton);
        svcButtonsRow.Children.Add(uninstallServiceButton);
        sidebar.Children.Add(svcButtonsRow);

        sidebar.Children.Add(Divider());

        // Customizable Hotkeys (collapsible so sidebar stays clean)
        BuildHotkeysSection();

        sidebar.Children.Add(Divider());

        // Screens list (identical to macOS sidebar)
        sidebar.Children.Add(SectionLabel("屏幕"));
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

    private void StyleCompactButton(Button button, string text)
    {
        button.Content = text;
        button.FontSize = 12;
        button.Background = theme.Brush("background");
        button.Foreground = theme.Brush("text");
        button.BorderBrush = theme.Brush("border");
        button.BorderThickness = new Thickness(1);
        button.CornerRadius = new CornerRadius(8);
        button.Padding = new Thickness(12, 6, 12, 6);
    }

    private TextBlock SectionLabel(string text) => new()
    {
        Text = text,
        FontSize = 12,
        FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
        Foreground = theme.Brush("text_secondary"),
        Margin = new Thickness(2, 4, 0, 2)
    };

    private void BuildHotkeysSection()
    {
        hotkeysHeaderButton.Content = "▸ 快捷键自定义";
        hotkeysHeaderButton.FontSize = 12;
        hotkeysHeaderButton.FontWeight = Microsoft.UI.Text.FontWeights.SemiBold;
        hotkeysHeaderButton.Foreground = theme.Brush("text_secondary");
        hotkeysHeaderButton.Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        hotkeysHeaderButton.BorderThickness = new Thickness(0);
        hotkeysHeaderButton.Padding = new Thickness(2, 4, 2, 4);
        hotkeysHeaderButton.HorizontalAlignment = HorizontalAlignment.Stretch;
        hotkeysHeaderButton.HorizontalContentAlignment = HorizontalAlignment.Left;
        hotkeysHeaderButton.Click += (_, _) =>
        {
            hotkeysExpanded = !hotkeysExpanded;
            hotkeysHeaderButton.Content = hotkeysExpanded ? "▾ 快捷键自定义" : "▸ 快捷键自定义";
            hotkeysPanel.Visibility = hotkeysExpanded ? Visibility.Visible : Visibility.Collapsed;
            if (!hotkeysExpanded && recordingHotkeyField is not null)
                CancelHotkeyRecording();
        };
        sidebar.Children.Add(hotkeysHeaderButton);

        var rows = new (string Field, string Label)[]
        {
            ("toggle_sharing", "暂停 / 恢复共享"),
            ("lock_screen", "锁定当前屏幕"),
            ("quick_jump_next", "QuickJump 下一台"),
            ("quick_jump_home", "QuickJump 回本机"),
            ("quick_jump_1", "QuickJump 设备 #1"),
            ("emergency_stop", "紧急停止"),
        };

        foreach (var (field, label) in rows)
        {
            var row = new Grid { ColumnSpacing = 4 };
            row.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
            row.Children.Add(new TextBlock
            {
                Text = label,
                FontSize = 11.5,
                Foreground = theme.Brush("text"),
                VerticalAlignment = VerticalAlignment.Center
            });
            var btn = new Button
            {
                Content = "—",
                FontSize = 11,
                Padding = new Thickness(7, 3, 7, 3),
                MinWidth = 76,
                Background = theme.Brush("background"),
                Foreground = theme.Brush("text"),
                BorderBrush = theme.Brush("border"),
                BorderThickness = new Thickness(1),
                CornerRadius = new CornerRadius(6)
            };
            string captured = field;
            btn.Click += (_, _) => ToggleHotkeyRecording(captured);
            Grid.SetColumn(btn, 1);
            row.Children.Add(btn);
            hotkeyButtons[field] = btn;

            var clearBtn = new Button
            {
                Content = "×",
                FontSize = 11,
                Padding = new Thickness(5, 2, 5, 2),
                MinWidth = 22,
                Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
                Foreground = theme.Brush("text_secondary"),
                BorderThickness = new Thickness(0),
                CornerRadius = new CornerRadius(6)
            };
            ToolTipService.SetToolTip(clearBtn, "清空该快捷键");
            clearBtn.Click += async (_, _) => await ClearHotkeyAsync(captured);
            Grid.SetColumn(clearBtn, 2);
            row.Children.Add(clearBtn);
            hotkeyClearButtons[field] = clearBtn;

            hotkeysPanel.Children.Add(row);
        }

        var hintAndReset = new Grid { Margin = new Thickness(0, 2, 0, 0) };
        hintAndReset.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        hintAndReset.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        hintAndReset.Children.Add(new TextBlock
        {
            Text = "点击录制 · Esc 取消 · Del 清空",
            FontSize = 10.5,
            Foreground = theme.Brush("text_secondary"),
            VerticalAlignment = VerticalAlignment.Center
        });
        var resetBtn = new Button
        {
            Content = "恢复默认",
            FontSize = 11,
            Padding = new Thickness(8, 3, 8, 3),
            Background = theme.Brush("background"),
            Foreground = theme.Brush("text_secondary"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(6)
        };
        resetBtn.Click += async (_, _) =>
        {
            CancelHotkeyRecording();
            await peerMesh.SetHotkeysAsync(HotkeyConfigInfo.Defaults);
        };
        Grid.SetColumn(resetBtn, 1);
        hintAndReset.Children.Add(resetBtn);
        hotkeysPanel.Children.Add(hintAndReset);
        sidebar.Children.Add(hotkeysPanel);

        root.PreviewKeyDown += OnRootPreviewKeyDown;
    }

    private void ToggleHotkeyRecording(string field)
    {
        if (recordingHotkeyField == field)
        {
            CancelHotkeyRecording();
            return;
        }
        recordingHotkeyField = field;
        _ = peerMesh.SetHotkeySuspendedAsync(true);
        RenderHotkeyButtons(currentSnap.Hotkeys);
    }

    private void CancelHotkeyRecording()
    {
        if (recordingHotkeyField is null) return;
        recordingHotkeyField = null;
        _ = peerMesh.SetHotkeySuspendedAsync(false);
        RenderHotkeyButtons(currentSnap.Hotkeys);
    }

    private async void OnRootPreviewKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (recordingHotkeyField is null) return;
        e.Handled = true;
        var key = e.Key;
        if (key is VirtualKey.Control or VirtualKey.LeftControl or VirtualKey.RightControl
            or VirtualKey.Menu or VirtualKey.LeftMenu or VirtualKey.RightMenu
            or VirtualKey.Shift or VirtualKey.LeftShift or VirtualKey.RightShift
            or VirtualKey.LeftWindows or VirtualKey.RightWindows)
        {
            return;
        }

        bool ctrl = (Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Control) & global::Windows.UI.Core.CoreVirtualKeyStates.Down) != 0;
        bool alt = (Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Menu) & global::Windows.UI.Core.CoreVirtualKeyStates.Down) != 0;
        bool shift = (Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift) & global::Windows.UI.Core.CoreVirtualKeyStates.Down) != 0;
        bool meta = ((Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.LeftWindows) | Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.RightWindows)) & global::Windows.UI.Core.CoreVirtualKeyStates.Down) != 0;
        bool hasMod = ctrl || alt || shift || meta;

        if (!hasMod && key == VirtualKey.Escape)
        {
            CancelHotkeyRecording();
            return;
        }

        string field = recordingHotkeyField;
        string? newChord;
        if (!hasMod && key is VirtualKey.Back or VirtualKey.Delete)
        {
            newChord = null;
        }
        else
        {
            string? keyName = VirtualKeyToChordName(key);
            bool standaloneAllowed = key >= VirtualKey.F1 && key <= VirtualKey.F20;
            if (keyName is null || (!hasMod && !standaloneAllowed))
            {
                ShowNotice("请配合 Ctrl / Alt / Shift 等修饰键（或使用 F1–F20）。", isError: true);
                return;
            }
            var parts = new List<string>(5);
            if (ctrl) parts.Add("Ctrl");
            if (alt) parts.Add("Alt");
            if (shift) parts.Add("Shift");
            if (meta) parts.Add("Meta");
            parts.Add(keyName);
            newChord = string.Join("+", parts);
        }

        ClearNotice();
        recordingHotkeyField = null;
        await peerMesh.SetHotkeySuspendedAsync(false);
        var h = currentSnap.Hotkeys;
        var updated = field switch
        {
            "toggle_sharing" => h with { ToggleSharing = newChord },
            "lock_screen" => h with { LockScreen = newChord },
            "quick_jump_next" => h with { QuickJumpNext = newChord },
            "quick_jump_home" => h with { QuickJumpHome = newChord },
            "quick_jump_1" => h with { QuickJump1 = newChord },
            "emergency_stop" => h with { EmergencyStop = newChord },
            _ => h
        };
        RenderHotkeyButtons(updated);
        await peerMesh.SetHotkeysAsync(updated);
    }

    private static string? VirtualKeyToChordName(VirtualKey key)
    {
        if (key >= VirtualKey.A && key <= VirtualKey.Z)
            return ((char)('A' + (key - VirtualKey.A))).ToString();
        if (key >= VirtualKey.Number0 && key <= VirtualKey.Number9)
            return ((char)('0' + (key - VirtualKey.Number0))).ToString();
        if (key >= VirtualKey.F1 && key <= VirtualKey.F20)
            return $"F{(int)(key - VirtualKey.F1) + 1}";
        return key switch
        {
            VirtualKey.Escape => "Esc",
            VirtualKey.Tab => "Tab",
            VirtualKey.Space => "Space",
            VirtualKey.Enter => "Enter",
            VirtualKey.Back => "Backspace",
            VirtualKey.Delete => "Delete",
            VirtualKey.Insert => "Insert",
            VirtualKey.Home => "Home",
            VirtualKey.End => "End",
            VirtualKey.PageUp => "PageUp",
            VirtualKey.PageDown => "PageDown",
            VirtualKey.Left => "Left",
            VirtualKey.Right => "Right",
            VirtualKey.Up => "Up",
            VirtualKey.Down => "Down",
            (VirtualKey)189 => "-",
            (VirtualKey)187 => "=",
            (VirtualKey)219 => "[",
            (VirtualKey)221 => "]",
            (VirtualKey)192 => "`",
            _ => null
        };
    }

    private async Task ClearHotkeyAsync(string field)
    {
        CancelHotkeyRecording();
        var h = currentSnap.Hotkeys;
        var updated = field switch
        {
            "toggle_sharing" => h with { ToggleSharing = null },
            "lock_screen" => h with { LockScreen = null },
            "quick_jump_next" => h with { QuickJumpNext = null },
            "quick_jump_home" => h with { QuickJumpHome = null },
            "quick_jump_1" => h with { QuickJump1 = null },
            "emergency_stop" => h with { EmergencyStop = null },
            _ => h
        };
        RenderHotkeyButtons(updated);
        await peerMesh.SetHotkeysAsync(updated);
    }

    private void RenderHotkeyButtons(HotkeyConfigInfo h)
    {
        string? GetVal(string f) => f switch
        {
            "toggle_sharing" => h.ToggleSharing,
            "lock_screen" => h.LockScreen,
            "quick_jump_next" => h.QuickJumpNext,
            "quick_jump_home" => h.QuickJumpHome,
            "quick_jump_1" => h.QuickJump1,
            "emergency_stop" => h.EmergencyStop,
            _ => null
        };
        foreach (var (field, btn) in hotkeyButtons)
        {
            bool rec = recordingHotkeyField == field;
            string? val = GetVal(field);
            btn.Content = rec ? "请按组合键…" : (val ?? "未设置");
            btn.BorderBrush = theme.Brush(rec ? "accent" : "border");
            btn.Foreground = theme.Brush(rec ? "accent" : (val is null ? "text_secondary" : "text"));
            if (hotkeyClearButtons.TryGetValue(field, out var clearBtn))
                clearBtn.Visibility = (!rec && val is not null) ? Visibility.Visible : Visibility.Collapsed;
        }
    }

    private void BuildWorkspace()
    {
        workspace.Margin = new Thickness(24, 22, 24, 16);
        workspace.RowDefinitions.Add(new() { Height = GridLength.Auto });
        workspace.RowDefinitions.Add(new() { Height = GridLength.Auto });
        workspace.RowDefinitions.Add(new() { Height = new GridLength(1, GridUnitType.Star) });
        workspace.RowDefinitions.Add(new() { Height = GridLength.Auto });
        workspace.RowDefinitions.Add(new() { Height = GridLength.Auto });

        // Drag & drop files onto a specific device screen (or workspace) to send!
        workspace.AllowDrop = true;
        workspace.DragOver += (_, e) =>
        {
            var connected = currentSnap.Peers.Where(p => p.Connected).ToList();
            if ((connected.Count > 0 || currentSnap.PeerDeviceId is not null)
                && e.DataView.Contains(StandardDataFormats.StorageItems))
            {
                e.AcceptedOperation = DataPackageOperation.Copy;
                string? hitDev = HitTestRemoteDeviceAtDrop(e);
                string targetLabel = hitDev is not null
                    ? DeviceName(hitDev)
                    : (connected.Count > 1 ? "全部已连接设备" : (connected.FirstOrDefault()?.Name ?? currentSnap.PeerName ?? "对方设备"));
                e.DragUIOverride.Caption = $"发送到 {targetLabel}";
            }
            else
            {
                e.AcceptedOperation = DataPackageOperation.None;
            }
        };
        workspace.Drop += async (_, e) =>
        {
            if ((currentSnap.Peers.All(p => !p.Connected) && currentSnap.PeerDeviceId is null)
                || !e.DataView.Contains(StandardDataFormats.StorageItems)) return;
            try
            {
                string? hitDev = HitTestRemoteDeviceAtDrop(e);
                var items = await e.DataView.GetStorageItemsAsync();
                var paths = items.Select(i => i.Path).Where(p => !string.IsNullOrWhiteSpace(p)).ToList();
                if (paths.Count > 0) await peerMesh.SendFilesAsync(paths, hitDev);
            }
            catch { }
        };

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

        // Full-width draggable canvas (matching macOS — no coordinate inspector!)
        var board = new Border
        {
            Background = theme.Brush("surface"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(12),
            Child = canvas
        };
        Grid.SetRow(board, 2);
        workspace.Children.Add(board);

        // Compact File Transfer Bar (only visible during/right after transfer)
        var xferGrid = new Grid { ColumnSpacing = 12 };
        xferGrid.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        xferGrid.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        var xferInfo = new StackPanel { Spacing = 4 };
        transferTitle.Foreground = theme.Brush("text");
        transferDetail.Foreground = theme.Brush("text_secondary");
        xferInfo.Children.Add(transferTitle);
        xferInfo.Children.Add(transferProgress);
        xferInfo.Children.Add(transferDetail);
        xferGrid.Children.Add(xferInfo);
        StyleCompactButton(transferActionButton, "取消");
        transferActionButton.VerticalAlignment = VerticalAlignment.Center;
        transferActionButton.Click += async (_, _) =>
        {
            if (currentSnap.Transfer?.State == "active")
                await peerMesh.CancelTransferAsync();
            else if (!string.IsNullOrEmpty(transferSavedDir))
                try { Process.Start(new ProcessStartInfo("explorer.exe", $"\"{transferSavedDir}\"") { UseShellExecute = true }); } catch { }
        };
        Grid.SetColumn(transferActionButton, 1);
        xferGrid.Children.Add(transferActionButton);
        transferBar.Child = xferGrid;
        transferBar.Background = theme.Brush("surface");
        transferBar.BorderBrush = theme.Brush("border");
        transferBar.BorderThickness = new Thickness(1);
        transferBar.CornerRadius = new CornerRadius(10);
        transferBar.Padding = new Thickness(14, 10, 14, 10);
        transferBar.Margin = new Thickness(0, 10, 0, 0);
        Grid.SetRow(transferBar, 3);
        workspace.Children.Add(transferBar);

        footerStatus.FontSize = 12;
        footerStatus.Foreground = theme.Brush("text_secondary");
        footerStatus.Margin = new Thickness(2, 12, 0, 0);
        footerStatus.Text = "快捷键：Ctrl + Alt + 空格 暂停/恢复 · Ctrl + Alt + Esc 紧急暂停 · 支持复制文件或拖入窗口跨屏直传";
        Grid.SetRow(footerStatus, 4);
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

            await BootstrapOnceAsync();
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

        AppWindow.Closing += (_, args) =>
        {
            if (allowClose) return;
            // Minimize cleanly to the system tray on X click so sharing keeps running.
            args.Cancel = true;
            AppWindow.Hide();
        };

        try
        {
            trayIcon = new TrayIcon(this, () => AppWindow.IsVisible);
            trayIcon.ShowOrRestoreRequested += () => DispatcherQueue.TryEnqueue(ShowAndActivate);
            trayIcon.HideRequested += () => DispatcherQueue.TryEnqueue(() => AppWindow.Hide());
            trayIcon.ToggleSharingRequested += () => DispatcherQueue.TryEnqueue(async () => await OnPrimaryActionClickAsync());
            trayIcon.ToggleScreenLockRequested += locked => DispatcherQueue.TryEnqueue(async () => await peerMesh.SetScreenLockAsync(locked));
            trayIcon.QuickJumpRequested += target => DispatcherQueue.TryEnqueue(async () => await peerMesh.QuickJumpAsync(target));
            trayIcon.ToggleClipboardRequested += enabled => DispatcherQueue.TryEnqueue(async () => await peerMesh.SetClipboardAsync(enabled));
            trayIcon.ToggleInputStateRequested += enabled => DispatcherQueue.TryEnqueue(async () => await peerMesh.SetInputStateSyncAsync(enabled));
            trayIcon.SelectChannelRequested += chId => DispatcherQueue.TryEnqueue(async () =>
                await peerMesh.SetChannelAsync(chId, null));
            trayIcon.SelectPeerChannelRequested += (devId, chId) => DispatcherQueue.TryEnqueue(async () =>
                await peerMesh.SetPeerChannelAsync(devId, chId));
            trayIcon.SendFilesDialogRequested += targetId => DispatcherQueue.TryEnqueue(async () => await PickAndSendFilesAsync(targetId));
            trayIcon.OpenReceiveFolderRequested += () => DispatcherQueue.TryEnqueue(OpenReceiveFolder);
            trayIcon.ExitRequested += () => DispatcherQueue.TryEnqueue(async () => await ExitApplicationAsync());
        }
        catch { }
    }

    private string? HitTestRemoteDeviceAtDrop(DragEventArgs e)
    {
        if (scale <= 0 || layout.Screens.Count == 0) return null;
        try
        {
            var pt = e.GetPosition(canvas);
            foreach (var s in layout.Screens)
            {
                if (s.DeviceId == currentSnap.LocalDeviceId || s.DeviceId == "local") continue;
                double left = offsetX + s.X * scale;
                double top = offsetY + s.Y * scale;
                double w = s.Width * scale;
                double h = s.Height * scale;
                if (pt.X >= left && pt.X <= left + w && pt.Y >= top && pt.Y <= top + h)
                    return s.DeviceId;
            }
        }
        catch { }
        return null;
    }

    private async Task PickAndSendFilesAsync(string? targetDeviceId = null)
    {
        if (currentSnap.Peers.All(p => !p.Connected) && currentSnap.PeerDeviceId is null) return;
        try
        {
            var picker = new FileOpenPicker();
            picker.FileTypeFilter.Add("*");
            InitializeWithWindow.Initialize(picker, WindowNative.GetWindowHandle(this));
            var files = await picker.PickMultipleFilesAsync();
            if (files is null || files.Count == 0) return;
            var paths = files.Select(f => f.Path).Where(p => !string.IsNullOrWhiteSpace(p)).ToList();
            if (paths.Count > 0)
                await peerMesh.SendFilesAsync(paths, targetDeviceId);
        }
        catch { }
    }

    private static void OpenReceiveFolder()
    {
        try
        {
            string dir = System.IO.Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.UserProfile),
                "Downloads", "Crossflow");
            Directory.CreateDirectory(dir);
            Process.Start(new ProcessStartInfo("explorer.exe", $"\"{dir}\"") { UseShellExecute = true });
        }
        catch { }
    }

    private void ShowAndActivate()
    {
        AppWindow.Show();
        IntPtr hwnd = WindowNative.GetWindowHandle(this);
        if (hwnd != IntPtr.Zero)
        {
            ShowWindow(hwnd, 9); // SW_RESTORE
            SetForegroundWindow(hwnd);
        }
        Activate();
    }

    private async Task ExitApplicationAsync()
    {
        if (closing) return;
        closing = true;
        try
        {
            trayIcon?.Dispose();
            trayIcon = null;
            await peerMesh.StopAsync();
            allowClose = true;
            Close();
        }
        catch
        {
            closing = false;
            ShowAndActivate();
            ShowNotice("停止对等服务时发生错误，请重试。", isError: true);
        }
    }

    private async Task RestartPeerAsync()
    {
        if (closing) return;
        try { await peerMesh.StartAsync(); }
        catch (Exception ex)
        {
            ShowNotice(ex is InvalidOperationException ? ex.Message : "无法启动对等自动发现服务。", isError: true);
        }
    }

    private async Task RunElevatedServiceCommandAsync(string subcommand)
    {
        if (serviceBusy || closing) return;
        string executable = System.IO.Path.Combine(AppContext.BaseDirectory, "crossflow.exe");
        if (!File.Exists(executable))
        {
            ShowNotice("缺少 crossflow.exe，无法操作 Windows 系统服务。", isError: true);
            return;
        }
        serviceBusy = true;
        installServiceButton.IsEnabled = false;
        uninstallServiceButton.IsEnabled = false;
        ClearNotice();
        try
        {
            var psi = new ProcessStartInfo(executable, subcommand)
            {
                WorkingDirectory = AppContext.BaseDirectory,
                UseShellExecute = true,
                Verb = "runas",
                WindowStyle = ProcessWindowStyle.Hidden
            };
            using var proc = Process.Start(psi);
            if (proc is null)
            {
                ShowNotice("无法启动管理员授权进程。", isError: true);
                return;
            }
            await proc.WaitForExitAsync();
            if (proc.ExitCode == 0)
            {
                ShowNotice(subcommand == "install-service"
                    ? "系统服务模式已启用，现在其他设备可直接操作 UAC 弹窗、管理员窗口和锁屏界面。"
                    : "系统服务模式已卸载，已切回普通便携模式。", isError: false);
            }
            else
            {
                ShowNotice($"服务操作失败（退出码 {proc.ExitCode}），请确认已允许管理员权限。", isError: true);
            }
        }
        catch (System.ComponentModel.Win32Exception ex) when (ex.NativeErrorCode == 1223)
        {
            ShowNotice("已取消管理员授权。", isError: false);
        }
        catch (Exception ex)
        {
            ShowNotice($"服务操作异常：{ex.Message}", isError: true);
        }
        finally
        {
            serviceBusy = false;
            installServiceButton.IsEnabled = !closing;
            uninstallServiceButton.IsEnabled = !closing;
        }
    }

    private async Task OnPrimaryActionClickAsync()
    {
        if (closing) return;
        switch (currentSnap.State)
        {
            case "sharing": await peerMesh.PauseAsync(); break;
            case "paused": await peerMesh.ResumeAsync(); break;
            default: await RestartPeerAsync(); break;
        }
    }

    private void OnPeerMeshUpdate(PeerMeshSnapshot snap)
    {
        currentSnap = snap;

        bool isAuthMismatch = snap.Error is not null
            && (snap.Error.StartsWith("等待密码匹配", StringComparison.Ordinal)
                || snap.Error.StartsWith("连接验证失败", StringComparison.Ordinal));

        statusTitle.Text = isAuthMismatch ? "等待密码匹配" : snap.State switch
        {
            "sharing" => "跨屏共享中",
            "paused" => "已连接 · 已暂停",
            "searching" => "正在发现设备",
            "starting" => "正在启动",
            "error" => "连接异常",
            "stopped" => "已停止",
            _ => "等待中"
        };

        statusDot.Fill = isAuthMismatch
            ? new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 217, 119, 6))
            : snap.State switch
        {
            "sharing" => new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 22, 163, 74)),
            "paused" or "searching" or "starting" => theme.Brush("accent"),
            "error" => new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 220, 38, 38)),
            _ => theme.Brush("text_secondary")
        };

        string focus = FocusDescription(snap);
        string routeInfo = snap.ActiveChannelLabel is { Length: > 0 } ch
            ? $"{ch} · {snap.PeerAddress}"
            : snap.PeerAddress ?? "";
        statusDetail.Text = snap.Error is not null ? snap.Error
            : snap.State == "sharing" ? (focus.Length > 0 ? focus : $"已连接 {snap.PeerName ?? "远端设备"}（{routeInfo}）")
            : snap.State == "paused" ? $"已连接 {snap.PeerName ?? "远端设备"} · 点击恢复共享"
            : snap.State == "searching" ? "正在自动搜索附近设备…"
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

        // Update password badge cleanly (no verbose text)
        passwordStateBadge.Text = snap.PassphraseConfigured ? "已配置" : "未配置";
        passwordStateBadge.Foreground = theme.Brush(snap.PassphraseConfigured ? "accent" : "text_secondary");
        editPasswordButton.Content = snap.PassphraseConfigured ? "修改" : "配置密码";
        clearPasswordButton.Visibility = snap.PassphraseConfigured ? Visibility.Visible : Visibility.Collapsed;

        if (clipboardToggle.IsOn != snap.ClipboardSync)
        {
            updatingClipboardUi = true;
            clipboardToggle.IsOn = snap.ClipboardSync;
            updatingClipboardUi = false;
        }
        if (inputStateToggle.IsOn != snap.InputStateSync)
        {
            updatingInputStateUi = true;
            inputStateToggle.IsOn = snap.InputStateSync;
            updatingInputStateUi = false;
        }
        if (screenLockToggle.IsOn != snap.ScreenLocked)
        {
            updatingScreenLockUi = true;
            screenLockToggle.IsOn = snap.ScreenLocked;
            updatingScreenLockUi = false;
        }
        serviceStateBadge.Text = snap.ServiceModeActive
            ? "运行中"
            : snap.ServiceModeInstalled ? "已安装" : "未安装";
        serviceStateBadge.Foreground = snap.ServiceModeActive
            ? new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 22, 163, 74))
            : theme.Brush("text_secondary");
        installServiceButton.Content = snap.ServiceModeInstalled ? "更新 / 重启服务" : "安装并启用服务";
        installServiceButton.IsEnabled = !serviceBusy && !closing;
        uninstallServiceButton.Visibility = snap.ServiceModeInstalled ? Visibility.Visible : Visibility.Collapsed;
        uninstallServiceButton.IsEnabled = !serviceBusy && !closing;
        RenderHotkeyButtons(snap.Hotkeys);
        string hkToggle = snap.Hotkeys.ToggleSharing ?? "未设置";
        string hkJump = snap.Hotkeys.QuickJumpNext ?? "未设置";
        string hkLock = snap.Hotkeys.LockScreen ?? "未设置";
        footerStatus.Text = $"快捷键：{hkToggle} 暂停/恢复 · {hkJump} 快速切屏（手柄直通）· {hkLock} 锁定屏幕 · 支持拖入文件跨屏直传";

        bool isRemoteFocus = snap.State == "sharing"
            && snap.FocusDeviceId is not null
            && !string.Equals(snap.FocusDeviceId, snap.LocalDeviceId, StringComparison.Ordinal);
        TrayVisualState trayVisual = (isAuthMismatch || snap.State is "paused" or "error" or "stopped")
            ? TrayVisualState.Paused
            : isRemoteFocus
                ? TrayVisualState.ActiveRemote
                : snap.State == "sharing"
                    ? TrayVisualState.Connected
                    : TrayVisualState.Searching;

        string peerTag = snap.PeerName ?? "远端设备";
        string channelSuffix = snap.ActiveChannelLabel is not null ? $" · {snap.ActiveChannelLabel}" : "";
        string trayTip = snap.State switch
        {
            "sharing" => $"Crossflow · 已连接 {peerTag}{channelSuffix}",
            "paused" => $"Crossflow · 已暂停（{peerTag}）",
            "searching" => "Crossflow · 正在发现设备",
            _ => "Crossflow"
        };
        string primaryMenuStatus = snap.State switch
        {
            "sharing" => $"已连接：{peerTag}{channelSuffix}",
            "paused" => $"已暂停：{peerTag}",
            _ => $"状态：{statusTitle.Text}"
        };
        string? focusMenuLine = snap.State == "sharing" && snap.FocusDeviceId is not null
            ? $"当前焦点：{(isRemoteFocus ? peerTag : "这台电脑")}{(snap.ScreenLocked ? "（已锁定）" : "")}"
            : null;

        var activePeers = snap.Peers.Where(p => p.Connected).ToList();
        if (activePeers.Count == 0 && snap.PeerDeviceId is not null && snap.State is "sharing" or "paused")
        {
            activePeers.Add(new PeerInfo(
                snap.PeerDeviceId,
                snap.PeerName ?? "远端设备",
                "peer",
                true,
                isRemoteFocus,
                1,
                snap.PeerAddress,
                snap.ActiveChannelLabel,
                "auto"));
        }

        trayIcon?.UpdateState(
            trayVisual,
            trayTip,
            primaryMenuStatus,
            focusMenuLine,
            connected: snap.State is "sharing" or "paused",
            canToggle: snap.State is "sharing" or "paused",
            sharing: snap.State == "sharing",
            locked: snap.ScreenLocked,
            activePeers: activePeers,
            clipboardSync: snap.ClipboardSync,
            inputStateSync: snap.InputStateSync,
            channelId: snap.SelectedChannel,
            availableChannels: snap.NetworkChannels);

        // Sync network channel dropdown
        SyncChannelSelector(snap);

        if (snap.Error is not null && !isAuthMismatch && snap.Error != lastShownError)
        {
            lastShownError = snap.Error;
            ShowNotice(snap.Error, isError: true);
        }
        else if ((snap.Error is null || isAuthMismatch) && lastShownError is not null)
        {
            lastShownError = null;
            ClearNotice();
        }

        RenderDiscoveredPeers(snap);
        RenderManualPeers(snap);
        UpdateTransferBar(snap.Transfer);
        SyncCanvasFromSnapshot(snap);
        UpdateControls();
    }

    private void UpdateTransferBar(TransferStatusInfo? xfer)
    {
        if (xfer is null)
        {
            transferBar.Visibility = Visibility.Collapsed;
            return;
        }
        transferBar.Visibility = Visibility.Visible;
        transferSavedDir = xfer.SavedDir;
        string dirLabel = xfer.Direction == "sending" ? "正在发送" : "正在接收";
        double pct = xfer.BytesTotal > 0 ? Math.Min(100.0, xfer.BytesDone * 100.0 / xfer.BytesTotal) : (xfer.State == "completed" ? 100.0 : 0.0);
        transferProgress.Value = pct;
        transferProgress.Visibility = xfer.State == "active" ? Visibility.Visible : Visibility.Collapsed;

        switch (xfer.State)
        {
            case "active":
                transferTitle.Text = $"{dirLabel}：{xfer.Name}";
                transferDetail.Text = $"{FormatBytes(xfer.BytesDone)} / {FormatBytes(xfer.BytesTotal)} · {pct:0}%（{xfer.FilesDone}/{xfer.FilesTotal}）";
                transferActionButton.Content = "取消";
                transferActionButton.Visibility = Visibility.Visible;
                break;
            case "completed":
                transferTitle.Text = xfer.Direction == "sending" ? $"已发送：{xfer.Name}" : $"已接收：{xfer.Name}（可直接粘贴）";
                transferDetail.Text = xfer.SavedDir is not null ? $"已保存至 {xfer.SavedDir}" : $"{FormatBytes(xfer.BytesTotal)} · 校验通过";
                transferActionButton.Content = "打开文件夹";
                transferActionButton.Visibility = xfer.SavedDir is not null ? Visibility.Visible : Visibility.Collapsed;
                break;
            case "cancelled":
                transferTitle.Text = $"已取消传输：{xfer.Name}";
                transferDetail.Text = "临时文件已清理";
                transferActionButton.Visibility = Visibility.Collapsed;
                break;
            default:
                transferTitle.Text = $"传输失败：{xfer.Name}";
                transferDetail.Text = xfer.Error ?? "请重试";
                transferActionButton.Visibility = Visibility.Collapsed;
                break;
        }
    }

    private static string FormatBytes(ulong bytes) => bytes switch
    {
        >= 1024UL * 1024 * 1024 => $"{bytes / (1024.0 * 1024 * 1024):0.##} GB",
        >= 1024UL * 1024 => $"{bytes / (1024.0 * 1024):0.#} MB",
        >= 1024UL => $"{bytes / 1024.0:0.#} KB",
        _ => $"{bytes} B"
    };

    private void SyncChannelSelector(PeerMeshSnapshot snap)
    {
        if (channelCombo.IsDropDownOpen) return;

        string itemsSig = string.Join(",", snap.NetworkChannels.Select(c => $"{c.Id}:{c.Label}"));
        updatingChannelUi = true;
        try
        {
            if (itemsSig != lastChannelListSig)
            {
                lastChannelListSig = itemsSig;
                channelCombo.Items.Clear();
                foreach (var ch in snap.NetworkChannels)
                {
                    channelCombo.Items.Add(new ComboBoxItem { Content = ch.Label, Tag = ch.Id });
                }
            }

            int desiredIndex = -1;
            for (int i = 0; i < channelCombo.Items.Count; i++)
            {
                if (channelCombo.Items[i] is ComboBoxItem { Tag: string id } && id == snap.SelectedChannel)
                {
                    desiredIndex = i;
                    break;
                }
            }
            if (desiredIndex < 0 && channelCombo.Items.Count > 0) desiredIndex = 0;
            if (desiredIndex >= 0 && channelCombo.SelectedIndex != desiredIndex)
                channelCombo.SelectedIndex = desiredIndex;
        }
        finally
        {
            updatingChannelUi = false;
        }
    }

    private void RenderManualPeers(PeerMeshSnapshot snap)
    {
        string sig = string.Join("|", snap.ManualPeers.Select(m => $"{m.Address}:{m.Label}:{m.Channel}"));
        if (sig == lastManualPeersSig) return;
        lastManualPeersSig = sig;
        manualPeersListPanel.Children.Clear();
        foreach (var mp in snap.ManualPeers)
        {
            var row = new Grid { ColumnSpacing = 6 };
            row.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });

            string title = string.IsNullOrWhiteSpace(mp.Label) ? mp.Address : $"{mp.Label} · {mp.Address}";
            row.Children.Add(new TextBlock
            {
                Text = $"手动 · {title}",
                FontSize = 11,
                Foreground = theme.Brush("text_secondary"),
                VerticalAlignment = VerticalAlignment.Center,
                TextTrimming = TextTrimming.CharacterEllipsis
            });
            var del = new Button
            {
                Content = "×",
                FontSize = 11,
                Padding = new Thickness(5, 1, 5, 1),
                MinWidth = 22,
                Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
                Foreground = theme.Brush("text_secondary"),
                BorderThickness = new Thickness(0),
                CornerRadius = new CornerRadius(5)
            };
            string addr = mp.Address;
            del.Click += async (_, _) => await peerMesh.RemoveManualPeerAsync(addr);
            Grid.SetColumn(del, 1);
            row.Children.Add(del);

            manualPeersListPanel.Children.Add(new Border
            {
                Child = row,
                Padding = new Thickness(8, 4, 4, 4),
                Background = theme.Brush("background"),
                BorderBrush = theme.Brush("border"),
                BorderThickness = new Thickness(1),
                CornerRadius = new CornerRadius(7)
            });
        }
    }

    private void RenderDiscoveredPeers(PeerMeshSnapshot snap)
    {
        if (peerChannelDropDownOpen) return;

        var items = snap.Peers.ToList();
        if (items.Count == 0 && snap.DiscoveredPeers.Count > 0)
        {
            items = snap.DiscoveredPeers.Select(d => new PeerInfo(
                d.Id, d.Name, d.Platform, d.Connected, false, d.Connected ? 1 : null,
                d.Connected ? snap.PeerAddress : null, d.Connected ? snap.ActiveChannelLabel : null, "auto")).ToList();
        }
        else if (items.Count == 0 && snap.PeerName is not null && snap.PeerDeviceId is not null)
        {
            items.Add(new PeerInfo(
                snap.PeerDeviceId, snap.PeerName, "peer", true, false, 1,
                snap.PeerAddress, snap.ActiveChannelLabel, "auto"));
        }

        string sig = (expandedPeerId ?? "")
            + "|" + string.Join(",", snap.NetworkChannels.Select(c => c.Id))
            + "|" + string.Join(";", items.Select(p => $"{p.Id}:{p.Name}:{p.Connected}:{p.Focused}:{p.Slot}:{p.Address}:{p.ActiveChannelLabel}:{p.PreferredChannel}"));
        if (sig == lastPeersListSig) return;
        lastPeersListSig = sig;

        peersPanel.Children.Clear();
        peerHint.Visibility = items.Count == 0 ? Visibility.Visible : Visibility.Collapsed;

        foreach (var peer in items)
        {
            bool isExpanded = string.Equals(expandedPeerId, peer.Id, StringComparison.Ordinal);
            var cardStack = new StackPanel { Spacing = 8 };

            var row = new Grid { ColumnSpacing = 7 };
            row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
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
            string slotPrefix = peer.Slot is int s ? $"#{s} " : "";
            info.Children.Add(new TextBlock
            {
                Text = $"{slotPrefix}{peer.Name}",
                FontSize = 13,
                FontWeight = peer.Connected ? Microsoft.UI.Text.FontWeights.SemiBold : Microsoft.UI.Text.FontWeights.Normal,
                Foreground = theme.Brush("text"),
                TextTrimming = TextTrimming.CharacterEllipsis
            });
            string sub = peer.Connected && peer.Address is not null
                ? (peer.ActiveChannelLabel is { Length: > 0 } ch ? $"{ch} · {peer.Address}" : peer.Address)
                : peer.Platform == "macos" ? "macOS" : "Windows";
            info.Children.Add(new TextBlock { Text = sub, FontSize = 11, Foreground = theme.Brush("text_secondary"), TextTrimming = TextTrimming.CharacterEllipsis });
            Grid.SetColumn(info, 1);
            row.Children.Add(info);

            var badge = new TextBlock
            {
                Text = peer.Focused ? "控制中" : (peer.Connected ? "已连接" : "已发现"),
                FontSize = 11,
                Foreground = peer.Focused
                    ? theme.Brush("accent")
                    : peer.Connected
                        ? new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, 22, 163, 74))
                        : theme.Brush("text_secondary"),
                VerticalAlignment = VerticalAlignment.Center
            };
            Grid.SetColumn(badge, 2);
            row.Children.Add(badge);

            var gear = new Button
            {
                Content = isExpanded ? "▴" : "▾",
                FontSize = 11,
                Padding = new Thickness(5, 2, 5, 2),
                MinWidth = 22,
                Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
                Foreground = theme.Brush("text_secondary"),
                BorderThickness = new Thickness(0),
                CornerRadius = new CornerRadius(5)
            };
            string peerId = peer.Id;
            ToolTipService.SetToolTip(gear, "该设备信道与快捷操作");
            gear.Click += (_, _) =>
            {
                expandedPeerId = isExpanded ? null : peerId;
                lastPeersListSig = "";
                RenderDiscoveredPeers(currentSnap);
            };
            Grid.SetColumn(gear, 3);
            row.Children.Add(gear);
            cardStack.Children.Add(row);

            if (isExpanded)
            {
                var drawer = new StackPanel { Spacing = 6, Margin = new Thickness(0, 2, 0, 0) };
                drawer.Children.Add(new TextBlock
                {
                    Text = "此设备信道",
                    FontSize = 11,
                    Foreground = theme.Brush("text_secondary")
                });
                var chBox = new ComboBox { HorizontalAlignment = HorizontalAlignment.Stretch, FontSize = 11.5 };
                int selIdx = 0;
                for (int i = 0; i < snap.NetworkChannels.Count; i++)
                {
                    var netCh = snap.NetworkChannels[i];
                    string lbl = i == 0 ? "跟随全局默认" : netCh.Label;
                    chBox.Items.Add(new ComboBoxItem { Content = lbl, Tag = netCh.Id });
                    if (string.Equals(netCh.Id, peer.PreferredChannel, StringComparison.Ordinal))
                        selIdx = i;
                }
                if (chBox.Items.Count > 0) chBox.SelectedIndex = selIdx;
                chBox.DropDownOpened += (_, _) => peerChannelDropDownOpen = true;
                chBox.DropDownClosed += (_, _) => peerChannelDropDownOpen = false;
                chBox.SelectionChanged += async (_, _) =>
                {
                    if (chBox.SelectedItem is ComboBoxItem { Tag: string cid }
                        && !string.Equals(cid, peer.PreferredChannel, StringComparison.Ordinal))
                    {
                        await peerMesh.SetPeerChannelAsync(peerId, cid);
                    }
                };
                drawer.Children.Add(chBox);

                if (peer.Connected)
                {
                    var actions = new Grid { ColumnSpacing = 6 };
                    actions.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
                    actions.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });

                    var qjBtn = new Button { HorizontalAlignment = HorizontalAlignment.Stretch };
                    StyleCompactButton(qjBtn, "QuickJump");
                    qjBtn.FontSize = 11;
                    qjBtn.Padding = new Thickness(6, 4, 6, 4);
                    qjBtn.Click += async (_, _) => await peerMesh.QuickJumpAsync(peerId);
                    actions.Children.Add(qjBtn);

                    var sendBtn = new Button { HorizontalAlignment = HorizontalAlignment.Stretch };
                    StyleCompactButton(sendBtn, "发文件…");
                    sendBtn.FontSize = 11;
                    sendBtn.Padding = new Thickness(6, 4, 6, 4);
                    sendBtn.Click += async (_, _) => await PickAndSendFilesAsync(peerId);
                    Grid.SetColumn(sendBtn, 1);
                    actions.Children.Add(sendBtn);
                    drawer.Children.Add(actions);
                }
                cardStack.Children.Add(drawer);
            }

            peersPanel.Children.Add(new Border
            {
                Child = cardStack,
                Padding = new Thickness(10, 8, 8, 8),
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
                return;
            }
        }

        if (nextFocus != lastDrawnFocus)
        {
            lastDrawnFocus = nextFocus;
            if (layout.Screens.Count > 0)
            {
                Draw();
                RefreshScreenList();
            }
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
        var matched = snap.Peers.FirstOrDefault(p => p.Id == id || id.StartsWith(p.Id, StringComparison.Ordinal) || p.Id.StartsWith(id, StringComparison.Ordinal));
        if (matched is not null) return matched.Name;
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

    private global::Windows.UI.Color DeviceColor(int index) => (index % 4) switch
    {
        0 => theme.Brush("accent").Color,                                   // Local: Cobalt Blue
        1 => theme.Brush("device_secondary").Color,                         // Peer 1: Emerald Teal
        2 => global::Windows.UI.Color.FromArgb(255, 124, 58, 237),          // Peer 2: Indigo Violet
        _ => global::Windows.UI.Color.FromArgb(255, 217, 119, 6),           // Peer 3: Warm Amber
    };

    private void RefreshScreenList()
    {
        screenListPanel.Children.Clear();
        if (layout.Screens.Count == 0)
        {
            screenListPanel.Children.Add(Text("正在读取显示器…", "caption", true));
            return;
        }
        var focusId = currentSnap.RouteEnabled ? currentSnap.FocusScreenId : null;
        int groupIndex = 0;
        foreach (var group in layout.Screens.GroupBy(s => s.DeviceId))
        {
            var color = new SolidColorBrush(DeviceColor(groupIndex++));
            var heading = Text($"{DeviceName(group.Key)} · {group.Count()} 屏", "caption");
            heading.Foreground = color;
            heading.Margin = new Thickness(4, 6, 0, 2);
            screenListPanel.Children.Add(heading);
            foreach (var screen in group)
            {
                bool active = selectedScreenId == screen.Id || focusId == screen.Id;
                var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
                line.Children.Add(new FontIcon { Glyph = screen.Height > screen.Width ? "\uE8EA" : "\uE7F4", FontSize = 14, Foreground = color });
                line.Children.Add(new TextBlock
                {
                    Text = $"{ScreenName(screen.Id)} ({screen.Width:g}×{screen.Height:g})",
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
            var color = DeviceColor(deviceIds.IndexOf(screen.DeviceId));
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
                Child = Text("正在自动发现附近 Crossflow 设备…", "caption", true)
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
