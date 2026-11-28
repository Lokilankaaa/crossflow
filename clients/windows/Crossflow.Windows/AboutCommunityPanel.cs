using System.Diagnostics;
using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.ApplicationModel.DataTransfer;

namespace Crossflow.Windows;

/// Quiet White「静白」About / Updater / Diagnostics & Community Support overlay modal,
/// modeled after HaloDeck's 「更新与诊断」 + 「社区与支持」 tabs.
internal sealed class AboutCommunityPanel
{
    private readonly Theme theme;
    private readonly AppUpdater updater;
    private readonly Func<PeerMeshSnapshot> getSnapshot;
    private readonly Func<Task> onApplyUpdate;
    private readonly Action<string, bool> onNotify;

    internal Grid OverlayRoot { get; } = new()
    {
        Visibility = Visibility.Collapsed,
        Background = new SolidColorBrush(ColorHelper.FromArgb(110, 15, 23, 42))
    };

    private readonly Button tabUpdatesBtn = new();
    private readonly Button tabCommunityBtn = new();
    private readonly Border tabUpdatesDot = new()
    {
        Width = 7,
        Height = 7,
        CornerRadius = new CornerRadius(4),
        Visibility = Visibility.Collapsed,
        VerticalAlignment = VerticalAlignment.Center
    };
    private readonly ScrollViewer updatesPane = new()
    {
        VerticalScrollBarVisibility = ScrollBarVisibility.Auto,
        HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled
    };
    private readonly ScrollViewer communityPane = new()
    {
        VerticalScrollBarVisibility = ScrollBarVisibility.Auto,
        HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled,
        Visibility = Visibility.Collapsed
    };

    // Update Hero controls
    private readonly Border statusPillBorder = new();
    private readonly TextBlock statusPillText = new();
    private readonly TextBlock updateMessageText = new() { TextWrapping = TextWrapping.Wrap };
    private readonly ProgressBar updateProgressBar = new() { Minimum = 0, Maximum = 100, Height = 5, Visibility = Visibility.Collapsed };
    private readonly Button checkBtn = new();
    private readonly Button applyBtn = new();
    private readonly Button notesBtn = new();
    private readonly Button cancelBtn = new();
    private readonly Border notesCard = new() { Visibility = Visibility.Collapsed };
    private readonly TextBlock notesBodyText = new() { TextWrapping = TextWrapping.Wrap };

    // Mirror & channel controls
    private readonly ComboBox mirrorCombo = new() { MinWidth = 220 };
    private readonly StackPanel customMirrorRow = new() { Orientation = Orientation.Horizontal, Spacing = 8, Visibility = Visibility.Collapsed };
    private readonly TextBox customMirrorInput = new() { PlaceholderText = "https://gh-proxy.com/", MinWidth = 300 };
    private readonly TextBlock customMirrorError = new() { FontSize = 11, Visibility = Visibility.Collapsed };
    private readonly Button channelStableBtn = new();
    private readonly Button channelPreBtn = new();
    private bool updatingControls;

    // Runtime diagnostics cells
    private readonly TextBlock diagVersionVal = new();
    private readonly TextBlock diagMirrorVal = new();
    private readonly TextBlock diagConnVal = new();
    private readonly TextBlock diagServiceVal = new();

    private string activeTab = "updates";

    internal AboutCommunityPanel(
        Theme theme,
        AppUpdater updater,
        Func<PeerMeshSnapshot> getSnapshot,
        Func<Task> onApplyUpdate,
        Action<string, bool> onNotify)
    {
        this.theme = theme;
        this.updater = updater;
        this.getSnapshot = getSnapshot;
        this.onApplyUpdate = onApplyUpdate;
        this.onNotify = onNotify;

        BuildOverlay();
        RefreshUpdaterState(updater.State);
        RefreshDiagnostics();
    }

    internal void Open(string tab = "updates")
    {
        SwitchTab(tab);
        RefreshUpdaterState(updater.State);
        RefreshDiagnostics();
        OverlayRoot.Visibility = Visibility.Visible;
    }

    internal void Close()
    {
        OverlayRoot.Visibility = Visibility.Collapsed;
    }

    internal bool IsOpen => OverlayRoot.Visibility == Visibility.Visible;

    internal void RefreshDiagnostics()
    {
        var snap = getSnapshot();
        var st = updater.State;
        diagVersionVal.Text = $"v{st.CurrentVersion} · {(st.IncludePrerelease ? "含测试版 (-rc)" : "仅正式版")}";
        diagMirrorVal.Text = AppUpdater.DescribeMirror(st.MirrorPrefix);
        int connectedCount = snap.Peers.Count(p => p.Connected);
        string ch = snap.ActiveChannelLabel ?? (snap.SelectedChannel == "auto" ? "自动择优" : snap.SelectedChannel);
        diagConnVal.Text = connectedCount > 0 ? $"已连接 {connectedCount} 台 · {ch}" : $"状态：{snap.State} · {ch}";
        diagServiceVal.Text = snap.ServiceModeActive
            ? "系统服务运行中（支持 UAC / 锁屏）"
            : (snap.ServiceModeInstalled ? "已安装（待重启刷新）" : "标准便携模式");
    }

