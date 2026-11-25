using System.Diagnostics;
using System.IO.Compression;
using System.Net.Http;
using System.Reflection;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Crossflow.Windows;

internal sealed record UpdateReleaseInfo(
    string Version,
    string TagName,
    string HtmlUrl,
    string Notes,
    string AssetName,
    string DownloadUrl,
    long AssetSize,
    string? ChecksumsUrl);

internal enum UpdaterPhase
{
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Verifying,
    Applying,
    Error
}

internal sealed record UpdaterState(
    UpdaterPhase Phase,
    string CurrentVersion,
    UpdateReleaseInfo? Release,
    double Progress,
    string Message);

/// GitHub Releases self-updater with mandatory SHA-256 verification, zip-slip protection,
/// atomic robocopy staging, and automatic CrossflowInputService refresh.
internal sealed class AppUpdater
{
    private const string LatestReleaseApi = "https://api.github.com/repos/Lokilankaaa/crossflow/releases/latest";
    private static readonly string[] AllowedHosts =
    [
        "api.github.com",
        "github.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com"
    ];

    private static readonly HttpClient Http = CreateHttpClient();
    private CancellationTokenSource? downloadCts;
    private UpdaterState state;

    internal event Action<UpdaterState>? Changed;
    internal UpdaterState State => state;
    internal static string CurrentVersion { get; } = ResolveCurrentVersion();

    internal AppUpdater()
    {
        state = new UpdaterState(UpdaterPhase.Idle, CurrentVersion, null, 0, $"当前版本 v{CurrentVersion}");
    }

    private static HttpClient CreateHttpClient()
    {
        var client = new HttpClient { Timeout = TimeSpan.FromMinutes(10) };
        client.DefaultRequestHeaders.UserAgent.ParseAdd($"Crossflow-Updater/{ResolveCurrentVersion()}");
        client.DefaultRequestHeaders.Accept.ParseAdd("application/vnd.github+json");
        return client;
    }

    private static string ResolveCurrentVersion()
    {
        var asm = Assembly.GetExecutingAssembly();
        var info = asm.GetCustomAttribute<AssemblyInformationalVersionAttribute>()?.InformationalVersion;
        if (!string.IsNullOrWhiteSpace(info))
        {
            int plus = info.IndexOf('+');
            string clean = plus >= 0 ? info[..plus] : info;
            clean = clean.Trim().TrimStart('v', 'V');
            if (TryParseSemVer(clean, out _)) return clean;
        }
        var ver = asm.GetName().Version;
        if (ver is not null && (ver.Major > 0 || ver.Minor > 0 || ver.Build > 0))
            return $"{ver.Major}.{ver.Minor}.{Math.Max(ver.Build, 0)}";
        return "0.4.0";
    }

    internal static bool TryParseSemVer(string? raw, out (int Major, int Minor, int Patch) parsed)
    {
        parsed = (0, 0, 0);
        if (string.IsNullOrWhiteSpace(raw)) return false;
        string s = raw.Trim().TrimStart('v', 'V');
        int dash = s.IndexOfAny(['-', '+']);
        if (dash >= 0) s = s[..dash];
        var parts = s.Split('.');
        if (parts.Length is < 2 or > 4) return false;
        if (!int.TryParse(parts[0], out int maj) || !int.TryParse(parts[1], out int min)) return false;
        int pat = 0;
        if (parts.Length >= 3 && !int.TryParse(parts[2], out pat)) return false;
        if (maj < 0 || min < 0 || pat < 0) return false;
        parsed = (maj, min, pat);
        return true;
    }

    internal static bool IsNewerVersion(string candidate, string current)
    {
        if (!TryParseSemVer(candidate, out var c) || !TryParseSemVer(current, out var cur))
            return false;
        return c.Major != cur.Major ? c.Major > cur.Major
            : c.Minor != cur.Minor ? c.Minor > cur.Minor
            : c.Patch > cur.Patch;
    }

