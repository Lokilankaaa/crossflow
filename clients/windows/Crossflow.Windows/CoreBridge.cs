using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Serialization;
namespace Crossflow.Windows;

internal sealed record ScreenLayout(string Id, string DeviceId, double X, double Y, double Width, double Height);
internal sealed record LayoutDocument(List<ScreenLayout> Screens);
internal sealed class BridgeValidationException(string code, string message) : InvalidOperationException(message)
{
    internal string Code { get; } = code;
}
internal static class CoreBridge
{
    static CoreBridge() => NativeLibrary.SetDllImportResolver(typeof(CoreBridge).Assembly, (name, assembly, searchPath) => name == "crossflow_bridge" ? NativeLibrary.Load(Path.Combine(AppContext.BaseDirectory, "crossflow_bridge.dll")) : IntPtr.Zero);
    internal static readonly JsonSerializerOptions Json = new() { PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower, WriteIndented = true, UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow };
    [DllImport("crossflow_bridge", CallingConvention = CallingConvention.Cdecl)] private static extern uint crossflow_bridge_api_version();
    [DllImport("crossflow_bridge", CallingConvention = CallingConvention.Cdecl)]
    private static extern int crossflow_bridge_request_json([In] byte[] request, nuint length, [Out] byte[]? output, nuint capacity, out nuint required);
    internal static JsonElement Request(object request)
    {
        if (crossflow_bridge_api_version() != 1) throw new InvalidOperationException("布局内核版本不匹配，请重新构建客户端。");
        byte[] bytes = JsonSerializer.SerializeToUtf8Bytes(request, Json);
        int result = crossflow_bridge_request_json(bytes, (nuint)bytes.Length, null, 0, out nuint required);
        if (result != 1 || required == 0 || required > 1_048_576) throw new InvalidOperationException("布局内核暂时无法处理此操作。");
        byte[] output = new byte[(int)required];
        result = crossflow_bridge_request_json(bytes, (nuint)bytes.Length, output, (nuint)output.Length, out required);
        if (result != 0 || required > (nuint)output.Length) throw new InvalidOperationException("布局内核暂时无法处理此操作。");
        using var response = JsonDocument.Parse(output.AsMemory(0, (int)required));
        var root = response.RootElement;
        if (!root.GetProperty("ok").GetBoolean())
        {
            string code = root.GetProperty("error").GetProperty("code").GetString()!;
            throw new BridgeValidationException(code, code == "invalid_layout" ? "布局无法保存。请检查屏幕名称是否重复、尺寸是否有效，以及屏幕是否相互重叠。" : "无法预览这次穿越，请检查起点是否位于所选屏幕内。");
        }
        return root.GetProperty("result").Clone();
    }
    internal static void Validate(LayoutDocument layout) => Request(new { op = "layout_validate", layout });
    internal static LayoutDocument Parse(string text)
    {
        // Send original JSON to Rust before deserializing, preserving unknown-field rejection.
        using var document = JsonDocument.Parse(text);
        Request(new { op = "layout_validate", layout = document.RootElement });
        return JsonSerializer.Deserialize<LayoutDocument>(text, Json)!;
    }
    internal static JsonElement Crossing(LayoutDocument layout, ScreenLayout screen, string direction)
    {
        double x = screen.X + screen.Width / 2, y = screen.Y + screen.Height / 2;
        var end = direction switch
        {
            "左" => new { x = Math.BitDecrement(screen.X), y }, "上" => new { x, y = Math.BitDecrement(screen.Y) },
            "下" => new { x, y = Math.BitIncrement(screen.Y + screen.Height) }, _ => new { x = Math.BitIncrement(screen.X + screen.Width), y }
        };
        if (!double.IsFinite(end.x) || !double.IsFinite(end.y))
            throw new InvalidOperationException("此坐标范围无法预览穿越，可继续编辑或导出布局。");
        return Request(new { op = "layout_crossing", layout, from_screen = screen.Id, start = new { x, y }, end });
    }
}
