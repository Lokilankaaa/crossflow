import AppKit
import CryptoKit
import Foundation
import Observation

struct UpdateReleaseInfo: Equatable, Sendable {
    let version: String
    let tagName: String
    let htmlUrl: String
    let notes: String
    let assetName: String
    let downloadUrl: String
    let assetSize: Int64
    let checksumsUrl: String?
    let isPrerelease: Bool
}

enum UpdaterPhase: Equatable, Sendable {
    case idle
    case checking
    case upToDate
    case available
    case downloading
    case verifying
    case applying
    case error
}

struct MirrorPreset: Identifiable, Equatable, Sendable {
    let id: String
    let label: String
    let desc: String
    let prefix: String?
}

private struct UpdaterSettingsFile: Codable, Sendable {
    var mirrorPrefix: String = ""
    var includePrerelease: Bool = false

    enum CodingKeys: String, CodingKey {
        case mirrorPrefix = "mirror_prefix"
        case includePrerelease = "include_prerelease"
    }
}

private struct GitHubReleaseDTO: Decodable, Sendable {
    let tagName: String?
    let htmlUrl: String?
    let body: String?
    let draft: Bool?
    let assets: [GitHubAssetDTO]?

    enum CodingKeys: String, CodingKey {
        case tagName = "tag_name"
        case htmlUrl = "html_url"
        case body
        case draft
        case assets
    }
}

private struct GitHubAssetDTO: Decodable, Sendable {
    let name: String?
    let size: Int64?
    let browserDownloadUrl: String?

    enum CodingKeys: String, CodingKey {
        case name
        case size
        case browserDownloadUrl = "browser_download_url"
    }
}

private struct AtomReleaseEntry: Sendable {
    let tagName: String
    let notes: String
    let htmlUrl: String
}

/// GitHub Releases self-updater with Atom feed + web redirect + REST API fallback,
/// optional HTTPS GitHub mirror prefix with automatic direct fallback, stable / prerelease (-rc)
/// channel switching, mandatory SHA-256 verification, codesign verification, and rollback-safe swap.
@MainActor @Observable
final class AppUpdater {
    nonisolated static let repoBaseURL = "https://github.com/Lokilankaaa/crossflow"
    nonisolated static let releasesAtomURL = "https://github.com/Lokilankaaa/crossflow/releases.atom"
    nonisolated static let latestReleaseWebURL = "https://github.com/Lokilankaaa/crossflow/releases/latest"
    nonisolated static let releasesPageURL = "https://github.com/Lokilankaaa/crossflow/releases"
    nonisolated static let issuesNewURL = "https://github.com/Lokilankaaa/crossflow/issues/new/choose"
    nonisolated static let authorGitHubURL = "https://github.com/Lokilankaaa"
    nonisolated static let authorBilibiliURL = "https://b23.tv/pANRtSc"

    private nonisolated static let latestReleaseAPI = URL(string: "https://api.github.com/repos/Lokilankaaa/crossflow/releases/latest")!
    private nonisolated static let releasesListAPI = URL(string: "https://api.github.com/repos/Lokilankaaa/crossflow/releases?per_page=10")!
    private nonisolated static let allowedHosts = [
        "api.github.com",
        "github.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com"
    ]

    nonisolated static let mirrorPresets: [MirrorPreset] = [
        MirrorPreset(id: "direct", label: "GitHub 官方直连", desc: "默认直连 github.com，适合海外或代理网络环境", prefix: ""),
        MirrorPreset(id: "ghproxy", label: "GH-Proxy · gh-proxy.com", desc: "国内常用 GitHub Releases 加速节点，失败自动回退直连", prefix: "https://gh-proxy.com/"),
        MirrorPreset(id: "ghfast", label: "GHFast · ghfast.top", desc: "国内备用 GitHub 下载加速镜像节点，失败自动回退直连", prefix: "https://ghfast.top/"),
        MirrorPreset(id: "custom", label: "自定义镜像前缀…", desc: "使用自建或自定义 HTTPS 加速前缀", prefix: nil)
    ]

    let currentVersion: String
    var phase: UpdaterPhase = .idle
    var release: UpdateReleaseInfo?
    var progress: Double = 0
    var message: String
    var mirrorPrefix: String
    var includePrerelease: Bool
    var showAboutSheet: Bool = false
    var activeAboutTab: String = "updates"

    @ObservationIgnored private var downloadTask: Task<Bool, Never>?
    @ObservationIgnored private var startupChecked = false

