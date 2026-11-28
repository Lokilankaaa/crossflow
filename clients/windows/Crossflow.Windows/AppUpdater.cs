using System.Diagnostics;
using System.IO.Compression;
using System.Net;
using System.Net.Http;
using System.Reflection;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;
using System.Xml.Linq;

namespace Crossflow.Windows;

internal sealed record UpdateReleaseInfo(
    string Version,
    string TagName,
    string HtmlUrl,
    string Notes,
    string AssetName,
    string DownloadUrl,
    long AssetSize,
    string? ChecksumsUrl,
    bool IsPrerelease);

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
    string Message,
    string MirrorPrefix,
    bool IncludePrerelease);

internal sealed record MirrorPreset(string Id, string Label, string Description, string? Prefix);

internal sealed class UpdaterSettings
{
    [JsonPropertyName("mirror_prefix")] public string MirrorPrefix { get; set; } = "";
    [JsonPropertyName("include_prerelease")] public bool IncludePrerelease { get; set; } = false;
}

/// GitHub Releases self-updater with Atom feed + web redirect + REST API fallback,
/// optional HTTPS GitHub mirror prefix with automatic direct fallback, stable / prerelease (-rc)
/// channel switching, mandatory SHA-256 verification, zip-slip protection, atomic robocopy
/// staging, and automatic CrossflowInputService refresh.
internal sealed class AppUpdater
{
    internal const string RepoBaseUrl = "https://github.com/Lokilankaaa/crossflow";
    internal const string ReleasesAtomUrl = RepoBaseUrl + "/releases.atom";
    internal const string LatestReleaseWebUrl = RepoBaseUrl + "/releases/latest";
    internal const string ReleasesPageUrl = RepoBaseUrl + "/releases";
    internal const string IssuesNewUrl = RepoBaseUrl + "/issues/new/choose";
    internal const string AuthorGitHubUrl = "https://github.com/Lokilankaaa";
    internal const string AuthorBilibiliUrl = "https://b23.tv/pANRtSc";

    private const string LatestReleaseApi = "https://api.github.com/repos/Lokilankaaa/crossflow/releases/latest";
    private const string ReleasesListApi = "https://api.github.com/repos/Lokilankaaa/crossflow/releases?per_page=10";

    private static readonly string[] AllowedHosts =
    [
        "api.github.com",
        "github.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com"
    ];

    internal static readonly IReadOnlyList<MirrorPreset> MirrorPresets =
    [
        new("direct", "GitHub 官方直连", "默认直连 github.com，适合海外或代理网络环境", ""),
        new("ghproxy", "GH-Proxy · gh-proxy.com", "国内常用 GitHub Releases 加速节点，失败自动回退直连", "https://gh-proxy.com/"),
        new("ghfast", "GHFast · ghfast.top", "国内备用 GitHub 下载加速镜像节点，失败自动回退直连", "https://ghfast.top/"),
        new("custom", "自定义镜像前缀…", "使用自建或自定义 HTTPS 加速前缀", null)
    ];

    private static readonly Regex TagInUrlRegex = new(@"/releases/tag/(?<tag>[^/?#]+)", RegexOptions.IgnoreCase | RegexOptions.Compiled);
    private static readonly Regex HtmlTagRegex = new(@"<[^>]+>", RegexOptions.Compiled);
    private static readonly Regex HtmlBreakRegex = new(@"<\s*(br|p|div|li|h[1-6]|tr)\b[^>]*>", RegexOptions.IgnoreCase | RegexOptions.Compiled);

    private static readonly HttpClient Http = CreateHttpClient();
    private CancellationTokenSource? downloadCts;
    private UpdaterSettings settings;
    private UpdaterState state;

    internal event Action<UpdaterState>? Changed;
    internal UpdaterState State => state;
    internal static string CurrentVersion { get; } = ResolveCurrentVersion();
    internal string MirrorPrefix => settings.MirrorPrefix;
    internal bool IncludePrerelease => settings.IncludePrerelease;

    internal static string UserDataDir =>
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Crossflow");

    internal static string LogsDir => Path.Combine(UserDataDir, "Logs");
    private static string SettingsPath => Path.Combine(UserDataDir, "updater-settings.json");

    internal AppUpdater()
    {
        settings = LoadSettings();
        state = new UpdaterState(
            UpdaterPhase.Idle,
            CurrentVersion,
            null,
            0,
            $"当前版本 v{CurrentVersion}",
            settings.MirrorPrefix,
            settings.IncludePrerelease);
    }