    internal void RefreshUpdaterState(UpdaterState st)
    {
        updatingControls = true;
        try
        {
            bool busy = st.Phase is UpdaterPhase.Checking or UpdaterPhase.Downloading or UpdaterPhase.Verifying or UpdaterPhase.Applying;
            bool hasUpd = st.Release is not null && st.Phase is UpdaterPhase.Available or UpdaterPhase.Downloading or UpdaterPhase.Verifying or UpdaterPhase.Applying or UpdaterPhase.Error;

            tabUpdatesDot.Visibility = st.Phase == UpdaterPhase.Available ? Visibility.Visible : Visibility.Collapsed;

            if (st.Phase == UpdaterPhase.Available && st.Release is not null)
            {
                statusPillText.Text = $"发现新版 v{st.Release.Version}{(st.Release.IsPrerelease ? " RC" : "")}";
                statusPillBorder.Background = new SolidColorBrush(ColorHelper.FromArgb(26, 37, 99, 235));
                statusPillText.Foreground = theme.Brush("accent");
            }
            else if (st.Phase == UpdaterPhase.UpToDate)
            {
                statusPillText.Text = "已是最新版本";
                statusPillBorder.Background = new SolidColorBrush(ColorHelper.FromArgb(26, 22, 163, 74));
                statusPillText.Foreground = new SolidColorBrush(ColorHelper.FromArgb(255, 21, 128, 61));
            }
            else if (st.Phase == UpdaterPhase.Error)
            {
                statusPillText.Text = "检查/更新异常";
                statusPillBorder.Background = new SolidColorBrush(ColorHelper.FromArgb(26, 220, 38, 38));
                statusPillText.Foreground = new SolidColorBrush(ColorHelper.FromArgb(255, 185, 28, 28));
            }
            else if (busy)
            {
                statusPillText.Text = st.Phase == UpdaterPhase.Checking ? "检查中…" : "更新中…";
                statusPillBorder.Background = new SolidColorBrush(ColorHelper.FromArgb(26, 37, 99, 235));
                statusPillText.Foreground = theme.Brush("accent");
            }
            else
            {
                statusPillText.Text = "点击检查更新";
                statusPillBorder.Background = theme.Brush("surface");
                statusPillText.Foreground = theme.Brush("text_secondary");
            }

            updateMessageText.Text = st.Message;
            updateMessageText.Foreground = st.Phase switch
            {
                UpdaterPhase.Error => new SolidColorBrush(ColorHelper.FromArgb(255, 220, 38, 38)),
                UpdaterPhase.Available => theme.Brush("accent"),
                _ => theme.Brush("text_secondary")
            };

            bool showProg = st.Phase is UpdaterPhase.Downloading or UpdaterPhase.Verifying or UpdaterPhase.Applying;
            updateProgressBar.Visibility = showProg ? Visibility.Visible : Visibility.Collapsed;
            updateProgressBar.IsIndeterminate = st.Phase is UpdaterPhase.Verifying or UpdaterPhase.Applying;
            if (st.Phase == UpdaterPhase.Downloading)
                updateProgressBar.Value = Math.Clamp(st.Progress * 100.0, 0, 100);

            checkBtn.IsEnabled = !busy;
            checkBtn.Content = st.Phase == UpdaterPhase.Checking ? "检查中…" : "立即检查更新";
            checkBtn.Visibility = st.Phase is UpdaterPhase.Downloading or UpdaterPhase.Verifying or UpdaterPhase.Applying
                ? Visibility.Collapsed : Visibility.Visible;

            bool canApply = st.Release is not null && st.Phase is UpdaterPhase.Available or UpdaterPhase.Error;
            applyBtn.Visibility = canApply ? Visibility.Visible : Visibility.Collapsed;
            applyBtn.IsEnabled = canApply;
            if (st.Release is not null)
                applyBtn.Content = $"一键更新 v{st.Release.Version}";

            notesBtn.Visibility = hasUpd && st.Release is not null && !string.IsNullOrWhiteSpace(st.Release.HtmlUrl)
                ? Visibility.Visible : Visibility.Collapsed;
            cancelBtn.Visibility = st.Phase == UpdaterPhase.Downloading ? Visibility.Visible : Visibility.Collapsed;

            if (st.Release is not null && !string.IsNullOrWhiteSpace(st.Release.Notes))
            {
                notesBodyText.Text = st.Release.Notes;
                notesCard.Visibility = Visibility.Visible;
            }
            else
            {
                notesCard.Visibility = Visibility.Collapsed;
            }

            // Mirror selector sync
            string presetId = AppUpdater.ResolveMirrorPresetId(st.MirrorPrefix);
            for (int i = 0; i < AppUpdater.MirrorPresets.Count; i++)
            {
                if (AppUpdater.MirrorPresets[i].Id == presetId)
                {
                    if (mirrorCombo.SelectedIndex != i)
                        mirrorCombo.SelectedIndex = i;
                    break;
                }
            }
            if (presetId == "custom")
            {
                customMirrorRow.Visibility = Visibility.Visible;
                if (string.IsNullOrWhiteSpace(customMirrorInput.Text))
                    customMirrorInput.Text = st.MirrorPrefix;
            }
            else if (mirrorCombo.SelectedIndex != AppUpdater.MirrorPresets.Count - 1)
            {
                customMirrorRow.Visibility = Visibility.Collapsed;
                customMirrorError.Visibility = Visibility.Collapsed;
            }

            StyleSegment(channelStableBtn, !st.IncludePrerelease);
            StyleSegment(channelPreBtn, st.IncludePrerelease);
            RefreshDiagnostics();
        }
        finally
        {
            updatingControls = false;
        }
    }

