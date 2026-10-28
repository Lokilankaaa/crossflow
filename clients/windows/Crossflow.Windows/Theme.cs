using System.Text.Json;
using Microsoft.UI.Xaml.Media;
namespace Crossflow.Windows;

internal sealed class Theme
{
    private readonly JsonElement data;
    public Theme()
    {
        using var document = JsonDocument.Parse(File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "design-tokens.json")));
        data = document.RootElement.Clone();
    }
    public double Number(string section, string key) => data.GetProperty(section).GetProperty(key).GetDouble();
    public string Copy(string key) => data.GetProperty("copy").GetProperty(key).GetString()!;
    public SolidColorBrush Brush(string key)
    {
        var value = data.GetProperty("colors").GetProperty(key).GetString()!;
        return new SolidColorBrush(global::Windows.UI.Color.FromArgb(255, Convert.ToByte(value[1..3], 16), Convert.ToByte(value[3..5], 16), Convert.ToByte(value[5..7], 16)));
    }
}