    internal static string DescribeMirror(string? prefix)
    {
        string clean = NormalizeMirrorPrefix(prefix);
        if (string.IsNullOrEmpty(clean)) return "GitHub 官方直连";
        var matched = MirrorPresets.FirstOrDefault(p =>
            !string.IsNullOrEmpty(p.Prefix)
            && string.Equals(p.Prefix, clean, StringComparison.OrdinalIgnoreCase));
        return matched?.Label ?? clean.TrimEnd('/');
    }

    internal static string ResolveMirrorPresetId(string? prefix)
    {
        string clean = NormalizeMirrorPrefix(prefix);
        if (string.IsNullOrEmpty(clean)) return "direct";
        var matched = MirrorPresets.FirstOrDefault(p =>
            !string.IsNullOrEmpty(p.Prefix)
            && string.Equals(p.Prefix, clean, StringComparison.OrdinalIgnoreCase));
        return matched?.Id ?? "custom";
    }

    internal static string NormalizeMirrorPrefix(string? raw)
    {
        if (string.IsNullOrWhiteSpace(raw)) return "";
        string trimmed = raw.Trim();
        if (!Uri.TryCreate(trimmed, UriKind.Absolute, out var uri)) return "";
        if (!string.Equals(uri.Scheme, Uri.UriSchemeHttps, StringComparison.OrdinalIgnoreCase)) return "";
        return trimmed.EndsWith('/') ? trimmed : trimmed + "/";
    }

    internal static IReadOnlyList<string> BuildCandidateUrls(string officialUrl, string? mirrorPrefix)
    {
        var list = new List<string>(2);
        string cleanMirror = NormalizeMirrorPrefix(mirrorPrefix);
        if (!string.IsNullOrEmpty(cleanMirror) && IsAllowedHttpsUrl(officialUrl))
        {
            list.Add(cleanMirror + officialUrl);
        }
        list.Add(officialUrl);
        return list;
    }

    internal bool SetMirrorPrefix(string? rawPrefix, out string? error)
    {
        error = null;
        string trimmed = (rawPrefix ?? "").Trim();
        if (trimmed.Length == 0)
        {
            settings.MirrorPrefix = "";
        }
        else
        {
            string normalized = NormalizeMirrorPrefix(trimmed);
            if (string.IsNullOrEmpty(normalized))
            {
                error = "镜像前缀必须为有效的 https:// 地址，例如 https://gh-proxy.com/";
                return false;
            }
            settings.MirrorPrefix = normalized;
        }
        SaveSettings(settings);
        Publish(state with { MirrorPrefix = settings.MirrorPrefix });
        return true;
    }

    internal void SetIncludePrerelease(bool includePrerelease)
    {
        if (settings.IncludePrerelease == includePrerelease) return;
        settings.IncludePrerelease = includePrerelease;
        SaveSettings(settings);
        Publish(state with { IncludePrerelease = includePrerelease });
    }

    private static UpdaterSettings LoadSettings()
    {
        try
        {
            if (File.Exists(SettingsPath))
            {
                string json = File.ReadAllText(SettingsPath, Encoding.UTF8);
                var parsed = JsonSerializer.Deserialize<UpdaterSettings>(json);
                if (parsed is not null)
                {
                    parsed.MirrorPrefix = NormalizeMirrorPrefix(parsed.MirrorPrefix);
                    return parsed;
                }
            }
        }
        catch { }
        return new UpdaterSettings();
    }

    private static void SaveSettings(UpdaterSettings value)
    {
        try
        {
            Directory.CreateDirectory(UserDataDir);
            string json = JsonSerializer.Serialize(value, new JsonSerializerOptions { WriteIndented = true });
            File.WriteAllText(SettingsPath, json, new UTF8Encoding(false));
        }
        catch { }
    }

