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

private struct GitHubReleaseDTO: Decodable, Sendable {
    let tagName: String?
    let htmlUrl: String?
    let body: String?
    let assets: [GitHubAssetDTO]?

    enum CodingKeys: String, CodingKey {
        case tagName = "tag_name"
        case htmlUrl = "html_url"
        case body
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

/// GitHub Releases self-updater with mandatory SHA-256 verification, codesign verification,
/// rollback-safe atomic `.app` swap, and automatic relaunch.
@MainActor @Observable
final class AppUpdater {
    private static let latestReleaseAPI = URL(string: "https://api.github.com/repos/Lokilankaaa/crossflow/releases/latest")!
    private static let allowedHosts = [
        "api.github.com",
        "github.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com"
    ]

    let currentVersion: String
    var phase: UpdaterPhase = .idle
    var release: UpdateReleaseInfo?
    var progress: Double = 0
    var message: String

    @ObservationIgnored private var downloadTask: Task<Bool, Never>?
    @ObservationIgnored private var startupChecked = false

    init() {
        let ver = Self.resolveCurrentVersion()
        self.currentVersion = ver
        self.message = "当前版本 v\(ver)"
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

        if !silent {
            phase = .checking
            progress = 0
            message = "正在检查 GitHub 最新版本…"
        }

        do {
            var req = URLRequest(url: Self.latestReleaseAPI, timeoutInterval: 12)
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
                    message = "检查更新失败（HTTP \(http.statusCode)）"
                }
                return
            }

            let dto = try JSONDecoder().decode(GitHubReleaseDTO.self, from: data)
            guard let rawTag = dto.tagName?.trimmingCharacters(in: .whitespacesAndNewlines), !rawTag.isEmpty else {
                if !silent {
                    phase = .error
                    message = "无法解析最新版本信息。"
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

            let assets = dto.assets ?? []
            guard let macAsset = assets.first(where: { asset in
                guard let name = asset.name, let url = asset.browserDownloadUrl else { return false }
                return name.lowercased().hasSuffix("-macos-arm64.zip") && Self.isAllowedHttpsUrl(url)
            }),
            let assetName = macAsset.name,
            let assetURL = macAsset.browserDownloadUrl else {
                if !silent {
                    phase = .error
                    message = "发现 v\(latestVer)，但未找到 macOS 安装包。"
                }
                return
            }

            let checksumsAsset = assets.first { asset in
                guard let name = asset.name, let url = asset.browserDownloadUrl else { return false }
                return name.caseInsensitiveCompare("SHA256SUMS.txt") == .orderedSame && Self.isAllowedHttpsUrl(url)
            }

            let htmlUrl: String
            if let rawHtml = dto.htmlUrl, Self.isAllowedHttpsUrl(rawHtml) {
                htmlUrl = rawHtml
            } else {
                htmlUrl = "https://github.com/Lokilankaaa/crossflow/releases/tag/\(rawTag)"
            }

            let info = UpdateReleaseInfo(
                version: latestVer,
                tagName: rawTag,
                htmlUrl: htmlUrl,
                notes: dto.body ?? "",
                assetName: assetName,
                downloadUrl: assetURL,
                assetSize: macAsset.size ?? 0,
                checksumsUrl: checksumsAsset?.browserDownloadUrl
            )
            let sizeText = info.assetSize > 0
                ? String(format: "（%.1f MB）", Double(info.assetSize) / (1024.0 * 1024.0))
                : ""
            release = info
            phase = .available
            progress = 0
            message = "发现新版本 v\(info.version)\(sizeText)"
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
        guard let urlStr = release?.htmlUrl,
              Self.isAllowedHttpsUrl(urlStr),
              let url = URL(string: urlStr) else { return }
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
            message = "当前应用目录不可写，请将 Crossflow.app 移至“应用程序”后再更新，或点击“查看说明”手动下载。"
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
                  let checksumsURL = URL(string: checksumsStr) else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "未找到 SHA256SUMS.txt 校验文件，已取消更新。"
                return false
            }

            var sumReq = URLRequest(url: checksumsURL, timeoutInterval: 20)
            sumReq.setValue("Crossflow-Updater/\(currentVersion)", forHTTPHeaderField: "User-Agent")
            let (sumData, sumResp) = try await URLSession.shared.data(for: sumReq)
            guard let sumHttp = sumResp as? HTTPURLResponse, (200...299).contains(sumHttp.statusCode),
                  let sumsText = String(data: sumData, encoding: .utf8),
                  let expectedSha = Self.parseExpectedSha256(sumsText, assetName: info.assetName) else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "SHA256SUMS.txt 中缺少该安装包的校验值，已取消更新。"
                return false
            }

