using System.Text;

namespace Crossflow.Windows;

internal static class StartupDiagnostics
{
    private static readonly object Sync = new();
    internal static string LogPath => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Crossflow", "startup.log");
    internal static void Write(string stage, Exception? exception = null)
    {
        try
        {
            lock (Sync)
            {
                Directory.CreateDirectory(Path.GetDirectoryName(LogPath)!);
                if (File.Exists(LogPath) && new FileInfo(LogPath).Length > 262144) File.WriteAllText(LogPath, "");
                string details = exception?.ToString() ?? "";
                if (details.Length > 16384) details = details[..16384];
                File.AppendAllText(LogPath, $"{DateTimeOffset.Now:O} pid={Environment.ProcessId} {stage}\n{details}\n", new UTF8Encoding(false));
            }
        }
        catch { /* Startup reporting must not create a second startup failure. */ }
    }
}
