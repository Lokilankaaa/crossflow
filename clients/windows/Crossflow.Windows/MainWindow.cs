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
        Title = "Crossflow";
        root.RequestedTheme = ElementTheme.Light;
        root.Background = theme.Brush("background");
        root.RowDefinitions.Add(new() { Height = GridLength.Auto });
        root.RowDefinitions.Add(new() { Height = new GridLength(1, GridUnitType.Star) });
        root.ColumnDefinitions.Add(new() { Width = new GridLength(theme.Number("window", "sidebar_width")) });
        root.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        sidebar = new StackPanel { Spacing = theme.Number("spacing", "medium"), Padding = new Thickness(20, 28, 20, 20), Background = theme.Brush("surface") };
        sidebar.Children.Add(Text("Crossflow", "section"));
        sidebar.Children.Add(Text("跨设备，自然流动", "caption", true));
        sidebar.Children.Add(new Border { Height = 24 });
        sidebar.Children.Add(Action(theme.Copy("layout_title"), "nav-layout", () => ShowLayout()));
        sidebar.Children.Add(Action(theme.Copy("input_title"), "nav-input", () => ShowInput()));
        sidebar.Children.Add(Action(theme.Copy("connection_title"), "nav-connection", () => ShowConnection()));
        Grid.SetRowSpan(sidebar, 2); root.Children.Add(sidebar);
        compactNavigation.Orientation = Orientation.Horizontal;
        compactNavigation.Spacing = theme.Number("spacing", "large");
        compactNavigation.Padding = new Thickness(16, 12, 16, 12);
        compactNavigation.Background = theme.Brush("surface");
        compactNavigation.Visibility = Visibility.Collapsed;
        var menu = new MenuFlyout();
        foreach (var (title, id, open) in new (string, string, Action)[] {
            (theme.Copy("layout_title"), "compact-nav-layout", ShowLayout),
            (theme.Copy("input_title"), "compact-nav-input", ShowInput),
            (theme.Copy("connection_title"), "compact-nav-connection", ShowConnection) })
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
        canvas.Background = theme.Brush("surface_subtle");
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
    }

    private TextBlock Text(string value, string size = "body", bool secondary = false) => new() { Text = value, FontSize = theme.Number("typography", size), Foreground = theme.Brush(secondary ? "text_secondary" : "text"), TextWrapping = TextWrapping.Wrap };
    private Button Action(string title, string id, Action run)
    {
        var button = new Button { Content = title, HorizontalAlignment = HorizontalAlignment.Stretch, FontSize = theme.Number("typography", "body") };
        AutomationProperties.SetAutomationId(button, id);
        button.Click += (_, _) => { if (!busy) run(); };
        return button;
    }
    private StackPanel Page(string title, string subtitle)
    {
        connectionStatus = null;
        editorGrid = null; screenBoard = null; propertiesPanel = null; propertiesScroll = null; layoutToolbar = null; toolbarScroll = null;
        workspace.Children.Clear(); workspace.RowDefinitions.Clear();
        var page = new StackPanel { Spacing = theme.Number("spacing", "large") };
        page.Children.Add(Text(title, "title")); page.Children.Add(Text(subtitle, secondary: true));
        workspace.Children.Add(page); return page;
    }
    private void ShowConnection()
    {
        var page = Page(theme.Copy("connection_title"), theme.Copy("session_unavailable"));
        connectionStatus = Text(coreReady ? theme.Copy("core_ready") : theme.Copy("core_unavailable"), "section");
        page.Children.Add(connectionStatus);
        page.Children.Add(Text("此客户端目前用于编辑屏幕布局。设备发现、连接控制与实时状态将在接入共享会话后显示。", secondary: true));
    }
    private void ShowInput()
    {
        var page = Page(theme.Copy("input_title"), "输入设置尚未接入此客户端。");
        page.Children.Add(new ToggleSwitch { Header = "Ctrl / Cmd 映射", IsEnabled = false });
        page.Children.Add(new ToggleSwitch { Header = "边缘阻尼", IsEnabled = false });
        page.Children.Add(Text("打开此窗口不会启动键鼠共享，也不会改变系统输入。", secondary: true));
    }
    private void ShowLayout()
    {
        connectionStatus = null;
        if (canvas.Parent is Border oldBoard) oldBoard.Child = null;
        if (inspector.Parent is ScrollViewer oldScroll) oldScroll.Content = null;
        if (export.Parent is Panel oldToolbar) oldToolbar.Children.Remove(export);
        workspace.Children.Clear(); workspace.RowDefinitions.Clear();
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
        var board = new Border { Background = theme.Brush("surface_subtle"), CornerRadius = new CornerRadius(theme.Number("radii", "panel")), Child = canvas };
        screenBoard = board;
        editor.Children.Add(board);
        inspector.Spacing = 12; inspector.Padding = new Thickness(16);
        var scroll = new ScrollViewer { Content = inspector, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        propertiesScroll = scroll;
        var properties = new Border { Child = scroll, Background = theme.Brush("surface"), CornerRadius = new CornerRadius(theme.Number("radii", "panel")) };
        propertiesPanel = properties;
        Grid.SetColumn(properties, 1); editor.Children.Add(properties);
        Grid.SetRow(editor, 3); workspace.Children.Add(editor);
        status.FontSize = theme.Number("typography", "caption"); status.Foreground = theme.Brush("text_secondary"); status.Margin = new Thickness(0, 16, 0, 0); status.TextWrapping = TextWrapping.Wrap;
        Grid.SetRow(status, 4); workspace.Children.Add(status);
        ApplyAdaptiveLayout(); UpdateStatus(); Draw(); Inspect();
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
        compactNavigation.Visibility = compact ? Visibility.Visible : Visibility.Collapsed;
        workspace.Margin = new Thickness(theme.Number("spacing", compact ? "large" : "section"));
        workspaceScroll.VerticalScrollBarVisibility = compact ? ScrollBarVisibility.Auto : ScrollBarVisibility.Disabled;
        workspaceScroll.VerticalScrollMode = compact ? ScrollMode.Enabled : ScrollMode.Disabled;
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
        if (connectionStatus is not null)
            connectionStatus.Text = coreReady ? theme.Copy("core_ready") : theme.Copy("core_unavailable");
        export.IsEnabled = coreReady && !busy && layout.Screens.Count > 0;
        status.Text = $"{(coreReady ? theme.Copy("core_ready") : theme.Copy("core_unavailable"))}  ·  {theme.Copy("session_unavailable")}";
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
        finally { busy = false; export.IsEnabled = coreReady && layout.Screens.Count > 0; Draw(); Inspect(); }
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
            var label = new StackPanel { Spacing = 6, VerticalAlignment = VerticalAlignment.Center };
            label.Children.Add(Text(screen.Id, "section")); label.Children.Add(Text(screen.DeviceId, "caption", true));
            label.Children.Add(Text($"{screen.Width:g} × {screen.Height:g}", "caption", true));
            var button = new Button { Content = label, Padding = new Thickness(12), HorizontalContentAlignment = HorizontalAlignment.Center, VerticalContentAlignment = VerticalAlignment.Center, Background = theme.Brush("surface"), BorderThickness = new Thickness(0) };
            AutomationProperties.SetName(button, $"屏幕 {screen.Id}，设备 {screen.DeviceId}。使用方向键移动。");
            AutomationProperties.SetAutomationId(button, "screen-" + screen.Id);
            var tile = new Border { Width = screen.Width * scale, Height = screen.Height * scale, Child = button, CornerRadius = new CornerRadius(theme.Number("radii", "control")), BorderThickness = new Thickness(selected == screen.Id ? 3 : 1), BorderBrush = theme.Brush(devices.IndexOf(screen.DeviceId) % 2 == 0 ? "accent" : "device_secondary") };
            Canvas.SetLeft(tile, screen.X * scale + offsetX); Canvas.SetTop(tile, screen.Y * scale + offsetY); canvas.Children.Add(tile);
            button.AddHandler(UIElement.PointerPressedEvent, new PointerEventHandler((_, e) =>
            {
                if (busy || !coreReady || !e.GetCurrentPoint(canvas).Properties.IsLeftButtonPressed) return;
                selected = screen.Id; Inspect(); button.Focus(FocusState.Pointer);
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
        inspector.Children.Clear(); inspector.Children.Add(Text("屏幕属性", "section"));
        var screen = layout.Screens.FirstOrDefault(s => s.Id == selected);
        if (screen is null) { inspector.Children.Add(Text("选择一块屏幕，编辑位置与尺寸。", secondary: true)); return; }
        var id = new TextBox { Header = "屏幕名称", Text = screen.Id };
        var device = new TextBox { Header = "所属设备", Text = screen.DeviceId };
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
            if (candidate.Screens.Count == 0) { layout = candidate; selected = null; sample = false; ++revision; UpdateStatus(); Draw(); Inspect(); }
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
        var device = new TextBox { Header = "所属设备", Text = "我的设备" };
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