    private void BuildOverlay()
    {
        // Click backdrop to dismiss
        var backdrop = new Border
        {
            Background = new SolidColorBrush(Colors.Transparent),
            HorizontalAlignment = HorizontalAlignment.Stretch,
            VerticalAlignment = VerticalAlignment.Stretch
        };
        backdrop.PointerPressed += (_, _) => Close();
        OverlayRoot.Children.Add(backdrop);

        var modal = new Border
        {
            Background = theme.Brush("background"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(16),
            MaxWidth = 760,
            MaxHeight = 640,
            Margin = new Thickness(32),
            HorizontalAlignment = HorizontalAlignment.Center,
            VerticalAlignment = VerticalAlignment.Center
        };

        var modalGrid = new Grid();
        modalGrid.RowDefinitions.Add(new() { Height = GridLength.Auto });
        modalGrid.RowDefinitions.Add(new() { Height = new GridLength(1, GridUnitType.Star) });

        // Top header bar with 2 tabs + close button
        var headerGrid = new Grid
        {
            Padding = new Thickness(20, 14, 16, 14),
            Background = theme.Brush("surface"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(0, 0, 0, 1)
        };
        headerGrid.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        headerGrid.ColumnDefinitions.Add(new() { Width = GridLength.Auto });

        var tabsRow = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center };

        tabUpdatesDot.Background = theme.Brush("accent");
        var updTabContent = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        updTabContent.Children.Add(new FontIcon { Glyph = "\uE777", FontSize = 13 });
        updTabContent.Children.Add(new TextBlock { Text = "更新与诊断", FontSize = 13, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold });
        updTabContent.Children.Add(tabUpdatesDot);
        tabUpdatesBtn.Content = updTabContent;
        tabUpdatesBtn.CornerRadius = new CornerRadius(9);
        tabUpdatesBtn.Padding = new Thickness(14, 7, 14, 7);
        tabUpdatesBtn.Click += (_, _) => SwitchTab("updates");

        var comTabContent = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        comTabContent.Children.Add(new FontIcon { Glyph = "\uEB51", FontSize = 13 });
        comTabContent.Children.Add(new TextBlock { Text = "社区与支持", FontSize = 13, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold });
        tabCommunityBtn.Content = comTabContent;
        tabCommunityBtn.CornerRadius = new CornerRadius(9);
        tabCommunityBtn.Padding = new Thickness(14, 7, 14, 7);
        tabCommunityBtn.Click += (_, _) => SwitchTab("community");

        tabsRow.Children.Add(tabUpdatesBtn);
        tabsRow.Children.Add(tabCommunityBtn);
        headerGrid.Children.Add(tabsRow);

        var closeBtn = new Button
        {
            Content = new FontIcon { Glyph = "\uE711", FontSize = 12 },
            Background = new SolidColorBrush(Colors.Transparent),
            BorderThickness = new Thickness(0),
            CornerRadius = new CornerRadius(8),
            Padding = new Thickness(8, 6, 8, 6),
            Foreground = theme.Brush("text_secondary")
        };
        closeBtn.Click += (_, _) => Close();
        Grid.SetColumn(closeBtn, 1);
        headerGrid.Children.Add(closeBtn);

        modalGrid.Children.Add(headerGrid);

        // Content host
        var bodyHost = new Grid { Padding = new Thickness(22, 18, 22, 20) };
        updatesPane.Content = BuildUpdatesTab();
        communityPane.Content = BuildCommunityTab();
        bodyHost.Children.Add(updatesPane);
        bodyHost.Children.Add(communityPane);
        Grid.SetRow(bodyHost, 1);
        modalGrid.Children.Add(bodyHost);

        modal.Child = modalGrid;
        OverlayRoot.Children.Add(modal);

        SwitchTab("updates");
    }

    private void SwitchTab(string tab)
    {
        activeTab = tab;
        bool isUpd = tab == "updates";
        updatesPane.Visibility = isUpd ? Visibility.Visible : Visibility.Collapsed;
        communityPane.Visibility = isUpd ? Visibility.Collapsed : Visibility.Visible;
        StyleNavTab(tabUpdatesBtn, isUpd);
        StyleNavTab(tabCommunityBtn, !isUpd);
    }

    private void StyleNavTab(Button btn, bool active)
    {
        btn.Background = active ? theme.Brush("background") : new SolidColorBrush(Colors.Transparent);
        btn.Foreground = active ? theme.Brush("accent") : theme.Brush("text_secondary");
        btn.BorderBrush = active ? theme.Brush("border") : new SolidColorBrush(Colors.Transparent);
        btn.BorderThickness = new Thickness(1);
    }

    private UIElement BuildUpdatesTab()
    {
        var stack = new StackPanel { Spacing = 14 };

        // Section heading
        var head = new StackPanel { Spacing = 2 };
        head.Children.Add(new TextBlock
        {
            Text = "更新与诊断",
            FontSize = 18,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text")
        });
        head.Children.Add(new TextBlock
        {
            Text = "应用内自更新、下载加速线路、测试通道与本地日志诊断",
            FontSize = 12,
            Foreground = theme.Brush("text_secondary")
        });
        stack.Children.Add(head);

        // 1. Brand & Updater Hero Card
        var heroContent = new StackPanel { Spacing = 12 };
        var heroRow = new Grid();
        heroRow.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        heroRow.ColumnDefinitions.Add(new() { Width = GridLength.Auto });

        var heroLeft = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 12, VerticalAlignment = VerticalAlignment.Center };
        var iconBox = new Border
        {
            Width = 42,
            Height = 42,
            CornerRadius = new CornerRadius(11),
            Background = new SolidColorBrush(ColorHelper.FromArgb(24, 37, 99, 235)),
            BorderBrush = new SolidColorBrush(ColorHelper.FromArgb(48, 37, 99, 235)),
            BorderThickness = new Thickness(1),
            Child = new FontIcon { Glyph = "\uE71B", FontSize = 20, Foreground = theme.Brush("accent") }
        };
        heroLeft.Children.Add(iconBox);

        var titleCol = new StackPanel { Spacing = 3, VerticalAlignment = VerticalAlignment.Center };
        var titleLine = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        titleLine.Children.Add(new TextBlock
        {
            Text = "Crossflow",
            FontSize = 16,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text")
        });
        titleLine.Children.Add(MakePill($"v{AppUpdater.CurrentVersion}", theme.Brush("surface"), theme.Brush("text_secondary")));

        statusPillText.FontSize = 11;
        statusPillText.FontWeight = Microsoft.UI.Text.FontWeights.SemiBold;
        statusPillBorder.CornerRadius = new CornerRadius(10);
        statusPillBorder.Padding = new Thickness(8, 2, 8, 2);
        statusPillBorder.VerticalAlignment = VerticalAlignment.Center;
        statusPillBorder.Child = statusPillText;
        titleLine.Children.Add(statusPillBorder);
        titleCol.Children.Add(titleLine);

        titleCol.Children.Add(new TextBlock
        {
            Text = "跨平台低延迟键鼠共享、剪贴板与文件直传 · 保留配置原地安全升级",
            FontSize = 12,
            Foreground = theme.Brush("text_secondary"),
            TextWrapping = TextWrapping.Wrap
        });
        heroLeft.Children.Add(titleCol);
        heroRow.Children.Add(heroLeft);

        var heroActions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, VerticalAlignment = VerticalAlignment.Center };
        StyleSecondaryButton(checkBtn, "立即检查更新");
        checkBtn.Click += async (_, _) => await updater.CheckForUpdatesAsync(silent: false);