    private static bool IsAllowedHttpsUrl(string? url)
    {
        if (!Uri.TryCreate(url, UriKind.Absolute, out var uri)) return false;
        if (!string.Equals(uri.Scheme, Uri.UriSchemeHttps, StringComparison.OrdinalIgnoreCase)) return false;
        return AllowedHosts.Any(h =>
            string.Equals(uri.Host, h, StringComparison.OrdinalIgnoreCase)
            || uri.Host.EndsWith("." + h, StringComparison.OrdinalIgnoreCase));
    }

    private void Publish(UpdaterState next)
    {
        state = next;
        Changed?.Invoke(next);
    }

    internal async Task CheckForUpdatesAsync(bool silent = false)
    {
        if (state.Phase is UpdaterPhase.Checking or UpdaterPhase.Downloading or UpdaterPhase.Verifying or UpdaterPhase.Applying)
            return;

        if (!silent)
            Publish(new UpdaterState(UpdaterPhase.Checking, CurrentVersion, state.Release, 0, "正在检查 GitHub 最新版本…"));

        try
        {
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(12));
            using var resp = await Http.GetAsync(LatestReleaseApi, cts.Token);
            if (!resp.IsSuccessStatusCode)
            {
                if (!silent)
                    Publish(new UpdaterState(UpdaterPhase.Error, CurrentVersion, null, 0, $"检查更新失败（HTTP {(int)resp.StatusCode}）"));
                return;
            }

            await using var stream = await resp.Content.ReadAsStreamAsync(cts.Token);
            var doc = await JsonSerializer.DeserializeAsync<GitHubReleaseDto>(stream, cancellationToken: cts.Token);
            if (doc is null || string.IsNullOrWhiteSpace(doc.TagName))
            {
                if (!silent)
                    Publish(new UpdaterState(UpdaterPhase.Error, CurrentVersion, null, 0, "无法解析最新版本信息。"));
                return;
            }

            string latestVer = doc.TagName.Trim().TrimStart('v', 'V');
            if (!IsNewerVersion(latestVer, CurrentVersion))
            {
                if (!silent)
                    Publish(new UpdaterState(UpdaterPhase.UpToDate, CurrentVersion, null, 0, $"已是最新版本（v{CurrentVersion}）"));
                return;
            }

            var winAsset = doc.Assets?.FirstOrDefault(a =>
                !string.IsNullOrWhiteSpace(a.Name)
                && a.Name.EndsWith("-windows-x64.zip", StringComparison.OrdinalIgnoreCase)
                && IsAllowedHttpsUrl(a.BrowserDownloadUrl));

            if (winAsset is null || string.IsNullOrWhiteSpace(winAsset.BrowserDownloadUrl))
            {
                if (!silent)
                    Publish(new UpdaterState(UpdaterPhase.Error, CurrentVersion, null, 0, $"发现 v{latestVer}，但未找到 Windows x64 安装包。"));
                return;
            }

            var checksumsAsset = doc.Assets?.FirstOrDefault(a =>
                string.Equals(a.Name, "SHA256SUMS.txt", StringComparison.OrdinalIgnoreCase)
                && IsAllowedHttpsUrl(a.BrowserDownloadUrl));

            var release = new UpdateReleaseInfo(
                latestVer,
                doc.TagName.Trim(),
                IsAllowedHttpsUrl(doc.HtmlUrl) ? doc.HtmlUrl! : $"https://github.com/Lokilankaaa/crossflow/releases/tag/{doc.TagName.Trim()}",
                doc.Body ?? "",
                winAsset.Name!,
                winAsset.BrowserDownloadUrl!,
                winAsset.Size,
                checksumsAsset?.BrowserDownloadUrl);

            string sizeText = release.AssetSize > 0 ? $"（{release.AssetSize / (1024.0 * 1024.0):0.1} MB）" : "";
            Publish(new UpdaterState(
                UpdaterPhase.Available,
                CurrentVersion,
                release,
                0,
                $"发现新版本 v{release.Version}{sizeText}"));
        }
        catch (OperationCanceledException)
        {
            if (!silent)
                Publish(new UpdaterState(UpdaterPhase.Error, CurrentVersion, null, 0, "检查更新超时，请检查网络连接。"));
        }
        catch (Exception ex)
        {
            if (!silent)
                Publish(new UpdaterState(UpdaterPhase.Error, CurrentVersion, null, 0, $"检查更新失败：{ex.Message}"));
        }
    }

    internal void CancelDownload()
    {
        try { downloadCts?.Cancel(); } catch { }
    }

    internal async Task<bool> DownloadAndApplyAsync(Func<Task> stopBeforeReplaceAsync)
    {
        var release = state.Release;
        if (release is null) return false;
        if (state.Phase is UpdaterPhase.Downloading or UpdaterPhase.Verifying or UpdaterPhase.Applying)
            return false;

        downloadCts?.Dispose();
        downloadCts = new CancellationTokenSource();
        var ct = downloadCts.Token;

        string workDir = Path.Combine(Path.GetTempPath(), "CrossflowUpdate", $"v{release.Version}-{Guid.NewGuid():N}");
        string zipPath = Path.Combine(workDir, release.AssetName);
        string stagingDir = Path.Combine(workDir, "staging");

        try
        {
            Directory.CreateDirectory(workDir);
            Publish(new UpdaterState(UpdaterPhase.Downloading, CurrentVersion, release, 0, $"正在下载 v{release.Version}…"));

            if (string.IsNullOrWhiteSpace(release.ChecksumsUrl))
            {
                TryDeleteDirectory(workDir);
                Publish(new UpdaterState(UpdaterPhase.Error, CurrentVersion, release, 0, "未找到 SHA256SUMS.txt 校验文件，已取消更新。"));
                return false;
            }
            string sumsText = await Http.GetStringAsync(release.ChecksumsUrl, ct);
            string? expectedSha256 = ParseExpectedSha256(sumsText, release.AssetName);
            if (string.IsNullOrWhiteSpace(expectedSha256))
            {
                TryDeleteDirectory(workDir);
                Publish(new UpdaterState(UpdaterPhase.Error, CurrentVersion, release, 0, "SHA256SUMS.txt 中缺少该安装包的校验值，已取消更新。"));
                return false;
            }

            using (var resp = await Http.GetAsync(release.DownloadUrl, HttpCompletionOption.ResponseHeadersRead, ct))
            {
                resp.EnsureSuccessStatusCode();
                long total = resp.Content.Headers.ContentLength ?? release.AssetSize;
                await using var netStream = await resp.Content.ReadAsStreamAsync(ct);
                await using var fileStream = new FileStream(zipPath, FileMode.Create, FileAccess.Write, FileShare.None, 65536, true);
                byte[] buffer = new byte[65536];
                long downloaded = 0;
                long lastReportTick = Environment.TickCount64;
                int read;
                while ((read = await netStream.ReadAsync(buffer, ct)) > 0)
                {
                    await fileStream.WriteAsync(buffer.AsMemory(0, read), ct);
                    downloaded += read;
                    long now = Environment.TickCount64;
                    if (now - lastReportTick >= 120 || (total > 0 && downloaded >= total))
                    {
                        lastReportTick = now;
                        double pct = total > 0 ? Math.Clamp((double)downloaded / total, 0, 1) : 0;
                        string mb = $"{downloaded / (1024.0 * 1024.0):0.1}";
                        string totalMb = total > 0 ? $" / {total / (1024.0 * 1024.0):0.1} MB" : " MB";
                        Publish(new UpdaterState(
                            UpdaterPhase.Downloading,
                            CurrentVersion,
                            release,
                            pct,
                            $"正在下载 v{release.Version}… {pct * 100:0}%（{mb}{totalMb}）"));
                    }
                }
            }

            Publish(new UpdaterState(UpdaterPhase.Verifying, CurrentVersion, release, 1.0, "正在校验 SHA-256 完整性…"));
            string actualSha256 = await ComputeFileSha256Async(zipPath, ct);
            if (!string.IsNullOrWhiteSpace(expectedSha256)
                && !string.Equals(actualSha256, expectedSha256, StringComparison.OrdinalIgnoreCase))
            {
                TryDeleteDirectory(workDir);
                Publish(new UpdaterState(UpdaterPhase.Error, CurrentVersion, release, 0, "安装包 SHA-256 校验失败，已取消更新。"));
                return false;
            }

            Publish(new UpdaterState(UpdaterPhase.Applying, CurrentVersion, release, 1.0, "正在解压并准备重启更新…"));
            await Task.Run(() => ExtractZipSafely(zipPath, stagingDir), ct);

            string payloadDir = ResolvePayloadDir(stagingDir);
            if (!File.Exists(Path.Combine(payloadDir, "Crossflow.Windows.exe"))
                || !File.Exists(Path.Combine(payloadDir, "crossflow.exe")))
            {
                TryDeleteDirectory(workDir);
                Publish(new UpdaterState(UpdaterPhase.Error, CurrentVersion, release, 0, "更新包结构不完整，已取消更新。"));
                return false;
            }

            string installDir = AppContext.BaseDirectory.TrimEnd('\\', '/');
            bool needsElevation = !CanWriteDirectory(installDir);
            string scriptPath = Path.Combine(workDir, "apply-update.ps1");
            WriteApplyScript(scriptPath, Environment.ProcessId, payloadDir, installDir, workDir);

            await stopBeforeReplaceAsync();

            if (needsElevation)
            {
                var psi = new ProcessStartInfo("powershell.exe")
                {
                    UseShellExecute = true,
                    Verb = "runas",
                    WindowStyle = ProcessWindowStyle.Hidden,
                    WorkingDirectory = Path.GetTempPath(),
                    Arguments = $"-NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{scriptPath}\""
                };
                Process.Start(psi);
            }
            else
            {
                var psi = new ProcessStartInfo("powershell.exe")
                {
                    UseShellExecute = false,
                    CreateNoWindow = true,
                    WorkingDirectory = Path.GetTempPath()
                };
                psi.ArgumentList.Add("-NoProfile");
                psi.ArgumentList.Add("-NonInteractive");
                psi.ArgumentList.Add("-ExecutionPolicy");
                psi.ArgumentList.Add("Bypass");
                psi.ArgumentList.Add("-File");
                psi.ArgumentList.Add(scriptPath);
                Process.Start(psi);
            }
            return true;
        }
        catch (OperationCanceledException)
        {
            TryDeleteDirectory(workDir);
            Publish(new UpdaterState(UpdaterPhase.Available, CurrentVersion, release, 0, $"已取消下载，可随时更新至 v{release.Version}"));
            return false;
        }
        catch (Exception ex)
        {
            TryDeleteDirectory(workDir);
            Publish(new UpdaterState(UpdaterPhase.Error, CurrentVersion, release, 0, $"更新失败：{ex.Message}"));
            return false;
        }
    }

    private static string? ParseExpectedSha256(string text, string assetName)
    {
        foreach (string rawLine in text.Split('\n'))
        {
            string line = rawLine.Trim();
            if (line.Length < 66) continue;
            var parts = line.Split([' ', '\t'], StringSplitOptions.RemoveEmptyEntries);
            if (parts.Length >= 2 && string.Equals(parts[1].TrimStart('*'), assetName, StringComparison.OrdinalIgnoreCase))
            {
                string hash = parts[0].Trim();
                if (hash.Length == 64 && hash.All(Uri.IsHexDigit))
                    return hash.ToLowerInvariant();
            }
        }
        return null;
    }

    private static async Task<string> ComputeFileSha256Async(string path, CancellationToken ct)
    {
        await using var fs = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read, 65536, true);
        byte[] hash = await SHA256.HashDataAsync(fs, ct);
        return Convert.ToHexString(hash).ToLowerInvariant();
    }

    private static void ExtractZipSafely(string zipPath, string destinationDir)
    {
        Directory.CreateDirectory(destinationDir);
        string destFull = Path.GetFullPath(destinationDir);
        if (!destFull.EndsWith(Path.DirectorySeparatorChar)) destFull += Path.DirectorySeparatorChar;
        using var archive = ZipFile.OpenRead(zipPath);
        foreach (var entry in archive.Entries)
        {
            string outPath = Path.GetFullPath(Path.Combine(destFull, entry.FullName));
            if (!outPath.StartsWith(destFull, StringComparison.OrdinalIgnoreCase))
                throw new InvalidDataException("更新包包含非法路径。");
            if (string.IsNullOrEmpty(entry.Name))
            {
                Directory.CreateDirectory(outPath);
            }
            else
            {
                Directory.CreateDirectory(Path.GetDirectoryName(outPath)!);
                entry.ExtractToFile(outPath, overwrite: true);
            }
        }
    }

    private static void WriteApplyScript(string scriptPath, int pid, string stagingDir, string installDir, string workDir)
    {
        static string Esc(string s) => s.Replace("'", "''");
        string script = $@"
$ErrorActionPreference = 'SilentlyContinue'
$targetPid = {pid}
$staging = '{Esc(stagingDir)}'
$install = '{Esc(installDir)}'
$work = '{Esc(workDir)}'

for ($i = 0; $i -lt 50; $i++) {{
    if (-not (Get-Process -Id $targetPid -ErrorAction SilentlyContinue)) {{ break }}
    Start-Sleep -Milliseconds 200
}}
if (Get-Process -Id $targetPid -ErrorAction SilentlyContinue) {{
    Stop-Process -Id $targetPid -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 300
}}
Get-Process -Name 'crossflow' -ErrorAction SilentlyContinue | Where-Object {{
    try {{ $_.Path -and $_.Path.StartsWith($install, [StringComparison]::OrdinalIgnoreCase) }} catch {{ $false }}
}} | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300

& robocopy $staging $install /E /IS /IT /R:8 /W:1 /NFL /NDL /NJH /NJS /NP | Out-Null

$svc = Get-Service -Name 'CrossflowInputService' -ErrorAction SilentlyContinue
if ($svc) {{
    $coreExe = Join-Path $install 'crossflow.exe'
    if (Test-Path $coreExe) {{
        Start-Process -FilePath $coreExe -ArgumentList 'install-service' -Verb RunAs -WindowStyle Hidden -Wait
    }}
}}

$appExe = Join-Path $install 'Crossflow.Windows.exe'
if (Test-Path $appExe) {{
    Start-Process -FilePath $appExe -WorkingDirectory $install
}}
Start-Sleep -Seconds 2
Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
";
        File.WriteAllText(scriptPath, script, new UTF8Encoding(encoderShouldEmitUTF8Identifier: true));
    }

    private static string ResolvePayloadDir(string stagingDir)
    {
        if (File.Exists(Path.Combine(stagingDir, "Crossflow.Windows.exe")))
            return stagingDir;
        var subDirs = Directory.GetDirectories(stagingDir);
        if (subDirs.Length == 1 && File.Exists(Path.Combine(subDirs[0], "Crossflow.Windows.exe")))
            return subDirs[0];
        return stagingDir;
    }

    private static bool CanWriteDirectory(string dir)
    {
        try
        {
            string probe = Path.Combine(dir, $".crossflow-write-test-{Guid.NewGuid():N}.tmp");
            File.WriteAllText(probe, "ok");
            File.Delete(probe);
            return true;
        }
        catch
        {
            return false;
        }
    }

    private static void TryDeleteDirectory(string path)
    {
        try { if (Directory.Exists(path)) Directory.Delete(path, recursive: true); } catch { }
    }

    private sealed class GitHubReleaseDto
    {
        [JsonPropertyName("tag_name")] public string? TagName { get; set; }
        [JsonPropertyName("html_url")] public string? HtmlUrl { get; set; }
        [JsonPropertyName("body")] public string? Body { get; set; }
        [JsonPropertyName("assets")] public List<GitHubAssetDto>? Assets { get; set; }
    }

    private sealed class GitHubAssetDto
    {
        [JsonPropertyName("name")] public string? Name { get; set; }
        [JsonPropertyName("size")] public long Size { get; set; }
        [JsonPropertyName("browser_download_url")] public string? BrowserDownloadUrl { get; set; }
    }
}
