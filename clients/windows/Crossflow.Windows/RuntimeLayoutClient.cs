using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Crossflow.Windows;

internal sealed record NativeDisplay(string Id, double X, double Y, double Width, double Height);
internal sealed record NativeRect(double X, double Y, double Width, double Height);
internal sealed record RuntimeScreenBinding(string ScreenId, string NativeId, NativeRect Native);
internal sealed record RuntimeLayoutDocument(LayoutDocument Layout, string LocalDeviceId, string RemoteDeviceId, List<RuntimeScreenBinding> Bindings);

/// Screen geometry comes from the local OS and the authenticated paired receiver.
internal sealed class DisplayClient
{
    private readonly BoundedCoreCommand<List<NativeDisplay>> command;
    internal DisplayClient(string? peerId = null) => command = new(peerId is null ? ["displays"] : ["peer-displays", "--peer", peerId], Parse, timeout: TimeSpan.FromSeconds(15));
    internal Task<List<NativeDisplay>> ReadAsync() => command.RunAsync();
    internal Task<bool> StopAsync() => command.StopAsync();

    internal static List<NativeDisplay> Parse(byte[] bytes)
    {
        try
        {
            if (bytes.Length > 262144) throw new JsonException();
            using var json = JsonDocument.Parse(new UTF8Encoding(false, true).GetString(bytes), new JsonDocumentOptions { MaxDepth = 4 });
            if (json.RootElement.ValueKind != JsonValueKind.Array || json.RootElement.GetArrayLength() is < 1 or > 64) throw new JsonException();
            var displays = new List<NativeDisplay>();
            var ids = new HashSet<string>(StringComparer.Ordinal);
            foreach (var item in json.RootElement.EnumerateArray())
            {
                string[] expected = ["id", "x", "y", "width", "height"];
                var fields = item.EnumerateObject().Select(x => x.Name).ToArray();
                if (fields.Length != 5 || fields.Distinct().Count() != 5 || expected.Except(fields).Any()) throw new JsonException();
                var display = item.Deserialize<NativeDisplay>(CoreBridge.Json) ?? throw new JsonException();
                if (string.IsNullOrWhiteSpace(display.Id) || Encoding.UTF8.GetByteCount(display.Id) > 128 || display.Id.Any(char.IsControl) || !ids.Add(display.Id)) throw new JsonException();
                ValidateRect(new(display.X, display.Y, display.Width, display.Height));
                displays.Add(display);
            }
            return displays;
        }
        catch (Exception error) when (error is JsonException or DecoderFallbackException or InvalidOperationException or ArgumentException)
        { throw new InvalidOperationException("屏幕信息无效。请更新两端客户端，再重新读取屏幕。", error); }
    }

    internal static void ValidateRect(NativeRect rect)
    {
        if (!new[] { rect.X, rect.Y, rect.Width, rect.Height }.All(double.IsFinite) || rect.Width < 1 || rect.Height < 1 ||
            Math.Abs(rect.X) > 1_000_000 || Math.Abs(rect.Y) > 1_000_000 || rect.Width > 1_000_000 || rect.Height > 1_000_000 ||
            rect.X + rect.Width <= rect.X || rect.Y + rect.Height <= rect.Y)
            throw new InvalidOperationException("屏幕尺寸无效。");
    }
}

internal static class RuntimeLayouts
{
    private static string ScreenId(string device, string native) => (device == "local" ? "local-" : "remote-") + Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(native))).ToLowerInvariant()[..24];
    internal static RuntimeLayoutDocument Create(string peerId, IReadOnlyList<NativeDisplay> local, IReadOnlyList<NativeDisplay> remote)
    {
        if (local.Count == 0 || remote.Count == 0) throw new InvalidOperationException("两端都需要至少一块可用屏幕。");
        var screens = new List<ScreenLayout>();
        var bindings = new List<RuntimeScreenBinding>();
        var localEdge = local.OrderByDescending(x => x.X + x.Width).ThenBy(x => x.Y).First();
        var remoteEdge = remote.OrderBy(x => x.X).ThenBy(x => x.Y).First();
        double remoteOffsetX = localEdge.X + localEdge.Width - remoteEdge.X;
        double remoteOffsetY = localEdge.Y - remoteEdge.Y;
        void Add(IEnumerable<NativeDisplay> displays, string device, double offsetX, double offsetY)
        {
            foreach (var display in displays)
            {
                var rect = new NativeRect(display.X, display.Y, display.Width, display.Height);
                DisplayClient.ValidateRect(rect);
                string id = ScreenId(device, display.Id);
                screens.Add(new(id, device, display.X + offsetX, display.Y + offsetY, display.Width, display.Height));
                bindings.Add(new(id, display.Id, rect));
            }
        }
        Add(local, "local", 0, 0); Add(remote, peerId, remoteOffsetX, remoteOffsetY);
        return new(new(screens), "local", peerId, bindings);
    }

    internal static RuntimeLayoutDocument WithLayout(RuntimeLayoutDocument source, LayoutDocument layout)
    {
        if (layout.Screens.Count != source.Bindings.Count || layout.Screens.Select(s => s.Id).Distinct().Count() != layout.Screens.Count)
            throw new InvalidOperationException("屏幕数量已改变，请重新读取两端屏幕后再应用。");
        foreach (var screen in layout.Screens)
        {
            var original = source.Layout.Screens.SingleOrDefault(s => s.Id == screen.Id);
            if (original is null || original.DeviceId != screen.DeviceId || source.Bindings.Count(b => b.ScreenId == screen.Id) != 1)
                throw new InvalidOperationException("屏幕所属设备已改变，请重新读取两端屏幕后再应用。");
        }
        return source with { Layout = layout };
    }

    internal static bool SameInventory(RuntimeLayoutDocument current, RuntimeLayoutDocument saved) =>
        current.LocalDeviceId == saved.LocalDeviceId && current.RemoteDeviceId == saved.RemoteDeviceId && current.Bindings.Count == saved.Bindings.Count &&
        current.Bindings.All(binding => saved.Bindings.Count(other => other == binding) == 1);

    private static string DirectoryPath => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Crossflow", "Layouts");
    internal static RuntimeLayoutDocument? Restore(RuntimeLayoutDocument current)
    {
        string path = Path.Combine(DirectoryPath, current.RemoteDeviceId + ".json");
        if (!File.Exists(path)) return null;
        if (new FileInfo(path).Length > 1_048_576) throw new InvalidOperationException("已保存布局过大，请重新排列屏幕。");
        var saved = JsonSerializer.Deserialize<RuntimeLayoutDocument>(File.ReadAllText(path), CoreBridge.Json);
        if (saved is null || !SameInventory(current, saved)) return null;
        return WithLayout(current, saved.Layout);
    }
    internal static string SaveSnapshot(RuntimeLayoutDocument config)
    {
        Directory.CreateDirectory(DirectoryPath);
        string json = JsonSerializer.Serialize(config, CoreBridge.Json);
        string snapshot = Path.Combine(DirectoryPath, "session-" + Guid.NewGuid().ToString("N") + ".json");
        // The running child receives its own immutable file, never the editable preset.
        File.WriteAllText(snapshot, json, new UTF8Encoding(false));
        string preset = Path.Combine(DirectoryPath, config.RemoteDeviceId + ".json");
        string temporary = preset + "." + Guid.NewGuid().ToString("N") + ".tmp";
        File.WriteAllText(temporary, json, new UTF8Encoding(false));
        File.Move(temporary, preset, true);
        return snapshot;
    }
}