        StylePrimaryButton(applyBtn, "一键更新");
        applyBtn.Visibility = Visibility.Collapsed;
        applyBtn.Click += async (_, _) => await onApplyUpdate();

        StyleSecondaryButton(notesBtn, "更新日志 ↗");
        notesBtn.Visibility = Visibility.Collapsed;
        notesBtn.Click += (_, _) => OpenUrl(updater.State.Release?.HtmlUrl ?? AppUpdater.ReleasesPageUrl);

        StyleSecondaryButton(cancelBtn, "取消");
        cancelBtn.Visibility = Visibility.Collapsed;
        cancelBtn.Click += (_, _) => updater.CancelDownload();

        heroActions.Children.Add(checkBtn);
        heroActions.Children.Add(applyBtn);
        heroActions.Children.Add(notesBtn);
        heroActions.Children.Add(cancelBtn);
        Grid.SetColumn(heroActions, 1);
        heroRow.Children.Add(heroActions);
        heroContent.Children.Add(heroRow);

        updateMessageText.FontSize = 12;
        heroContent.Children.Add(updateMessageText);
        heroContent.Children.Add(updateProgressBar);

        notesBodyText.FontSize = 11.5;
        notesBodyText.Foreground = theme.Brush("text_secondary");
        notesCard.Background = theme.Brush("surface");
        notesCard.BorderBrush = theme.Brush("border");
        notesCard.BorderThickness = new Thickness(1);
        notesCard.CornerRadius = new CornerRadius(10);
        notesCard.Padding = new Thickness(12, 10, 12, 10);
        notesCard.Child = notesBodyText;
        heroContent.Children.Add(notesCard);