    private static HttpClient CreateHttpClient()
    {
        var client = new HttpClient { Timeout = TimeSpan.FromMinutes(10) };
        client.DefaultRequestHeaders.UserAgent.ParseAdd($"Crossflow-Updater/{ResolveCurrentVersion()}");
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
            if (TryParseVersion(clean, out _)) return clean;
        }
        var ver = asm.GetName().Version;
        if (ver is not null && (ver.Major > 0 || ver.Minor > 0 || ver.Build > 0))
            return $"{ver.Major}.{ver.Minor}.{Math.Max(ver.Build, 0)}";
        return "0.4.1";
    }

    internal static bool IsPrereleaseTag(string? raw)
    {
        if (string.IsNullOrWhiteSpace(raw)) return false;
        return raw.Contains("-rc", StringComparison.OrdinalIgnoreCase)
            || raw.Contains("-beta", StringComparison.OrdinalIgnoreCase)
            || raw.Contains("-alpha", StringComparison.OrdinalIgnoreCase);
    }

    internal static bool TryParseVersion(string? raw, out (int Major, int Minor, int Patch, string Pre) parsed)
    {
        parsed = (0, 0, 0, "");
        if (string.IsNullOrWhiteSpace(raw)) return false;
        string s = raw.Trim().TrimStart('v', 'V');
        int plus = s.IndexOf('+');
        if (plus >= 0) s = s[..plus];
        string pre = "";
        int dash = s.IndexOf('-');
        if (dash >= 0)
        {
            pre = s[(dash + 1)..].Trim();
            s = s[..dash];
        }
        var parts = s.Split('.');
        if (parts.Length is < 2 or > 4) return false;
        if (!int.TryParse(parts[0], out int maj) || !int.TryParse(parts[1], out int min)) return false;
        int pat = 0;
        if (parts.Length >= 3 && !int.TryParse(parts[2], out pat)) return false;
        if (maj < 0 || min < 0 || pat < 0) return false;
        parsed = (maj, min, pat, pre);
        return true;
    }

    internal static int CompareVersions(string candidate, string current)
    {
        if (!TryParseVersion(candidate, out var c) || !TryParseVersion(current, out var cur))
            return 0;
        if (c.Major != cur.Major) return c.Major.CompareTo(cur.Major);
        if (c.Minor != cur.Minor) return c.Minor.CompareTo(cur.Minor);
        if (c.Patch != cur.Patch) return c.Patch.CompareTo(cur.Patch);

        bool cPre = !string.IsNullOrEmpty(c.Pre);
        bool curPre = !string.IsNullOrEmpty(cur.Pre);
        if (!cPre && !curPre) return 0;
        if (!cPre && curPre) return 1; // 0.4.2 > 0.4.2-rc.1
        if (cPre && !curPre) return -1;

        var aParts = c.Pre.Split('.');
        var bParts = cur.Pre.Split('.');
        int len = Math.Max(aParts.Length, bParts.Length);
        for (int i = 0; i < len; i++)
        {
            if (i >= aParts.Length) return -1;
            if (i >= bParts.Length) return 1;
            bool aNum = int.TryParse(aParts[i], out int ai);
            bool bNum = int.TryParse(bParts[i], out int bi);
            int cmp = (aNum && bNum)
                ? ai.CompareTo(bi)
                : string.Compare(aParts[i], bParts[i], StringComparison.OrdinalIgnoreCase);
            if (cmp != 0) return cmp;
        }
        return 0;
    }

    internal static bool IsNewerVersion(string candidate, string current) =>
        CompareVersions(candidate, current) > 0;

    internal static bool IsAllowedHttpsUrl(string? url)
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

    private UpdaterState MakeState(UpdaterPhase phase, UpdateReleaseInfo? rel, double progress, string message) =>
        new(phase, CurrentVersion, rel, progress, message, settings.MirrorPrefix, settings.IncludePrerelease);

    internal async Task CheckForUpdatesAsync(bool silent = false)
    {
        if (state.Phase is UpdaterPhase.Checking or UpdaterPhase.Downloading or UpdaterPhase.Verifying or UpdaterPhase.Applying)
            return;

        bool includePre = settings.IncludePrerelease;
        string mirror = settings.MirrorPrefix;

        if (!silent)
            Publish(MakeState(UpdaterPhase.Checking, state.Release, 0, "正在检查 GitHub 最新版本…"));

        try
        {
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(18));

            // 1. Primary: GitHub Releases Atom feed (zero API rate limit, works through mirror,
            //    supports stable + prerelease channels, includes release notes).
            var atomHit = await FetchLatestFromAtomAsync(includePre, mirror, cts.Token);

            string? tagName = atomHit?.TagName;
            string notes = atomHit?.Notes ?? "";
            string? htmlUrl = atomHit?.HtmlUrl;
            GitHubReleaseDto? apiDoc = null;

            // 2. Fallback (stable): GitHub /releases/latest redirect/JSON (zero API rate limit)
            if (string.IsNullOrWhiteSpace(tagName) && !includePre)
            {
                tagName = await FetchLatestTagFromWebAsync(mirror, cts.Token);
            }

            // 3. Final fallback: GitHub REST API
            if (string.IsNullOrWhiteSpace(tagName))
            {
                string apiUrl = includePre ? ReleasesListApi : LatestReleaseApi;
                using var apiReq = new HttpRequestMessage(HttpMethod.Get, apiUrl);
                apiReq.Headers.Accept.ParseAdd("application/vnd.github+json");
                using var resp = await Http.SendAsync(apiReq, cts.Token);
                if (!resp.IsSuccessStatusCode)
                {
                    if (!silent)
                        Publish(MakeState(UpdaterPhase.Error, null, 0, $"检查更新失败（HTTP {(int)resp.StatusCode}），可切换下载加速线路后重试"));
                    return;
                }
                await using var stream = await resp.Content.ReadAsStreamAsync(cts.Token);
                if (includePre)
                {
                    var list = await JsonSerializer.DeserializeAsync<List<GitHubReleaseDto>>(stream, cancellationToken: cts.Token);
                    apiDoc = list?
                        .Where(r => !r.Draft && !string.IsNullOrWhiteSpace(r.TagName) && r.TagName!.StartsWith("v", StringComparison.OrdinalIgnoreCase))
                        .OrderByDescending(r => r.TagName!, Comparer<string>.Create(CompareVersions))
                        .FirstOrDefault();
                }
                else
                {
                    apiDoc = await JsonSerializer.DeserializeAsync<GitHubReleaseDto>(stream, cancellationToken: cts.Token);
                }
                tagName = apiDoc?.TagName?.Trim();
                notes = apiDoc?.Body ?? notes;
                htmlUrl = apiDoc?.HtmlUrl ?? htmlUrl;
            }

            if (string.IsNullOrWhiteSpace(tagName))
            {
                if (!silent)
                    Publish(MakeState(UpdaterPhase.Error, null, 0, "无法解析最新版本信息，请检查网络或切换镜像线路。"));
                return;
            }

            string latestVer = tagName.Trim().TrimStart('v', 'V');
            if (!IsNewerVersion(latestVer, CurrentVersion))
            {
                if (!silent)
                    Publish(MakeState(UpdaterPhase.UpToDate, null, 0, $"已是最新版本（v{CurrentVersion}）"));
                return;
            }

            bool isPre = IsPrereleaseTag(tagName);
            UpdateReleaseInfo? release = null;
            if (apiDoc is not null)
            {
                var winAsset = apiDoc.Assets?.FirstOrDefault(a =>
                    !string.IsNullOrWhiteSpace(a.Name)
                    && a.Name.EndsWith("-windows-x64.zip", StringComparison.OrdinalIgnoreCase)
                    && IsAllowedHttpsUrl(a.BrowserDownloadUrl));
                var checksumsAsset = apiDoc.Assets?.FirstOrDefault(a =>
                    string.Equals(a.Name, "SHA256SUMS.txt", StringComparison.OrdinalIgnoreCase)
                    && IsAllowedHttpsUrl(a.BrowserDownloadUrl));
                if (winAsset is not null && !string.IsNullOrWhiteSpace(winAsset.BrowserDownloadUrl))
                {
                    release = new UpdateReleaseInfo(
                        latestVer,
                        tagName,
                        IsAllowedHttpsUrl(apiDoc.HtmlUrl) ? apiDoc.HtmlUrl! : $"{RepoBaseUrl}/releases/tag/{Uri.EscapeDataString(tagName)}",
                        TrimNotes(notes),
                        winAsset.Name!,
                        winAsset.BrowserDownloadUrl!,
                        winAsset.Size,
                        checksumsAsset?.BrowserDownloadUrl,
                        isPre);
                }
            }
            else
            {
                string escapedTag = Uri.EscapeDataString(tagName);
                string checksumsUrl = $"{RepoBaseUrl}/releases/download/{escapedTag}/SHA256SUMS.txt";
                string assetName = $"Crossflow-{tagName}-windows-x64.zip";
                string? sumsText = await TryGetStringWithMirrorAsync(checksumsUrl, mirror, cts.Token);
                if (!string.IsNullOrWhiteSpace(sumsText))
                {
                    string? matched = FindAssetNameInChecksums(sumsText, "-windows-x64.zip");
                    if (!string.IsNullOrWhiteSpace(matched))
                        assetName = matched;
                }

                string downloadUrl = $"{RepoBaseUrl}/releases/download/{escapedTag}/{assetName}";
                long assetSize = await ProbeContentLengthAsync(downloadUrl, mirror, cts.Token);
                release = new UpdateReleaseInfo(
                    latestVer,
                    tagName,
                    IsAllowedHttpsUrl(htmlUrl) ? htmlUrl! : $"{RepoBaseUrl}/releases/tag/{escapedTag}",
                    TrimNotes(notes),
                    assetName,
                    downloadUrl,
                    assetSize,
                    checksumsUrl,
                    isPre);
            }

            if (release is null || !IsAllowedHttpsUrl(release.DownloadUrl))
            {
                if (!silent)
                    Publish(MakeState(UpdaterPhase.Error, null, 0, $"发现 v{latestVer}，但未找到 Windows x64 安装包。"));
                return;
            }

            string preBadge = release.IsPrerelease ? " RC" : "";
            string sizeText = release.AssetSize > 0 ? $"（{release.AssetSize / (1024.0 * 1024.0):F1} MB）" : "";
            Publish(MakeState(
                UpdaterPhase.Available,
                release,
                0,
                $"发现新版本 v{release.Version}{preBadge}{sizeText}"));
        }
        catch (OperationCanceledException)
        {
            if (!silent)
                Publish(MakeState(UpdaterPhase.Error, null, 0, "检查更新超时，请检查网络或切换下载加速线路。"));
        }
        catch (Exception ex)
        {
            if (!silent)
                Publish(MakeState(UpdaterPhase.Error, null, 0, $"检查更新失败：{ex.Message}"));
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

        string mirror = settings.MirrorPrefix;
        string workDir = Path.Combine(Path.GetTempPath(), "CrossflowUpdate", $"v{release.Version}-{Guid.NewGuid():N}");
        string zipPath = Path.Combine(workDir, release.AssetName);
        string stagingDir = Path.Combine(workDir, "staging");

        try
        {
            Directory.CreateDirectory(workDir);
            Publish(MakeState(UpdaterPhase.Downloading, release, 0, $"正在下载 v{release.Version}…"));

            if (string.IsNullOrWhiteSpace(release.ChecksumsUrl))
            {
                TryDeleteDirectory(workDir);
                Publish(MakeState(UpdaterPhase.Error, release, 0, "未找到 SHA256SUMS.txt 校验文件，已取消更新。"));
                return false;
            }

            string? sumsText = await TryGetStringWithMirrorAsync(release.ChecksumsUrl, mirror, ct);
            string? expectedSha256 = string.IsNullOrWhiteSpace(sumsText)
                ? null
                : ParseExpectedSha256(sumsText, release.AssetName);
            if (string.IsNullOrWhiteSpace(expectedSha256))
            {
                TryDeleteDirectory(workDir);
                Publish(MakeState(UpdaterPhase.Error, release, 0, "SHA256SUMS.txt 获取或解析失败，已取消更新。"));
                return false;
            }

            var candidates = BuildCandidateUrls(release.DownloadUrl, mirror);
            bool verifiedOk = false;
            string? lastFailure = null;

            for (int idx = 0; idx < candidates.Count; idx++)
            {
                string dlUrl = candidates[idx];
                bool viaMirror = idx == 0 && candidates.Count > 1;
                try
                {
                    if (File.Exists(zipPath)) File.Delete(zipPath);
                    using (var resp = await Http.GetAsync(dlUrl, HttpCompletionOption.ResponseHeadersRead, ct))
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
                                string mb = $"{downloaded / (1024.0 * 1024.0):F1}";
                                string totalMb = total > 0 ? $" / {total / (1024.0 * 1024.0):F1} MB" : " MB";
                                string route = viaMirror ? "镜像" : "直连";
                                Publish(MakeState(
                                    UpdaterPhase.Downloading,
                                    release,
                                    pct,
                                    $"正在下载 v{release.Version}（{route}）… {pct * 100:0}%（{mb}{totalMb}）"));
                            }
                        }
                    }

                    Publish(MakeState(UpdaterPhase.Verifying, release, 1.0, "正在校验 SHA-256 完整性…"));
                    string actualSha256 = await ComputeFileSha256Async(zipPath, ct);
                    if (string.Equals(actualSha256, expectedSha256, StringComparison.OrdinalIgnoreCase))
                    {
                        verifiedOk = true;
                        break;
                    }
                    lastFailure = "安装包 SHA-256 校验不匹配";
                    if (viaMirror)
                    {
                        Publish(MakeState(UpdaterPhase.Downloading, release, 0, "镜像校验异常，正在自动回退 GitHub 官方直连…"));
                    }
                }
                catch (OperationCanceledException)
                {
                    throw;
                }
                catch (Exception ex)
                {
                    lastFailure = ex.Message;
                    if (viaMirror)
                    {
                        Publish(MakeState(UpdaterPhase.Downloading, release, 0, "镜像线路不可用，正在自动回退 GitHub 官方直连…"));
                    }
                }
            }

            if (!verifiedOk)
            {
                TryDeleteDirectory(workDir);
                Publish(MakeState(UpdaterPhase.Error, release, 0, $"更新包下载或校验失败：{lastFailure ?? "网络异常"}"));
                return false;
            }

            Publish(MakeState(UpdaterPhase.Applying, release, 1.0, "正在解压并准备重启更新…"));
            await Task.Run(() => ExtractZipSafely(zipPath, stagingDir), ct);

            string payloadDir = ResolvePayloadDir(stagingDir);
            if (!File.Exists(Path.Combine(payloadDir, "Crossflow.Windows.exe"))
                || !File.Exists(Path.Combine(payloadDir, "crossflow.exe")))
            {
                TryDeleteDirectory(workDir);
                Publish(MakeState(UpdaterPhase.Error, release, 0, "更新包结构不完整，已取消更新。"));
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
            Publish(MakeState(UpdaterPhase.Available, release, 0, $"已取消下载，可随时更新至 v{release.Version}"));
            return false;
        }
        catch (Exception ex)
        {
            TryDeleteDirectory(workDir);
            Publish(MakeState(UpdaterPhase.Error, release, 0, $"更新失败：{ex.Message}"));
            return false;
        }
    }

    private sealed record AtomEntry(string TagName, string Notes, string HtmlUrl);

    private static async Task<AtomEntry?> FetchLatestFromAtomAsync(bool includePrerelease, string mirrorPrefix, CancellationToken ct)
    {
        string? xml = await TryGetStringWithMirrorAsync(ReleasesAtomUrl, mirrorPrefix, ct);
        if (string.IsNullOrWhiteSpace(xml)) return null;
        try
        {
            var doc = XDocument.Parse(xml);
            XNamespace ns = doc.Root?.GetDefaultNamespace() ?? XNamespace.None;
            AtomEntry? best = null;
            foreach (var entry in doc.Descendants(ns + "entry"))
            {
                string linkHref = entry.Elements(ns + "link")
                    .Select(l => (string?)l.Attribute("href") ?? "")
                    .FirstOrDefault(h => h.Contains("/releases/tag/", StringComparison.OrdinalIgnoreCase)) ?? "";

                string tag = "";
                var m = TagInUrlRegex.Match(linkHref);
                if (m.Success)
                {
                    tag = Uri.UnescapeDataString(m.Groups["tag"].Value);
                }
                else
                {
                    string idVal = (string?)entry.Element(ns + "id") ?? "";
                    int slash = idVal.LastIndexOf('/');
                    if (slash >= 0 && slash + 1 < idVal.Length)
                        tag = idVal[(slash + 1)..];
                }

                if (string.IsNullOrWhiteSpace(tag) || !tag.StartsWith("v", StringComparison.OrdinalIgnoreCase))
                    continue;

                if (IsPrereleaseTag(tag) && !includePrerelease)
                    continue;

                string rawHtml = (string?)entry.Element(ns + "content") ?? "";
                string notes = StripHtmlToPlainText(rawHtml);
                string htmlUrl = IsAllowedHttpsUrl(linkHref)
                    ? linkHref
                    : $"{RepoBaseUrl}/releases/tag/{Uri.EscapeDataString(tag)}";

                if (best is null || CompareVersions(tag, best.TagName) > 0)
                    best = new AtomEntry(tag.Trim(), notes, htmlUrl);
            }
            return best;
        }
        catch
        {
            return null;
        }
    }

    private static async Task<string?> TryGetStringWithMirrorAsync(string officialUrl, string mirrorPrefix, CancellationToken ct)
    {
        foreach (string url in BuildCandidateUrls(officialUrl, mirrorPrefix))
        {
            try
            {
                using var req = new HttpRequestMessage(HttpMethod.Get, url);
                using var resp = await Http.SendAsync(req, ct);
                if (resp.IsSuccessStatusCode)
                    return await resp.Content.ReadAsStringAsync(ct);
            }
            catch (OperationCanceledException) { throw; }
            catch { }
        }
        return null;
    }

    private static async Task<string?> FetchLatestTagFromWebAsync(string mirrorPrefix, CancellationToken ct)
    {
        foreach (string url in BuildCandidateUrls(LatestReleaseWebUrl, mirrorPrefix))
        {
            try
            {
                using var req = new HttpRequestMessage(HttpMethod.Get, url);
                req.Headers.Accept.ParseAdd("application/json");
                using var resp = await Http.SendAsync(req, ct);
                if (!resp.IsSuccessStatusCode) continue;

                string? finalPath = resp.RequestMessage?.RequestUri?.AbsolutePath;
                if (!string.IsNullOrWhiteSpace(finalPath))
                {
                    var m = TagInUrlRegex.Match(finalPath);
                    if (m.Success)
                    {
                        string fromUrl = Uri.UnescapeDataString(m.Groups["tag"].Value).Trim('/');
                        if (!string.IsNullOrWhiteSpace(fromUrl))
                            return fromUrl;
                    }
                }

                await using var stream = await resp.Content.ReadAsStreamAsync(ct);
                using var doc = await JsonDocument.ParseAsync(stream, cancellationToken: ct);
                if (doc.RootElement.TryGetProperty("tag_name", out var tagEl) && tagEl.ValueKind == JsonValueKind.String)
                {
                    string? tag = tagEl.GetString()?.Trim();
                    if (!string.IsNullOrWhiteSpace(tag)) return tag;
                }
            }
            catch (OperationCanceledException) { throw; }
            catch { }
        }
        return null;
    }

    private static string StripHtmlToPlainText(string html)
    {
        if (string.IsNullOrWhiteSpace(html)) return "";
        string s = HtmlBreakRegex.Replace(html, "\n");
        s = HtmlTagRegex.Replace(s, "");
        s = WebUtility.HtmlDecode(s);
        var lines = s.Split('\n').Select(l => l.Trim()).Where(l => l.Length > 0);
        return string.Join("\n", lines);
    }

    private static string TrimNotes(string notes)
    {
        string clean = (notes ?? "").Trim();
        return clean.Length > 1600 ? clean[..1600] + "\n…" : clean;
    }

    private static string? FindAssetNameInChecksums(string text, string suffix)
    {
        foreach (string rawLine in text.Split('\n'))
        {
            string line = rawLine.Trim();
            if (line.Length < 66) continue;
            var parts = line.Split([' ', '\t'], StringSplitOptions.RemoveEmptyEntries);
            if (parts.Length >= 2)
            {
                string name = parts[1].TrimStart('*');
                if (name.EndsWith(suffix, StringComparison.OrdinalIgnoreCase))
                    return name;
            }
        }
        return null;
    }

    private static async Task<long> ProbeContentLengthAsync(string officialUrl, string mirrorPrefix, CancellationToken ct)
    {
        foreach (string url in BuildCandidateUrls(officialUrl, mirrorPrefix))
        {
            try
            {
                using var req = new HttpRequestMessage(HttpMethod.Head, url);
                using var resp = await Http.SendAsync(req, HttpCompletionOption.ResponseHeadersRead, ct);
                if (resp.IsSuccessStatusCode && resp.Content.Headers.ContentLength is long len && len > 0)
                    return len;
            }
            catch (OperationCanceledException) { throw; }
            catch { }
        }
        return 0;
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
    try {{ $_.Path -and $_.Path.StartsWith($install, [StringComparison.OrdinalIgnoreCase]) }} catch {{ $false }}
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
        [JsonPropertyName("draft")] public bool Draft { get; set; }
        [JsonPropertyName("assets")] public List<GitHubAssetDto>? Assets { get; set; }
    }

    private sealed class GitHubAssetDto
    {
        [JsonPropertyName("name")] public string? Name { get; set; }
        [JsonPropertyName("size")] public long Size { get; set; }
        [JsonPropertyName("browser_download_url")] public string? BrowserDownloadUrl { get; set; }
    }
}