    nonisolated static var userDataDirURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/Crossflow", isDirectory: true)
    }

    nonisolated static var logsDirURL: URL {
        userDataDirURL.appendingPathComponent("Logs", isDirectory: true)
    }

    private nonisolated static var settingsURL: URL {
        userDataDirURL.appendingPathComponent("updater-settings.json")
    }

    init() {
        let ver = Self.resolveCurrentVersion()
        let saved = Self.loadSettings()
        self.currentVersion = ver
        self.mirrorPrefix = saved.mirrorPrefix
        self.includePrerelease = saved.includePrerelease
        self.message = "当前版本 v\(ver)"
    }

    func openAbout(tab: String = "updates") {
        activeAboutTab = tab
        showAboutSheet = true
    }

    func describeMirror() -> String {
        Self.describeMirror(mirrorPrefix)
    }

    nonisolated static func describeMirror(_ rawPrefix: String) -> String {
        let clean = normalizeMirrorPrefix(rawPrefix)
        if clean.isEmpty { return "GitHub 官方直连" }
        if let match = mirrorPresets.first(where: { ($0.prefix ?? "").caseInsensitiveCompare(clean) == .orderedSame }) {
            return match.label
        }
        return clean.hasSuffix("/") ? String(clean.dropLast()) : clean
    }

    nonisolated static func resolveMirrorPresetID(_ rawPrefix: String) -> String {
        let clean = normalizeMirrorPrefix(rawPrefix)
        if clean.isEmpty { return "direct" }
        if let match = mirrorPresets.first(where: { ($0.prefix ?? "").caseInsensitiveCompare(clean) == .orderedSame }) {
            return match.id
        }
        return "custom"
    }

    nonisolated static func normalizeMirrorPrefix(_ raw: String) -> String {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let url = URL(string: trimmed),
              url.scheme?.lowercased() == "https",
              let host = url.host, !host.isEmpty else { return "" }
        return trimmed.hasSuffix("/") ? trimmed : trimmed + "/"
    }

    nonisolated static func buildCandidateURLs(officialURL: String, mirrorPrefix: String) -> [String] {
        var result: [String] = []
        let cleanMirror = normalizeMirrorPrefix(mirrorPrefix)
        if !cleanMirror.isEmpty && isAllowedHttpsUrl(officialURL) {
            result.append(cleanMirror + officialURL)
        }
        result.append(officialURL)
        return result
    }

    func setMirrorPrefix(_ raw: String) -> String? {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            mirrorPrefix = ""
        } else {
            let normalized = Self.normalizeMirrorPrefix(trimmed)
            if normalized.isEmpty {
                return "镜像前缀必须为有效的 https:// 地址，例如 https://gh-proxy.com/"
            }
            mirrorPrefix = normalized
        }
        saveSettings()
        return nil
    }

    func setIncludePrerelease(_ enabled: Bool) {
        guard includePrerelease != enabled else { return }
        includePrerelease = enabled
        saveSettings()
    }

    private func saveSettings() {
        let cfg = UpdaterSettingsFile(mirrorPrefix: mirrorPrefix, includePrerelease: includePrerelease)
        let url = Self.settingsURL
        do {
            try FileManager.default.createDirectory(at: Self.userDataDirURL, withIntermediateDirectories: true)
            let enc = JSONEncoder()
            enc.outputFormatting = [.prettyPrinted, .sortedKeys]
            let data = try enc.encode(cfg)
            try data.write(to: url, options: .atomic)
        } catch {}
    }

    private nonisolated static func loadSettings() -> UpdaterSettingsFile {
        guard let data = try? Data(contentsOf: settingsURL),
              var cfg = try? JSONDecoder().decode(UpdaterSettingsFile.self, from: data) else {
            return UpdaterSettingsFile()
        }
        cfg.mirrorPrefix = normalizeMirrorPrefix(cfg.mirrorPrefix)
        return cfg
    }

    func scheduleSilentStartupCheck() {
        guard !startupChecked else { return }
        startupChecked = true
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(8))
            await self?.checkForUpdates(silent: true)
        }
    }

    func checkForUpdates(silent: Bool = false) async {
        switch phase {
        case .checking, .downloading, .verifying, .applying:
            return
        default:
            break
        }

        let includePre = includePrerelease
        let mirror = mirrorPrefix

        if !silent {
            phase = .checking
            progress = 0
            message = "正在检查 GitHub 最新版本…"
        }

        do {
            // 1. Primary: GitHub Releases Atom feed (zero API rate limit, works via mirror,
            //    supports stable + prerelease channels + release notes)
            let atomHit = await fetchLatestFromAtom(includePrerelease: includePre, mirrorPrefix: mirror)

            var resolvedTag = atomHit?.tagName
            var notes = atomHit?.notes ?? ""
            var htmlUrl = atomHit?.htmlUrl
            var apiDTO: GitHubReleaseDTO?

            // 2. Fallback (stable): GitHub /releases/latest web redirect/JSON
            if resolvedTag == nil && !includePre {
                resolvedTag = await fetchLatestTagFromWeb(mirrorPrefix: mirror)
            }

            // 3. Final fallback: GitHub REST API
            if resolvedTag == nil {
                var req = URLRequest(url: includePre ? Self.releasesListAPI : Self.latestReleaseAPI, timeoutInterval: 15)
                req.setValue("Crossflow-Updater/\(currentVersion)", forHTTPHeaderField: "User-Agent")
                req.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")

                let (data, response) = try await URLSession.shared.data(for: req)
                guard let http = response as? HTTPURLResponse else {
                    if !silent {
                        phase = .error
                        message = "无法获取 GitHub 响应。"
                    }
                    return
                }
                guard (200...299).contains(http.statusCode) else {
                    if !silent {
                        phase = .error
                        message = "检查更新失败（HTTP \(http.statusCode)），可切换下载加速线路后重试"
                    }
                    return
                }
                if includePre {
                    let list = try JSONDecoder().decode([GitHubReleaseDTO].self, from: data)
                    let valid = list.filter { ($0.draft ?? false) == false && ($0.tagName?.lowercased().hasPrefix("v") ?? false) }
                    apiDTO = valid.max { a, b in
                        Self.compareVersions(a.tagName ?? "", b.tagName ?? "") < 0
                    }
                } else {
                    apiDTO = try JSONDecoder().decode(GitHubReleaseDTO.self, from: data)
                }
                resolvedTag = apiDTO?.tagName?.trimmingCharacters(in: .whitespacesAndNewlines)
                notes = apiDTO?.body ?? notes
                htmlUrl = apiDTO?.htmlUrl ?? htmlUrl
            }

            guard let rawTag = resolvedTag, !rawTag.isEmpty else {
                if !silent {
                    phase = .error
                    message = "无法解析最新版本信息，请检查网络或切换镜像线路。"
                }
                return
            }

            let latestVer = rawTag.trimmingCharacters(in: CharacterSet(charactersIn: "vV"))
            guard Self.isNewerVersion(latestVer, than: currentVersion) else {
                if !silent {
                    release = nil
                    phase = .upToDate
                    progress = 0
                    message = "已是最新版本（v\(currentVersion)）"
                }
                return
            }

            let isPre = Self.isPrereleaseTag(rawTag)
            let info: UpdateReleaseInfo?
            if let dto = apiDTO {
                let assets = dto.assets ?? []
                if let macAsset = assets.first(where: { asset in
                    guard let name = asset.name, let url = asset.browserDownloadUrl else { return false }
                    return name.lowercased().hasSuffix("-macos-arm64.zip") && Self.isAllowedHttpsUrl(url)
                }),
                let assetName = macAsset.name,
                let assetURL = macAsset.browserDownloadUrl {
                    let checksumsAsset = assets.first { asset in
                        guard let name = asset.name, let url = asset.browserDownloadUrl else { return false }
                        return name.caseInsensitiveCompare("SHA256SUMS.txt") == .orderedSame && Self.isAllowedHttpsUrl(url)
                    }
                    let safeHtml = (dto.htmlUrl.flatMap { Self.isAllowedHttpsUrl($0) ? $0 : nil })
                        ?? "\(Self.repoBaseURL)/releases/tag/\(rawTag)"
                    info = UpdateReleaseInfo(
                        version: latestVer,
                        tagName: rawTag,
                        htmlUrl: safeHtml,
                        notes: Self.trimNotes(notes),
                        assetName: assetName,
                        downloadUrl: assetURL,
                        assetSize: macAsset.size ?? 0,
                        checksumsUrl: checksumsAsset?.browserDownloadUrl,
                        isPrerelease: isPre
                    )
                } else {
                    info = nil
                }
            } else {
                let escapedTag = rawTag.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? rawTag
                let checksumsUrl = "\(Self.repoBaseURL)/releases/download/\(escapedTag)/SHA256SUMS.txt"
                var assetName = "Crossflow-\(rawTag)-macos-arm64.zip"
                if let sumText = await fetchStringWithMirror(officialURL: checksumsUrl, mirrorPrefix: mirror),
                   let matched = Self.findAssetNameInChecksums(sumText, suffix: "-macos-arm64.zip") {
                    assetName = matched
                }
                let downloadUrl = "\(Self.repoBaseURL)/releases/download/\(escapedTag)/\(assetName)"
                let assetSize = await probeContentLength(officialURL: downloadUrl, mirrorPrefix: mirror)
                let safeHtml = (htmlUrl.flatMap { Self.isAllowedHttpsUrl($0) ? $0 : nil })
                    ?? "\(Self.repoBaseURL)/releases/tag/\(escapedTag)"
                info = UpdateReleaseInfo(
                    version: latestVer,
                    tagName: rawTag,
                    htmlUrl: safeHtml,
                    notes: Self.trimNotes(notes),
                    assetName: assetName,
                    downloadUrl: downloadUrl,
                    assetSize: assetSize,
                    checksumsUrl: checksumsUrl,
                    isPrerelease: isPre
                )
            }

            guard let resolvedInfo = info, Self.isAllowedHttpsUrl(resolvedInfo.downloadUrl) else {
                if !silent {
                    phase = .error
                    message = "发现 v\(latestVer)，但未找到 macOS 安装包。"
                }
                return
            }

            let preBadge = resolvedInfo.isPrerelease ? " RC" : ""
            let sizeText = resolvedInfo.assetSize > 0
                ? String(format: "（%.1f MB）", Double(resolvedInfo.assetSize) / (1024.0 * 1024.0))
                : ""
            release = resolvedInfo
            phase = .available
            progress = 0
            message = "发现新版本 v\(resolvedInfo.version)\(preBadge)\(sizeText)"
        } catch is CancellationError {
            if !silent {
                phase = .error
                message = "检查更新已取消。"
            }
        } catch {
            if !silent {
                phase = .error
                message = "检查更新失败：\(error.localizedDescription)"
            }
        }
    }

    func cancelDownload() {
        downloadTask?.cancel()
    }

    func openReleaseNotes() {
        let urlStr = release?.htmlUrl ?? Self.releasesPageURL
        openExternalURL(urlStr)
    }

    func openExternalURL(_ raw: String) {
        guard let url = URL(string: raw),
              url.scheme?.lowercased() == "https" else { return }
        NSWorkspace.shared.open(url)
    }

    func openFolder(_ url: URL) {
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        NSWorkspace.shared.open(url)
    }

    func downloadAndApply(stopBeforeReplace: @escaping @MainActor () async -> Void) async -> Bool {
        guard let info = release else { return false }
        switch phase {
        case .downloading, .verifying, .applying:
            return false
        default:
            break
        }

        let targetAppURL = Bundle.main.bundleURL.standardizedFileURL
        guard targetAppURL.pathExtension == "app" else {
            phase = .error
            message = "当前以命令行模式运行，请从 Crossflow.app 启动后再使用自更新。"
            return false
        }
        let parentDir = targetAppURL.deletingLastPathComponent()
        guard Self.canWriteDirectory(parentDir) else {
            phase = .error
            message = "当前应用目录不可写，请将 Crossflow.app 移至“应用程序”后再更新，或点击“更新日志”手动下载。"
            return false
        }

        let task = Task<Bool, Never> { @MainActor in
            await self.runDownloadAndApply(info: info, targetAppURL: targetAppURL, stopBeforeReplace: stopBeforeReplace)
        }
        downloadTask = task
        let result = await task.value
        downloadTask = nil
        return result
    }

    private func runDownloadAndApply(
        info: UpdateReleaseInfo,
        targetAppURL: URL,
        stopBeforeReplace: @escaping @MainActor () async -> Void
    ) async -> Bool {
        let fm = FileManager.default
        let mirror = mirrorPrefix
        let workDir = fm.temporaryDirectory
            .appendingPathComponent("CrossflowUpdate", isDirectory: true)
            .appendingPathComponent("v\(info.version)-\(UUID().uuidString)", isDirectory: true)
        let zipURL = workDir.appendingPathComponent(info.assetName)
        let stagingDir = workDir.appendingPathComponent("staging", isDirectory: true)

        do {
            try fm.createDirectory(at: workDir, withIntermediateDirectories: true)
            phase = .downloading
            progress = 0
            message = "正在下载 v\(info.version)…"

            guard let checksumsStr = info.checksumsUrl,
                  Self.isAllowedHttpsUrl(checksumsStr),
                  let sumsText = await fetchStringWithMirror(officialURL: checksumsStr, mirrorPrefix: mirror),
                  let expectedSha = Self.parseExpectedSha256(sumsText, assetName: info.assetName) else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "SHA256SUMS.txt 获取或解析失败，已取消更新。"
                return false
            }

            guard Self.isAllowedHttpsUrl(info.downloadUrl) else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "更新包下载地址不受信任，已取消更新。"
                return false
            }

            let candidates = Self.buildCandidateURLs(officialURL: info.downloadUrl, mirrorPrefix: mirror)
            var verifiedOk = false
            var lastErrMsg: String?

            for (idx, candidateStr) in candidates.enumerated() {
                try Task.checkCancellation()
                guard let dlURL = URL(string: candidateStr) else { continue }
                let viaMirror = idx == 0 && candidates.count > 1
                let route = viaMirror ? "镜像" : "直连"

                do {
                    if fm.fileExists(atPath: zipURL.path) {
                        try? fm.removeItem(at: zipURL)
                    }
                    var dlReq = URLRequest(url: dlURL, timeoutInterval: 600)
                    dlReq.setValue("Crossflow-Updater/\(currentVersion)", forHTTPHeaderField: "User-Agent")
                    let expectedLen = info.assetSize
                    let versionLabel = info.version
                    try await Self.downloadFile(
                        request: dlReq,
                        destination: zipURL,
                        fallbackTotal: expectedLen
                    ) { [weak self] downloaded, total in
                        Task { @MainActor [weak self] in
                            self?.updateDownloadProgress(downloaded: downloaded, total: total, version: versionLabel, route: route)
                        }
                    }

                    try Task.checkCancellation()
                    phase = .verifying
                    progress = 1.0
                    message = "正在校验 SHA-256 完整性…"

                    let actualSha = try await Task.detached(priority: .userInitiated) {
                        try Self.computeFileSha256(url: zipURL)
                    }.value

                    if actualSha.caseInsensitiveCompare(expectedSha) == .orderedSame {
                        verifiedOk = true
                        break
                    }
                    lastErrMsg = "安装包 SHA-256 校验失败"
                    if viaMirror {
                        phase = .downloading
                        progress = 0
                        message = "镜像校验异常，正在自动回退 GitHub 官方直连…"
                    }
                } catch is CancellationError {
                    throw CancellationError()
                } catch {
                    lastErrMsg = error.localizedDescription
                    if viaMirror {
                        phase = .downloading
                        progress = 0
                        message = "镜像线路不可用，正在自动回退 GitHub 官方直连…"
                    }
                }
            }

            guard verifiedOk else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "更新包下载或校验失败：\(lastErrMsg ?? "网络异常")"
                return false
            }

            try Task.checkCancellation()
            phase = .applying
            progress = 1.0
            message = "正在解压并验证应用签名…"

            try fm.createDirectory(at: stagingDir, withIntermediateDirectories: true)
            let extractStatus = try await Task.detached(priority: .userInitiated) {
                try Self.runTool("/usr/bin/ditto", arguments: ["-x", "-k", zipURL.path, stagingDir.path])
            }.value
            guard extractStatus == 0 else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "解压更新包失败（退出码 \(extractStatus)）。"
                return false
            }

            guard let stagedApp = Self.locateAppBundle(in: stagingDir) else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "更新包中未找到 Crossflow.app，已取消更新。"
                return false
            }

            let macBinary = stagedApp.appendingPathComponent("Contents/MacOS/CrossflowMac")
            let coreBinary = stagedApp.appendingPathComponent("Contents/MacOS/crossflow")
            guard fm.isExecutableFile(atPath: macBinary.path), fm.isExecutableFile(atPath: coreBinary.path) else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "更新包结构不完整，已取消更新。"
                return false
            }

            _ = try? await Task.detached(priority: .userInitiated) {
                try Self.runTool("/usr/bin/xattr", arguments: ["-dr", "com.apple.quarantine", stagedApp.path])
            }.value

            let signStatus = try await Task.detached(priority: .userInitiated) {
                try Self.runTool("/usr/bin/codesign", arguments: ["--verify", "--deep", "--strict", stagedApp.path])
            }.value
            guard signStatus == 0 else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "更新包签名校验未通过，已取消更新。"
                return false
            }

            message = "正在替换应用并重启…"
            let scriptURL = workDir.appendingPathComponent("apply-update.sh")
            try Self.writeApplyScript(
                to: scriptURL,
                pid: ProcessInfo.processInfo.processIdentifier,
                stagedAppPath: stagedApp.path,
                targetAppPath: targetAppURL.path,
                workDirPath: workDir.path
            )

            await stopBeforeReplace()

            let helper = Process()
            helper.executableURL = URL(fileURLWithPath: "/bin/sh")
            helper.arguments = [scriptURL.path]
            helper.standardInput = FileHandle.nullDevice
            helper.standardOutput = FileHandle.nullDevice
            helper.standardError = FileHandle.nullDevice
            try helper.run()
            return true
        } catch is CancellationError {
            try? fm.removeItem(at: workDir)
            phase = .available
            progress = 0
            message = "已取消下载，可随时更新至 v\(info.version)"
            return false
        } catch {
            try? fm.removeItem(at: workDir)
            phase = .error
            progress = 0
            message = "更新失败：\(error.localizedDescription)"
            return false
        }
    }

    private func updateDownloadProgress(downloaded: Int64, total: Int64, version: String, route: String) {
        let pct = total > 0 ? min(max(Double(downloaded) / Double(total), 0), 1) : 0
        progress = pct
        let mb = String(format: "%.1f", Double(downloaded) / (1024.0 * 1024.0))
        if total > 0 {
            let totalMb = String(format: "%.1f", Double(total) / (1024.0 * 1024.0))
            message = String(format: "正在下载 v%@（%@）… %.0f%%（%@ / %@ MB）", version, route, pct * 100.0, mb, totalMb)
        } else {
            message = "正在下载 v\(version)（\(route)）…（\(mb) MB）"
        }
    }

    private func fetchStringWithMirror(officialURL: String, mirrorPrefix: String) async -> String? {
        for candidate in Self.buildCandidateURLs(officialURL: officialURL, mirrorPrefix: mirrorPrefix) {
            guard let url = URL(string: candidate) else { continue }
            var req = URLRequest(url: url, timeoutInterval: 15)
            req.setValue("Crossflow-Updater/\(currentVersion)", forHTTPHeaderField: "User-Agent")
            if let (data, resp) = try? await URLSession.shared.data(for: req),
               let http = resp as? HTTPURLResponse,
               (200...299).contains(http.statusCode),
               let text = String(data: data, encoding: .utf8) {
                return text
            }
        }
        return nil
    }

    private func fetchLatestFromAtom(includePrerelease: Bool, mirrorPrefix: String) async -> AtomReleaseEntry? {
        guard let xml = await fetchStringWithMirror(officialURL: Self.releasesAtomURL, mirrorPrefix: mirrorPrefix),
              let data = xml.data(using: .utf8) else { return nil }
        return Self.parseReleasesAtom(data: data, includePrerelease: includePrerelease)
    }

    private nonisolated static func parseReleasesAtom(data: Data, includePrerelease: Bool) -> AtomReleaseEntry? {
        final class ParserDelegate: NSObject, XMLParserDelegate {
            var entries: [(id: String, link: String, content: String)] = []
            private var inEntry = false
            private var currentTag = ""
            private var curId = ""
            private var curLink = ""
            private var curContent = ""

            func parser(_ parser: XMLParser, didStartElement elementName: String, namespaceURI: String?, qualifiedName qName: String?, attributes attributeDict: [String : String] = [:]) {
                if elementName == "entry" {
                    inEntry = true
                    curId = ""
                    curLink = ""
                    curContent = ""
                } else if inEntry {
                    currentTag = elementName
                    if elementName == "link", let href = attributeDict["href"], href.lowercased().contains("/releases/tag/") {
                        curLink = href
                    }
                }
            }

            func parser(_ parser: XMLParser, foundCharacters string: String) {
                guard inEntry else { return }
                if currentTag == "id" { curId += string }
                else if currentTag == "content" { curContent += string }
            }

            func parser(_ parser: XMLParser, didEndElement elementName: String, namespaceURI: String?, qualifiedName qName: String?) {
                if elementName == "entry" {
                    entries.append((curId.trimmingCharacters(in: .whitespacesAndNewlines), curLink, curContent))
                    inEntry = false
                }
                if elementName == currentTag { currentTag = "" }
            }
        }

        let delegate = ParserDelegate()
        let parser = XMLParser(data: data)
        parser.delegate = delegate
        guard parser.parse() else { return nil }

        var best: AtomReleaseEntry?
        for item in delegate.entries {
            var tag = ""
            if let range = item.link.range(of: "/releases/tag/", options: .caseInsensitive) {
                let raw = String(item.link[range.upperBound...])
                    .split(whereSeparator: { $0 == "?" || $0 == "#" || $0 == "/" })
                    .first.map(String.init) ?? ""
                tag = raw.removingPercentEncoding ?? raw
            } else if let lastSlash = item.id.lastIndex(of: "/") {
                tag = String(item.id[item.id.index(after: lastSlash)...])
            }
            guard !tag.isEmpty, tag.lowercased().hasPrefix("v") else { continue }
            if isPrereleaseTag(tag) && !includePrerelease { continue }
            let notes = stripHtmlToPlainText(item.content)
            let htmlUrl = isAllowedHttpsUrl(item.link) ? item.link : "\(repoBaseURL)/releases/tag/\(tag)"
            if best == nil || compareVersions(tag, best!.tagName) > 0 {
                best = AtomReleaseEntry(tagName: tag, notes: notes, htmlUrl: htmlUrl)
            }
        }
        return best
    }

    private nonisolated static func stripHtmlToPlainText(_ html: String) -> String {
        guard !html.isEmpty else { return "" }
        var s = html
        if let brRe = try? NSRegularExpression(pattern: "<\\s*(br|p|div|li|h[1-6]|tr)\\b[^>]*>", options: .caseInsensitive) {
            s = brRe.stringByReplacingMatches(in: s, range: NSRange(s.startIndex..., in: s), withTemplate: "\n")
        }
        if let tagRe = try? NSRegularExpression(pattern: "<[^>]+>") {
            s = tagRe.stringByReplacingMatches(in: s, range: NSRange(s.startIndex..., in: s), withTemplate: "")
        }
        s = s.replacingOccurrences(of: "&amp;", with: "&")
            .replacingOccurrences(of: "&lt;", with: "<")
            .replacingOccurrences(of: "&gt;", with: ">")
            .replacingOccurrences(of: "&quot;", with: "\"")
            .replacingOccurrences(of: "&#39;", with: "'")
            .replacingOccurrences(of: "&nbsp;", with: " ")
        let lines = s.split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
        return lines.joined(separator: "\n")
    }

    private nonisolated static func trimNotes(_ notes: String) -> String {
        let clean = notes.trimmingCharacters(in: .whitespacesAndNewlines)
        if clean.count > 1600 {
            return String(clean.prefix(1600)) + "\n…"
        }
        return clean
    }

    private func fetchLatestTagFromWeb(mirrorPrefix: String) async -> String? {
        for candidate in Self.buildCandidateURLs(officialURL: Self.latestReleaseWebURL, mirrorPrefix: mirrorPrefix) {
            guard let url = URL(string: candidate) else { continue }
            var req = URLRequest(url: url, timeoutInterval: 12)
            req.setValue("Crossflow-Updater/\(currentVersion)", forHTTPHeaderField: "User-Agent")
            req.setValue("application/json", forHTTPHeaderField: "Accept")
            guard let (data, response) = try? await URLSession.shared.data(for: req),
                  let http = response as? HTTPURLResponse,
                  (200...299).contains(http.statusCode) else { continue }

            if let finalPath = http.url?.path,
               let range = finalPath.range(of: "/releases/tag/", options: .caseInsensitive) {
                let fromURL = String(finalPath[range.upperBound...]).trimmingCharacters(in: CharacterSet(charactersIn: "/"))
                if !fromURL.isEmpty { return fromURL }
            }
            if let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
               let tag = (json["tag_name"] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !tag.isEmpty {
                return tag
            }
        }
        return nil
    }

    private func probeContentLength(officialURL: String, mirrorPrefix: String) async -> Int64 {
        for candidate in Self.buildCandidateURLs(officialURL: officialURL, mirrorPrefix: mirrorPrefix) {
            guard let url = URL(string: candidate) else { continue }
            var req = URLRequest(url: url, timeoutInterval: 10)
            req.httpMethod = "HEAD"
            req.setValue("Crossflow-Updater/\(currentVersion)", forHTTPHeaderField: "User-Agent")
            if let (_, resp) = try? await URLSession.shared.data(for: req),
               let http = resp as? HTTPURLResponse,
               (200...299).contains(http.statusCode),
               http.expectedContentLength > 0 {
                return http.expectedContentLength
            }
        }
        return 0
    }

    private nonisolated static func resolveCurrentVersion() -> String {
        if let raw = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String {
            let clean = raw.trimmingCharacters(in: .whitespacesAndNewlines)
                .trimmingCharacters(in: CharacterSet(charactersIn: "vV"))
            if parseVersion(clean) != nil {
                return clean
            }
        }
        return "0.4.1"
    }

    nonisolated static func isPrereleaseTag(_ raw: String) -> Bool {
        let lower = raw.lowercased()
        return lower.contains("-rc") || lower.contains("-beta") || lower.contains("-alpha")
    }

    nonisolated static func parseVersion(_ raw: String) -> (Int, Int, Int, String)? {
        var s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            .trimmingCharacters(in: CharacterSet(charactersIn: "vV"))
        if let plus = s.firstIndex(of: "+") {
            s = String(s[..<plus])
        }
        var pre = ""
        if let dash = s.firstIndex(of: "-") {
            pre = String(s[s.index(after: dash)...]).trimmingCharacters(in: .whitespaces)
            s = String(s[..<dash])
        }
        let parts = s.split(separator: ".")
        guard parts.count >= 2 && parts.count <= 4,
              let maj = Int(parts[0]), let min = Int(parts[1]),
              maj >= 0, min >= 0 else { return nil }
        let pat: Int
        if parts.count >= 3 {
            guard let p = Int(parts[2]), p >= 0 else { return nil }
            pat = p
        } else {
            pat = 0
        }
        return (maj, min, pat, pre)
    }

    nonisolated static func compareVersions(_ candidate: String, _ current: String) -> Int {
        guard let c = parseVersion(candidate), let cur = parseVersion(current) else { return 0 }
        if c.0 != cur.0 { return c.0 < cur.0 ? -1 : 1 }
        if c.1 != cur.1 { return c.1 < cur.1 ? -1 : 1 }
        if c.2 != cur.2 { return c.2 < cur.2 ? -1 : 1 }

        let cPre = !c.3.isEmpty
        let curPre = !cur.3.isEmpty
        if !cPre && !curPre { return 0 }
        if !cPre && curPre { return 1 }
        if cPre && !curPre { return -1 }

        let aParts = c.3.split(separator: ".").map(String.init)
        let bParts = cur.3.split(separator: ".").map(String.init)
        let count = max(aParts.count, bParts.count)
        for i in 0..<count {
            if i >= aParts.count { return -1 }
            if i >= bParts.count { return 1 }
            if let ai = Int(aParts[i]), let bi = Int(bParts[i]) {
                if ai != bi { return ai < bi ? -1 : 1 }
            } else {
                let cmp = aParts[i].caseInsensitiveCompare(bParts[i])
                if cmp != .orderedSame { return cmp == .orderedAscending ? -1 : 1 }
            }
        }
        return 0
    }

    nonisolated static func isNewerVersion(_ candidate: String, than current: String) -> Bool {
        compareVersions(candidate, current) > 0
    }

    private nonisolated static func isAllowedHttpsUrl(_ raw: String) -> Bool {
        guard let url = URL(string: raw),
              url.scheme?.lowercased() == "https",
              let host = url.host?.lowercased() else { return false }
        return allowedHosts.contains { allowed in
            host == allowed || host.hasSuffix(".\(allowed)")
        }
    }

    private nonisolated static func findAssetNameInChecksums(_ text: String, suffix: String) -> String? {
        let lowerSuffix = suffix.lowercased()
        for rawLine in text.split(whereSeparator: \.isNewline) {
            let parts = rawLine.split(whereSeparator: { $0 == " " || $0 == "\t" })
            guard parts.count >= 2 else { continue }
            let name = String(parts[1]).trimmingCharacters(in: CharacterSet(charactersIn: "*"))
            if name.lowercased().hasSuffix(lowerSuffix) {
                return name
            }
        }
        return nil
    }

    private nonisolated static func parseExpectedSha256(_ text: String, assetName: String) -> String? {
        for rawLine in text.split(whereSeparator: \.isNewline) {
            let parts = rawLine.split(whereSeparator: { $0 == " " || $0 == "\t" })
            guard parts.count >= 2 else { continue }
            let hash = String(parts[0]).trimmingCharacters(in: .whitespacesAndNewlines)
            let name = String(parts[1]).trimmingCharacters(in: CharacterSet(charactersIn: "*"))
            if name.caseInsensitiveCompare(assetName) == .orderedSame,
               hash.count == 64,
               hash.allSatisfy({ $0.isHexDigit }) {
                return hash.lowercased()
            }
        }
        return nil
    }

    private nonisolated static func downloadFile(
        request: URLRequest,
        destination: URL,
        fallbackTotal: Int64,
        onProgress: @escaping @Sendable (Int64, Int64) -> Void
    ) async throws {
        final class Delegate: NSObject, URLSessionDownloadDelegate, @unchecked Sendable {
            private let lock = NSLock()
            private let destination: URL
            private let fallbackTotal: Int64
            private let onProgress: @Sendable (Int64, Int64) -> Void
            private var continuation: CheckedContinuation<Void, Error>?
            private var lastReport = Date.distantPast
            private var finished = false
            private var cancelled = false
            weak var task: URLSessionDownloadTask?

            init(destination: URL, fallbackTotal: Int64, onProgress: @escaping @Sendable (Int64, Int64) -> Void) {
                self.destination = destination
                self.fallbackTotal = fallbackTotal
                self.onProgress = onProgress
            }

            func attach(_ cont: CheckedContinuation<Void, Error>, task: URLSessionDownloadTask) {
                lock.lock()
                self.continuation = cont
                self.task = task
                let alreadyCancelled = self.cancelled
                lock.unlock()
                if alreadyCancelled {
                    task.cancel()
                } else {
                    task.resume()
                }
            }

            func cancel() {
                lock.lock()
                self.cancelled = true
                let t = task
                lock.unlock()
                t?.cancel()
            }

            private func complete(_ result: Result<Void, Error>, session: URLSession) {
                lock.lock()
                if finished {
                    lock.unlock()
                    return
                }
                finished = true
                let cont = continuation
                continuation = nil
                lock.unlock()
                session.finishTasksAndInvalidate()
                cont?.resume(with: result)
            }

            func urlSession(
                _ session: URLSession,
                downloadTask: URLSessionDownloadTask,
                didWriteData bytesWritten: Int64,
                totalBytesWritten: Int64,
                totalBytesExpectedToWrite: Int64
            ) {
                let total = totalBytesExpectedToWrite > 0 ? totalBytesExpectedToWrite : fallbackTotal
                lock.lock()
                let now = Date()
                let shouldReport = now.timeIntervalSince(lastReport) >= 0.12 || (total > 0 && totalBytesWritten >= total)
                if shouldReport { lastReport = now }
                lock.unlock()
                if shouldReport {
                    onProgress(totalBytesWritten, total)
                }
            }

            func urlSession(_ session: URLSession, downloadTask: URLSessionDownloadTask, didFinishDownloadingTo location: URL) {
                if let http = downloadTask.response as? HTTPURLResponse, !(200...299).contains(http.statusCode) {
                    complete(.failure(NSError(domain: "CrossflowUpdater", code: http.statusCode, userInfo: [NSLocalizedDescriptionKey: "下载失败（HTTP \(http.statusCode)）"])), session: session)
                    return
                }
                do {
                    let fm = FileManager.default
                    if fm.fileExists(atPath: destination.path) {
                        try fm.removeItem(at: destination)
                    }
                    try fm.moveItem(at: location, to: destination)
                    let size = (try? fm.attributesOfItem(atPath: destination.path)[.size] as? Int64) ?? fallbackTotal
                    onProgress(size, max(size, fallbackTotal))
                    complete(.success(()), session: session)
                } catch {
                    complete(.failure(error), session: session)
                }
            }

            func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
                if let error {
                    if (error as NSError).domain == NSURLErrorDomain && (error as NSError).code == NSURLErrorCancelled {
                        complete(.failure(CancellationError()), session: session)
                    } else {
                        complete(.failure(error), session: session)
                    }
                } else {
                    complete(.failure(NSError(domain: "CrossflowUpdater", code: -1, userInfo: [NSLocalizedDescriptionKey: "下载意外中止。"])), session: session)
                }
            }
        }

        let delegate = Delegate(destination: destination, fallbackTotal: fallbackTotal, onProgress: onProgress)
        let session = URLSession(configuration: .default, delegate: delegate, delegateQueue: nil)
        let downloadTask = session.downloadTask(with: request)

        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
                delegate.attach(cont, task: downloadTask)
            }
        } onCancel: {
            delegate.cancel()
        }
    }

    private nonisolated static func computeFileSha256(url: URL) throws -> String {
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        var hasher = SHA256()
        while true {
            let chunk = try handle.read(upToCount: 65536)
            guard let chunk, !chunk.isEmpty else { break }
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }

    private nonisolated static func runTool(_ path: String, arguments: [String]) throws -> Int32 {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: path)
        proc.arguments = arguments
        proc.standardInput = FileHandle.nullDevice
        proc.standardOutput = FileHandle.nullDevice
        proc.standardError = FileHandle.nullDevice
        try proc.run()
        proc.waitUntilExit()
        return proc.terminationStatus
    }

    private nonisolated static func locateAppBundle(in dir: URL) -> URL? {
        let direct = dir.appendingPathComponent("Crossflow.app", isDirectory: true)
        if FileManager.default.fileExists(atPath: direct.path) {
            return direct
        }
        guard let items = try? FileManager.default.contentsOfDirectory(
            at: dir,
            includingPropertiesForKeys: nil,
            options: [.skipsHiddenFiles]
        ) else { return nil }
        if let app = items.first(where: { $0.pathExtension == "app" }) {
            return app
        }
        for sub in items {
            let nested = sub.appendingPathComponent("Crossflow.app", isDirectory: true)
            if FileManager.default.fileExists(atPath: nested.path) {
                return nested
            }
        }
        return nil
    }

    private nonisolated static func canWriteDirectory(_ dir: URL) -> Bool {
        let probe = dir.appendingPathComponent(".crossflow-write-test-\(UUID().uuidString).tmp")
        do {
            try Data("ok".utf8).write(to: probe, options: .atomic)
            try? FileManager.default.removeItem(at: probe)
            return true
        } catch {
            return false
        }
    }

    private nonisolated static func shellQuote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\"'\"'") + "'"
    }

    private nonisolated static func writeApplyScript(
        to url: URL,
        pid: Int32,
        stagedAppPath: String,
        targetAppPath: String,
        workDirPath: String
    ) throws {
        let backupPath = (workDirPath as NSString).appendingPathComponent("Crossflow.backup.app")
        let script = """
        #!/bin/sh
        TARGET_PID=\(pid)
        STAGED_APP=\(shellQuote(stagedAppPath))
        TARGET_APP=\(shellQuote(targetAppPath))
        BACKUP_APP=\(shellQuote(backupPath))
        WORK_DIR=\(shellQuote(workDirPath))

        i=0
        while [ $i -lt 50 ]; do
            if ! kill -0 "$TARGET_PID" 2>/dev/null; then
                break
            fi
            sleep 0.2
            i=$((i + 1))
        done
        if kill -0 "$TARGET_PID" 2>/dev/null; then
            kill -9 "$TARGET_PID" 2>/dev/null || true
            sleep 0.3
        fi
        pkill -x crossflow 2>/dev/null || true
        sleep 0.2

        rm -rf "$BACKUP_APP" 2>/dev/null || true
        if [ -d "$TARGET_APP" ]; then
            if ! mv "$TARGET_APP" "$BACKUP_APP"; then
                /usr/bin/open -n "$TARGET_APP" 2>/dev/null || true
                rm -rf "$WORK_DIR" 2>/dev/null || true
                exit 1
            fi
        fi

        if /usr/bin/ditto "$STAGED_APP" "$TARGET_APP"; then
            /usr/bin/xattr -dr com.apple.quarantine "$TARGET_APP" 2>/dev/null || true
            /usr/bin/open -n "$TARGET_APP" 2>/dev/null || true
        else
            rm -rf "$TARGET_APP" 2>/dev/null || true
            if [ -d "$BACKUP_APP" ]; then
                mv "$BACKUP_APP" "$TARGET_APP" 2>/dev/null || true
            fi
            /usr/bin/open -n "$TARGET_APP" 2>/dev/null || true
        fi

        sleep 2
        rm -rf "$WORK_DIR" 2>/dev/null || true
        """
        try script.write(to: url, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: url.path)
    }
}