        // Mirror + Update Channel + Feedback settings rows inside Hero card
        var settingsGroup = new StackPanel { Spacing = 8, Margin = new Thickness(0, 4, 0, 0) };

        // Row A: Download Mirror
        foreach (var preset in AppUpdater.MirrorPresets)
            mirrorCombo.Items.Add(preset.Label);
        mirrorCombo.FontSize = 12;
        mirrorCombo.SelectionChanged += (_, _) =>
        {
            if (updatingControls) return;
            int idx = mirrorCombo.SelectedIndex;
            if (idx < 0 || idx >= AppUpdater.MirrorPresets.Count) return;
            var preset = AppUpdater.MirrorPresets[idx];
            if (preset.Prefix is null)
            {
                customMirrorRow.Visibility = Visibility.Visible;
                customMirrorInput.Text = updater.MirrorPrefix;
                customMirrorInput.Focus(FocusState.Programmatic);
            }
            else
            {
                customMirrorRow.Visibility = Visibility.Collapsed;
                customMirrorError.Visibility = Visibility.Collapsed;
                updater.SetMirrorPrefix(preset.Prefix, out _);
                onNotify($"下载加速线路已切换为：{preset.Label}", false);
            }
        };
        settingsGroup.Children.Add(SettingRow(
            "下载加速线路",
            "用于检查更新与下载安装包，镜像失败或校验异常时自动回退 GitHub 官方直连",
            mirrorCombo));

        var saveMirrorBtn = new Button();
        StylePrimaryButton(saveMirrorBtn, "保存前缀");
        saveMirrorBtn.Click += (_, _) =>
        {
            if (updater.SetMirrorPrefix(customMirrorInput.Text, out string? err))
            {
                customMirrorError.Visibility = Visibility.Collapsed;
                onNotify("自定义镜像前缀已保存。", false);
            }
            else
            {
                customMirrorError.Text = err ?? "无效的镜像地址";
                customMirrorError.Foreground = new SolidColorBrush(ColorHelper.FromArgb(255, 220, 38, 38));
                customMirrorError.Visibility = Visibility.Visible;
            }
        };
        var cancelMirrorBtn = new Button();
        StyleSecondaryButton(cancelMirrorBtn, "取消");
        cancelMirrorBtn.Click += (_, _) =>
        {
            customMirrorError.Visibility = Visibility.Collapsed;
            RefreshUpdaterState(updater.State);
            if (AppUpdater.ResolveMirrorPresetId(updater.MirrorPrefix) != "custom")
                customMirrorRow.Visibility = Visibility.Collapsed;
        };
        customMirrorInput.FontSize = 12;
        customMirrorRow.Children.Add(customMirrorInput);
        customMirrorRow.Children.Add(saveMirrorBtn);
        customMirrorRow.Children.Add(cancelMirrorBtn);
        settingsGroup.Children.Add(customMirrorRow);
        settingsGroup.Children.Add(customMirrorError);

