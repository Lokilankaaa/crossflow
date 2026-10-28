using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Crossflow.Windows;

internal static class Program
{
    [STAThread]
    private static void Main(string[] args)
    {
        if (args.Length == 2 && args[0] == "--self-test") { SelfTest.Run(args[1]); return; }
        WinRT.ComWrappersSupport.InitializeComWrappers();
        Application.Start(parameters =>
        {
            SynchronizationContext.SetSynchronizationContext(new DispatcherQueueSynchronizationContext(DispatcherQueue.GetForCurrentThread()));
            _ = new CrossflowApplication();
        });
    }
}

internal sealed class CrossflowApplication : Application
{
    private Window? window;
    public CrossflowApplication() => Resources.MergedDictionaries.Add(new XamlControlsResources());
    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        try { window = new MainWindow(new Theme()); }
        catch { window = new Window { Title = "Crossflow", Content = new TextBlock { Text = "界面资源未能加载，请重新构建客户端。", Margin = new Thickness(32) } }; }
        window.Activate();
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
            checks["valid_layout"] = layout.Screens.Count == 2;
            var first = layout.Screens[0];
            var overlapping = new LayoutDocument([first, layout.Screens[1] with { X = first.X, Y = first.Y }]);
            try { CoreBridge.Validate(overlapping); checks["overlap_rejected"] = false; }
            catch (BridgeValidationException error) when (error.Code == "invalid_layout") { checks["overlap_rejected"] = true; }
            checks["crossing"] = CoreBridge.Crossing(layout, first, "右").GetProperty("crossing").GetProperty("target_screen_id").GetString() == layout.Screens[1].Id;
            var translated = new LayoutDocument(layout.Screens.Select(s => s with { X = s.X + 18014398509481984d }).ToList());
            checks["large_coordinate_crossing"] = CoreBridge.Crossing(translated, translated.Screens[0], "右").GetProperty("crossing").GetProperty("target_screen_id").GetString() == translated.Screens[1].Id;
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
