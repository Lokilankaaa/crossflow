using System.Runtime.InteropServices;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Crossflow.Windows;

internal static class Program
{
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr FindWindow(string? lpClassName, string lpWindowName);

    [DllImport("user32.dll")]
    private static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);

    [DllImport("user32.dll")]
    private static extern bool SetForegroundWindow(IntPtr hWnd);

    private static Mutex? singleInstanceMutex;
    internal static bool StartMinimized { get; private set; }

    [STAThread]
    private static void Main(string[] args)
    {
        StartMinimized = args.Any(a => string.Equals(a, "--minimized", StringComparison.OrdinalIgnoreCase));
        if (args.Length == 2 && args[0] == "--oneclick-fixture") { OneClickSelfTest.FixtureAsync(args[1]).GetAwaiter().GetResult(); return; }
        if (args.Length == 2 && args[0] == "--oneclick-self-test") { OneClickSelfTest.RunAsync(args[1]).GetAwaiter().GetResult(); return; }
        if (args.Length == 2 && args[0] == "--runtime-layout-self-test") { RuntimeLayoutSelfTest.RunAsync(args[1]).GetAwaiter().GetResult(); return; }
        if (args.Length == 2 && args[0] == "--peer-fixture") { PeerSelfTest.FixtureAsync(args[1]).GetAwaiter().GetResult(); return; }
        if (args.Length == 2 && args[0] == "--peer-self-test") { PeerSelfTest.RunAsync(args[1]).GetAwaiter().GetResult(); return; }
        if (args.Length == 2 && args[0] == "--discovery-fixture") { DiscoverySelfTest.FixtureAsync(args[1]).GetAwaiter().GetResult(); return; }
        if (args.Length == 2 && args[0] == "--discovery-self-test") { DiscoverySelfTest.RunAsync(args[1]).GetAwaiter().GetResult(); return; }
        if (args.Length == 2 && args[0] == "--session-fixture") { SessionSelfTest.FixtureAsync(args[1]).GetAwaiter().GetResult(); return; }
        if (args.Length == 2 && args[0] == "--session-self-test") { SessionSelfTest.RunAsync(args[1]).GetAwaiter().GetResult(); return; }
        if (args.Length == 2 && args[0] == "--self-test") { SelfTest.Run(args[1]); return; }
        singleInstanceMutex = new Mutex(true, @"Local\Crossflow.Windows.SingleInstance", out bool createdNew);
        if (!createdNew)
        {
            var existing = FindWindow(null, "Crossflow");
            if (existing != IntPtr.Zero)
            {
                ShowWindow(existing, 5); // SW_SHOW (unhide if minimized to tray)
                ShowWindow(existing, 9); // SW_RESTORE
                SetForegroundWindow(existing);
            }
            return;
        }
        StartupDiagnostics.Write("startup-enter");
        AppDomain.CurrentDomain.UnhandledException += (_, failure) => StartupDiagnostics.Write("domain-unhandled", failure.ExceptionObject as Exception);
        try
        {
            WinRT.ComWrappersSupport.InitializeComWrappers();
            StartupDiagnostics.Write("com-wrappers-ready");
            Application.Start(parameters =>
            {
                try
                {
                    StartupDiagnostics.Write("xaml-start-callback");
                    SynchronizationContext.SetSynchronizationContext(new DispatcherQueueSynchronizationContext(DispatcherQueue.GetForCurrentThread()));
                    _ = new CrossflowApplication();
                }
                catch (Exception failure) { StartupDiagnostics.Write("xaml-start-failed", failure); throw; }
            });
            StartupDiagnostics.Write("application-exited");
        }
        catch (Exception failure) { StartupDiagnostics.Write("startup-failed", failure); Environment.ExitCode = 1; }
    }
}

public sealed partial class CrossflowApplication : Application
{
    private Window? window;
    public CrossflowApplication()
    {
        UnhandledException += (_, failure) => StartupDiagnostics.Write("xaml-unhandled", failure.Exception);
        StartupDiagnostics.Write("application-created");
        InitializeComponent();
        StartupDiagnostics.Write("xaml-resources-ready");
    }
    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        StartupDiagnostics.Write("on-launched");
        try { window = new MainWindow(new Theme()); StartupDiagnostics.Write("main-window-created"); }
        catch (Exception failure)
        {
            StartupDiagnostics.Write("main-window-failed", failure);
            window = new Window { Title = "Crossflow", Content = new TextBlock { Text = "界面未能加载。启动原因已写入：\n" + StartupDiagnostics.LogPath, TextWrapping = TextWrapping.Wrap, Margin = new Thickness(32) } };
        }
        if (Program.StartMinimized && window is MainWindow main)
        {
            main.StartInTray();
            StartupDiagnostics.Write("window-started-in-tray");
        }
        else
        {
            window.Activate();
            StartupDiagnostics.Write("window-activated");
        }
    }
}

internal static class SelfTest
{
    internal static void Run(string path)
    {
        var checks = new Dictionary<string, bool>();
        try
        {
            var theme = new Theme();
            checks["shared_tokens"] = theme.Copy("layout_title") == "屏幕排列" && theme.Number("window", "width") > 0;
            var env = CoreBridge.Request(new { op = "environment" });
            checks["environment"] = env.GetProperty("session_status").GetString() == "not_exposed";
            var layout = CoreBridge.Parse(File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "sample-layout.json")));
            checks["valid_layout"] = layout.Screens.Count >= 2;
            checks["multiple_screens_per_device"] = layout.Screens.GroupBy(screen => screen.DeviceId).Count(group => group.Count() >= 2) >= 2;
            var first = layout.Screens[0];
            var overlapping = new LayoutDocument([first, layout.Screens[1] with { X = first.X, Y = first.Y }]);
            try { CoreBridge.Validate(overlapping); checks["overlap_rejected"] = false; }
            catch (BridgeValidationException error) when (error.Code == "invalid_layout") { checks["overlap_rejected"] = true; }
            string? crossingTarget = CoreBridge.Crossing(layout, first, "右").GetProperty("crossing").GetProperty("target_screen_id").GetString();
            checks["crossing"] = layout.Screens.Any(screen => screen.Id == crossingTarget && screen.Id != first.Id);
            var translated = new LayoutDocument(layout.Screens.Select(s => s with { X = s.X + 18014398509481984d }).ToList());
            checks["large_coordinate_crossing"] = CoreBridge.Crossing(translated, translated.Screens[0], "右").GetProperty("crossing").GetProperty("target_screen_id").GetString() == crossingTarget;
            checks["unknown_fields_rejected"] = Rejects(System.Text.Json.JsonSerializer.Serialize(new { screens = layout.Screens, extra = 1 }, CoreBridge.Json));
            File.WriteAllText(path, System.Text.Json.JsonSerializer.Serialize(new { ok = checks.Values.All(x => x), checks }, CoreBridge.Json));
            Environment.ExitCode = checks.Values.All(x => x) ? 0 : 1;
        }
        catch (Exception error)
        {
            File.WriteAllText(path, System.Text.Json.JsonSerializer.Serialize(new { ok = false, checks, error = error.Message }, CoreBridge.Json));
            Environment.ExitCode = 1;
        }
    }
    private static bool Rejects(string text) { try { CoreBridge.Parse(text); return false; } catch (BridgeValidationException error) when (error.Code == "invalid_layout") { return true; } }
}