        // Row B: Update Channel (Stable vs Prerelease -rc)
        channelStableBtn.Content = "仅正式版";
        channelPreBtn.Content = "含测试版 (-rc)";
        channelStableBtn.FontSize = 12;
        channelPreBtn.FontSize = 12;
        channelStableBtn.Padding = new Thickness(12, 5, 12, 5);
        channelPreBtn.Padding = new Thickness(12, 5, 12, 5);
        channelStableBtn.CornerRadius = new CornerRadius(7);
        channelPreBtn.CornerRadius = new CornerRadius(7);
        channelStableBtn.Click += async (_, _) =>
        {
            updater.SetIncludePrerelease(false);
            await updater.CheckForUpdatesAsync(silent: false);
        };
        channelPreBtn.Click += async (_, _) =>
        {
            updater.SetIncludePrerelease(true);
            await updater.CheckForUpdatesAsync(silent: false);
        };
        var segWrap = new Border
        {
            Background = theme.Brush("surface"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(9),
            Padding = new Thickness(3),
            Child = new StackPanel
            {
                Orientation = Orientation.Horizontal,
                Spacing = 4,
                Children = { channelStableBtn, channelPreBtn }
            }
        };
        settingsGroup.Children.Add(SettingRow(
            "更新通道",
            "正式版稳定可靠；测试版 (-rc) 可抢先体验最新跨屏特性与修复",
            segWrap));

        // Row C: Feedback (GitHub Issues)
        var feedbackBtn = new Button();
        StyleSecondaryButton(feedbackBtn, "复制诊断并反馈 ↗");
        feedbackBtn.Click += (_, _) =>
        {
            CopyDiagnosticSummary();
            OpenUrl(AppUpdater.IssuesNewUrl);
            onNotify("已复制脱敏诊断摘要并打开 GitHub Issues。", false);
        };
        settingsGroup.Children.Add(SettingRow(
            "反馈问题（GitHub Issues）",
            "自动复制脱敏环境摘要（不含密码与剪贴板内容）并前往 GitHub 提交工单",
            feedbackBtn));

        heroContent.Children.Add(settingsGroup);
        stack.Children.Add(Card(heroContent));

        // 2. Diagnostics & Local Directories Card
        var diagCard = new StackPanel { Spacing = 10 };
        var diagHead = new Grid();
        diagHead.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        diagHead.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        diagHead.Children.Add(new TextBlock
        {
            Text = "系统诊断与本地目录",
            FontSize = 13.5,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text"),
            VerticalAlignment = VerticalAlignment.Center
        });
        var copyDiagBtn = new Button();
        StylePrimaryButton(copyDiagBtn, "一键复制完整诊断信息");
        copyDiagBtn.Click += (_, _) =>
        {
            CopyDiagnosticSummary();
            onNotify("已复制 Crossflow 脱敏诊断摘要到剪贴板。", false);
        };
        Grid.SetColumn(copyDiagBtn, 1);
        diagHead.Children.Add(copyDiagBtn);
        diagCard.Children.Add(diagHead);

        // 2x2 runtime info grid
        var grid = new Grid { ColumnSpacing = 10, RowSpacing = 10 };
        grid.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        grid.RowDefinitions.Add(new() { Height = GridLength.Auto });
        grid.RowDefinitions.Add(new() { Height = GridLength.Auto });
        AddDiagCell(grid, 0, 0, "当前版本与通道", diagVersionVal);
        AddDiagCell(grid, 0, 1, "下载加速线路", diagMirrorVal);
        AddDiagCell(grid, 1, 0, "网络与对端状态", diagConnVal);
        AddDiagCell(grid, 1, 1, "Windows 输入模式", diagServiceVal);
        diagCard.Children.Add(grid);

        var openLogsBtn = new Button();
        StyleSecondaryButton(openLogsBtn, "打开目录");
        openLogsBtn.Click += (_, _) => OpenFolder(AppUpdater.LogsDir);
        diagCard.Children.Add(SettingRow("运行日志目录", AppUpdater.LogsDir, openLogsBtn));

        var openDataBtn = new Button();
        StyleSecondaryButton(openDataBtn, "打开目录");
        openDataBtn.Click += (_, _) => OpenFolder(AppUpdater.UserDataDir);
        diagCard.Children.Add(SettingRow("用户配置目录", AppUpdater.UserDataDir, openDataBtn));

        stack.Children.Add(Card(diagCard));
        return stack;
    }

    private UIElement BuildCommunityTab()
    {
        var stack = new StackPanel { Spacing = 14 };

        var head = new StackPanel { Spacing = 2 };
        head.Children.Add(new TextBlock
        {
            Text = "社区与支持",
            FontSize = 18,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text")
        });
        head.Children.Add(new TextBlock
        {
            Text = "关注开源项目进展、参与问题讨论与查看实机演示视频",
            FontSize = 12,
            Foreground = theme.Brush("text_secondary")
        });
        stack.Children.Add(head);

        // Community Hero Card (adapted from HaloDeck)
        var hero = new StackPanel { Spacing = 10 };
        var heroTop = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 14 };
        heroTop.Children.Add(new Border
        {
            Width = 46,
            Height = 46,
            CornerRadius = new CornerRadius(12),
            Background = new SolidColorBrush(ColorHelper.FromArgb(24, 37, 99, 235)),
            BorderBrush = new SolidColorBrush(ColorHelper.FromArgb(48, 37, 99, 235)),
            BorderThickness = new Thickness(1),
            VerticalAlignment = VerticalAlignment.Top,
            Child = new FontIcon { Glyph = "\uE71B", FontSize = 22, Foreground = theme.Brush("accent") }
        });