            guard Self.isAllowedHttpsUrl(info.downloadUrl), let dlURL = URL(string: info.downloadUrl) else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "更新包下载地址不受信任，已取消更新。"
                return false
            }

            var dlReq = URLRequest(url: dlURL, timeoutInterval: 600)
            dlReq.setValue("Crossflow-Updater/\(currentVersion)", forHTTPHeaderField: "User-Agent")
            let (asyncBytes, dlResp) = try await URLSession.shared.bytes(for: dlReq)
            guard let dlHttp = dlResp as? HTTPURLResponse, (200...299).contains(dlHttp.statusCode) else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "下载更新包失败，请稍后重试。"
                return false
            }

            let expectedLen = dlHttp.expectedContentLength > 0 ? dlHttp.expectedContentLength : info.assetSize
            fm.createFile(atPath: zipURL.path, contents: nil)
            let handle = try FileHandle(forWritingTo: zipURL)
            defer { try? handle.close() }

            var buffer = Data()
            buffer.reserveCapacity(65536)
            var downloaded: Int64 = 0
            var lastReport = Date.distantPast

            for try await byte in asyncBytes {
                try Task.checkCancellation()
                buffer.append(byte)
                if buffer.count >= 65536 {
                    try handle.write(contentsOf: buffer)
                    downloaded += Int64(buffer.count)
                    buffer.removeAll(keepingCapacity: true)

                    let now = Date()
                    if now.timeIntervalSince(lastReport) >= 0.12 {
                        lastReport = now
                        updateDownloadProgress(downloaded: downloaded, total: expectedLen, version: info.version)
                    }
                }
            }
            if !buffer.isEmpty {
                try handle.write(contentsOf: buffer)
                downloaded += Int64(buffer.count)
                buffer.removeAll(keepingCapacity: false)
            }
            try handle.synchronize()
            updateDownloadProgress(downloaded: downloaded, total: expectedLen, version: info.version)

            try Task.checkCancellation()
            phase = .verifying
            progress = 1.0
            message = "正在校验 SHA-256 完整性…"

            let actualSha = try await Task.detached(priority: .userInitiated) {
                try Self.computeFileSha256(url: zipURL)
            }.value

            guard actualSha.caseInsensitiveCompare(expectedSha) == .orderedSame else {
                try? fm.removeItem(at: workDir)
                phase = .error
                message = "安装包 SHA-256 校验失败，已取消更新。"
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

    private func updateDownloadProgress(downloaded: Int64, total: Int64, version: String) {
        let pct = total > 0 ? min(max(Double(downloaded) / Double(total), 0), 1) : 0
        progress = pct
        let mb = String(format: "%.1f", Double(downloaded) / (1024.0 * 1024.0))
        if total > 0 {
            let totalMb = String(format: "%.1f", Double(total) / (1024.0 * 1024.0))
            message = String(format: "正在下载 v%@… %.0f%%（%@ / %@ MB）", version, pct * 100.0, mb, totalMb)
        } else {
            message = "正在下载 v\(version)…（\(mb) MB）"
        }
    }

    private nonisolated static func resolveCurrentVersion() -> String {
        if let raw = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String {
            let clean = raw.trimmingCharacters(in: .whitespacesAndNewlines)
                .trimmingCharacters(in: CharacterSet(charactersIn: "vV"))
            if parseSemVer(clean) != nil {
                return clean
            }
        }
        return "0.4.0"
    }

    nonisolated static func parseSemVer(_ raw: String) -> (Int, Int, Int)? {
        var s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            .trimmingCharacters(in: CharacterSet(charactersIn: "vV"))
        if let idx = s.firstIndex(where: { $0 == "-" || $0 == "+" }) {
            s = String(s[..<idx])
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
        return (maj, min, pat)
    }

    nonisolated static func isNewerVersion(_ candidate: String, than current: String) -> Bool {
        guard let c = parseSemVer(candidate), let cur = parseSemVer(current) else { return false }
        if c.0 != cur.0 { return c.0 > cur.0 }
        if c.1 != cur.1 { return c.1 > cur.1 }
        return c.2 > cur.2
    }

    private nonisolated static func isAllowedHttpsUrl(_ raw: String) -> Bool {
        guard let url = URL(string: raw),
              url.scheme?.lowercased() == "https",
              let host = url.host?.lowercased() else { return false }
        return allowedHosts.contains { allowed in
            host == allowed || host.hasSuffix(".\(allowed)")
        }
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