        var copyCol = new StackPanel { Spacing = 6, MaxWidth = 580 };
        copyCol.Children.Add(new TextBlock
        {
            Text = "开源 · 免费 · 专为多机桌面协同而生",
            FontSize = 15,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text")
        });
        copyCol.Children.Add(new TextBlock
        {
            Text = "Crossflow 遵循 MIT 开源协议持续迭代，所有跨屏键鼠共享、Noise 端到端加密、剪贴板图片与文件直传及双端原生客户端均永久免费开放。欢迎在 GitHub 点亮 Star、提交反馈或共同参与贡献。",
            FontSize = 12.5,
            Foreground = theme.Brush("text_secondary"),
            TextWrapping = TextWrapping.Wrap
        });
        var pills = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 2, 0, 0) };
        pills.Children.Add(MakePill("MIT 开源协议", new SolidColorBrush(ColorHelper.FromArgb(26, 22, 163, 74)), new SolidColorBrush(ColorHelper.FromArgb(255, 21, 128, 61))));
        pills.Children.Add(MakePill("永久免费 · 无广告", theme.Brush("surface"), theme.Brush("text_secondary")));
        pills.Children.Add(MakePill("本地直连 · 零遥测", theme.Brush("surface"), theme.Brush("text_secondary")));
        copyCol.Children.Add(pills);
        heroTop.Children.Add(copyCol);
        hero.Children.Add(heroTop);
        stack.Children.Add(Card(hero));

        // 3-Column Social / Community Cards (1:1 with HaloDeck)
        var cardsGrid = new Grid { ColumnSpacing = 12 };
        cardsGrid.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        cardsGrid.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        cardsGrid.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });

        var card1 = BuildSocialCard(
            glyph: "\uE943",
            accentColor: ColorHelper.FromArgb(255, 15, 23, 42),
            cta: "前往 Star ↗",
            title: "Crossflow 项目仓库",
            desc: "浏览源码、点亮 Star、参与 Issue 反馈与 PR 贡献",
            urlLabel: "github.com/Lokilankaaa/crossflow",
            url: AppUpdater.RepoBaseUrl);
        Grid.SetColumn(card1, 0);
        cardsGrid.Children.Add(card1);

        var card2 = BuildSocialCard(
            glyph: "\uE77B",
            accentColor: ColorHelper.FromArgb(255, 37, 99, 235),
            cta: "关注主页 ↗",
            title: "作者 GitHub 主页",
            desc: "关注 Lokilankaaa 开源动态、HaloDeck 与新工具发布",
            urlLabel: "github.com/Lokilankaaa",
            url: AppUpdater.AuthorGitHubUrl);
        Grid.SetColumn(card2, 1);
        cardsGrid.Children.Add(card2);

        var card3 = BuildSocialCard(
            glyph: "\uE714",
            accentColor: ColorHelper.FromArgb(255, 251, 114, 153),
            cta: "前往 B 站 ↗",
            title: "作者哔哩哔哩主页",
            desc: "嗷嗷嗷嗷的猫神 · 版本演示、跨屏实战与折腾分享",
            urlLabel: "b23.tv/pANRtSc",
            url: AppUpdater.AuthorBilibiliUrl);
        Grid.SetColumn(card3, 2);
        cardsGrid.Children.Add(card3);

        stack.Children.Add(cardsGrid);
        return stack;
    }

    private Button BuildSocialCard(
        string glyph,
        global::Windows.UI.Color accentColor,
        string cta,
        string title,
        string desc,
        string urlLabel,
        string url)
    {
        var content = new Grid { RowSpacing = 12 };
        content.RowDefinitions.Add(new() { Height = GridLength.Auto });
        content.RowDefinitions.Add(new() { Height = new GridLength(1, GridUnitType.Star) });
        content.RowDefinitions.Add(new() { Height = GridLength.Auto });

        var topRow = new Grid();
        topRow.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        topRow.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        topRow.ColumnDefinitions.Add(new() { Width = GridLength.Auto });

        var iconWrap = new Border
        {
            Width = 36,
            Height = 36,
            CornerRadius = new CornerRadius(10),
            Background = new SolidColorBrush(ColorHelper.FromArgb(26, accentColor.R, accentColor.G, accentColor.B)),
            Child = new FontIcon { Glyph = glyph, FontSize = 16, Foreground = new SolidColorBrush(accentColor) }
        };
        topRow.Children.Add(iconWrap);

        var ctaText = new TextBlock
        {
            Text = cta,
            FontSize = 11.5,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("accent"),
            VerticalAlignment = VerticalAlignment.Center
        };
        Grid.SetColumn(ctaText, 2);
        topRow.Children.Add(ctaText);
        content.Children.Add(topRow);

        var copyStack = new StackPanel { Spacing = 4 };
        copyStack.Children.Add(new TextBlock
        {
            Text = title,
            FontSize = 13.5,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text"),
            TextWrapping = TextWrapping.Wrap
        });
        copyStack.Children.Add(new TextBlock
        {
            Text = desc,
            FontSize = 11.5,
            Foreground = theme.Brush("text_secondary"),
            TextWrapping = TextWrapping.Wrap
        });
        Grid.SetRow(copyStack, 1);
        content.Children.Add(copyStack);

        var urlText = new TextBlock
        {
            Text = urlLabel,
            FontSize = 11,
            Foreground = theme.Brush("text_secondary"),
            TextTrimming = TextTrimming.CharacterEllipsis
        };
        Grid.SetRow(urlText, 2);
        content.Children.Add(urlText);

        var btn = new Button
        {
            Content = content,
            HorizontalAlignment = HorizontalAlignment.Stretch,
            VerticalAlignment = VerticalAlignment.Stretch,
            HorizontalContentAlignment = HorizontalAlignment.Stretch,
            VerticalContentAlignment = VerticalAlignment.Stretch,
            Background = theme.Brush("surface"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(12),
            Padding = new Thickness(14),
            MinHeight = 156
        };
        btn.Click += (_, _) => OpenUrl(url);
        return btn;
    }

    private Border Card(UIElement child) => new()
    {
        Child = child,
        Background = theme.Brush("background"),
        BorderBrush = theme.Brush("border"),
        BorderThickness = new Thickness(1),
        CornerRadius = new CornerRadius(12),
        Padding = new Thickness(16)
    };

    private Border SettingRow(string title, string subtitle, FrameworkElement control)
    {
        var grid = new Grid { ColumnSpacing = 12 };
        grid.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new() { Width = GridLength.Auto });

        var copy = new StackPanel { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
        copy.Children.Add(new TextBlock
        {
            Text = title,
            FontSize = 12.5,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            Foreground = theme.Brush("text")
        });
        copy.Children.Add(new TextBlock
        {
            Text = subtitle,
            FontSize = 11,
            Foreground = theme.Brush("text_secondary"),
            TextWrapping = TextWrapping.Wrap
        });
        grid.Children.Add(copy);

        control.VerticalAlignment = VerticalAlignment.Center;
        Grid.SetColumn(control, 1);
        grid.Children.Add(control);

        return new Border
        {
            Child = grid,
            Background = theme.Brush("surface"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(10),
            Padding = new Thickness(12, 9, 12, 9)
        };
    }

    private void AddDiagCell(Grid grid, int row, int col, string label, TextBlock valueBlock)
    {
        valueBlock.FontSize = 12;
        valueBlock.FontWeight = Microsoft.UI.Text.FontWeights.SemiBold;
        valueBlock.Foreground = theme.Brush("text");
        valueBlock.TextWrapping = TextWrapping.Wrap;

        var cell = new Border
        {
            Background = theme.Brush("surface"),
            BorderBrush = theme.Brush("border"),
            BorderThickness = new Thickness(1),
            CornerRadius = new CornerRadius(9),
            Padding = new Thickness(12, 8, 12, 8),
            Child = new StackPanel
            {
                Spacing = 2,
                Children =
                {
                    new TextBlock { Text = label, FontSize = 11, Foreground = theme.Brush("text_secondary") },
                    valueBlock
                }
            }
        };
        Grid.SetRow(cell, row);
        Grid.SetColumn(cell, col);
        grid.Children.Add(cell);
    }

    private Border MakePill(string text, Brush bg, Brush fg) => new()
    {
        Background = bg,
        CornerRadius = new CornerRadius(10),
        Padding = new Thickness(8, 2, 8, 2),
        VerticalAlignment = VerticalAlignment.Center,
        Child = new TextBlock { Text = text, FontSize = 11, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, Foreground = fg }
    };

    private void StylePrimaryButton(Button button, string text)
    {
        button.Content = text;
        button.FontSize = 12;
        button.Background = theme.Brush("accent");
        button.Foreground = new SolidColorBrush(Colors.White);
        button.BorderThickness = new Thickness(0);
        button.CornerRadius = new CornerRadius(8);
        button.Padding = new Thickness(12, 6, 12, 6);
    }

    private void StyleSecondaryButton(Button button, string text)
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

    private void StyleSegment(Button button, bool active)
    {
        button.Background = active ? theme.Brush("accent") : new SolidColorBrush(Colors.Transparent);
        button.Foreground = active ? new SolidColorBrush(Colors.White) : theme.Brush("text_secondary");
        button.BorderThickness = new Thickness(0);
    }

    private void CopyDiagnosticSummary()
    {
        var snap = getSnapshot();
        var st = updater.State;
        int connected = snap.Peers.Count(p => p.Connected);
        string summary = string.Join("\n",
        [
            "### Crossflow 环境诊断摘要",
            $"- 客户端版本: v{st.CurrentVersion} (Windows {Environment.OSVersion.Version})",
            $"- 更新通道: {(st.IncludePrerelease ? "含测试版 (-rc)" : "仅正式版")}",
            $"- 下载加速线路: {AppUpdater.DescribeMirror(st.MirrorPrefix)}",
            $"- 连接状态: {snap.State} (已连接对端: {connected})",
            $"- 当前信道: {snap.ActiveChannelLabel ?? snap.SelectedChannel}",
            $"- 共享开关: 剪贴板={(snap.ClipboardSync ? "开" : "关")} · 输入法同步={(snap.InputStateSync ? "开" : "关")} · 系统服务={(snap.ServiceModeActive ? "运行中" : snap.ServiceModeInstalled ? "已安装" : "未安装")}"
        ]);
        try
        {
            var pkg = new DataPackage();
            pkg.SetText(summary);
            Clipboard.SetContent(pkg);
            Clipboard.Flush();
        }
        catch { }
    }

    private static void OpenUrl(string url)
    {
        if (string.IsNullOrWhiteSpace(url)) return;
        try { Process.Start(new ProcessStartInfo(url) { UseShellExecute = true }); } catch { }
    }

    private static void OpenFolder(string dir)
    {
        try
        {
            Directory.CreateDirectory(dir);
            Process.Start(new ProcessStartInfo("explorer.exe", $"\"{dir}\"") { UseShellExecute = true });
        }
        catch { }
    }
}
