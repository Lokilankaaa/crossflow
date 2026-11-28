import AppKit
import SwiftUI

/// Quiet White「静白」About / Updater / Diagnostics & Community Support sheet for macOS,
/// matching Windows & HaloDeck's 「更新与诊断」 + 「社区与支持」 tabs 1:1.
struct AboutCommunityView: View {
    @Bindable var model: WorkspaceModel
    @Bindable var peer: PeerMeshModel
    @Bindable var updater: AppUpdater

    @ViewState private var mirrorPresetID: String = "direct"
    @ViewState private var customMirrorInput: String = ""
    @ViewState private var customMirrorError: String?
    @ViewState private var toastNotice: String?
    @ViewState private var showKernelJSON = false

    var body: some View {
        VStack(spacing: 0) {
            // Header bar with 2 tabs + close button
            HStack(spacing: 8) {
                tabButton(id: "updates", title: "更新与诊断", systemImage: "arrow.triangle.2.circlepath", showDot: updater.phase == .available)
                tabButton(id: "community", title: "社区与支持", systemImage: "heart", showDot: false)
                Spacer()
                if let toast = toastNotice {
                    Text(toast)
                        .font(.system(size: 11.5, weight: .medium))
                        .foregroundStyle(Color(red: 21/255, green: 128/255, blue: 61/255))
                        .padding(.horizontal, 10)
                        .padding(.vertical, 4)
                        .background(Color(red: 22/255, green: 163/255, blue: 74/255).opacity(0.12), in: Capsule())
                }
                Button {
                    updater.showAboutSheet = false
                } label: {
                    Image(systemName: "xmark")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.secondary)
                        .padding(6)
                }
                .buttonStyle(.plain)
                .help("关闭")
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 13)
            .background(Theme.surface)

            Divider().overlay(Theme.border)

            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    if updater.activeAboutTab == "community" {
                        communityTab
                    } else {
                        updatesTab
                    }
                }
                .padding(22)
            }
            .background(Theme.background)
        }
        .frame(width: 720, height: 600)
        .preferredColorScheme(.light)
        .onAppear {
            syncMirrorControls()
        }
        .onChange(of: updater.mirrorPrefix) { _, _ in
            syncMirrorControls()
        }
    }

    private func syncMirrorControls() {
        let id = AppUpdater.resolveMirrorPresetID(updater.mirrorPrefix)
        mirrorPresetID = id
        if id == "custom" && customMirrorInput.isEmpty {
            customMirrorInput = updater.mirrorPrefix
        }
    }

    private func showToast(_ text: String) {
        toastNotice = text
        Task {
            try? await Task.sleep(for: .seconds(3))
            if toastNotice == text {
                toastNotice = nil
            }
        }
    }

    private func tabButton(id: String, title: String, systemImage: String, showDot: Bool) -> some View {
        let active = updater.activeAboutTab == id
        return Button {
            updater.activeAboutTab = id
        } label: {
            HStack(spacing: 6) {
                Image(systemName: systemImage)
                    .font(.system(size: 12.5, weight: .semibold))
                Text(title)
                    .font(.system(size: 13, weight: .semibold))
                if showDot {
                    Circle()
                        .fill(Theme.accent)
                        .frame(width: 7, height: 7)
                }
            }
            .foregroundStyle(active ? Theme.accent : Theme.secondary)
            .padding(.horizontal, 13)
            .padding(.vertical, 7)
            .background(active ? Theme.background : Color.clear, in: RoundedRectangle(cornerRadius: 9))
            .overlay(
                RoundedRectangle(cornerRadius: 9)
                    .stroke(active ? Theme.border : Color.clear, lineWidth: 1)
            )
        }
        .buttonStyle(.plain)
    }

    // MARK: - Tab 1: 更新与诊断
    private var updatesTab: some View {
        let isBusy = updater.phase == .checking || updater.phase == .downloading || updater.phase == .verifying || updater.phase == .applying
        let canApply = updater.release != nil && (updater.phase == .available || updater.phase == .error)

        return VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 2) {
                Text("更新与诊断")
                    .font(.system(size: 18, weight: .semibold))
                    .foregroundStyle(Theme.text)
                Text("应用内自更新、下载加速线路、测试通道与本地日志诊断")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.secondary)
            }

            // 1. Brand & Updater Hero Card
            VStack(alignment: .leading, spacing: 12) {
                HStack(alignment: .center, spacing: 12) {
                    ZStack {
                        RoundedRectangle(cornerRadius: 11)
                            .fill(Theme.accent.opacity(0.10))
                            .overlay(RoundedRectangle(cornerRadius: 11).stroke(Theme.accent.opacity(0.22), lineWidth: 1))
                        Image(systemName: "rectangle.2.swap")
                            .font(.system(size: 19, weight: .semibold))
                            .foregroundStyle(Theme.accent)
                    }
                    .frame(width: 42, height: 42)

                    VStack(alignment: .leading, spacing: 3) {
                        HStack(spacing: 8) {
                            Text("Crossflow")
                                .font(.system(size: 16, weight: .semibold))
                                .foregroundStyle(Theme.text)
                            pill("v\(updater.currentVersion)", bg: Theme.surface, fg: Theme.secondary)
                            statusBadge
                        }
                        Text("跨平台低延迟键鼠共享、剪贴板与文件直传 · 保留配置原地安全升级")
                            .font(.system(size: 12))
                            .foregroundStyle(Theme.secondary)
                    }

                    Spacer()

                    HStack(spacing: 6) {
                        if updater.phase != .downloading && updater.phase != .verifying && updater.phase != .applying {
                            Button(updater.phase == .checking ? "检查中…" : "立即检查更新") {
                                Task { await updater.checkForUpdates(silent: false) }
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                            .disabled(isBusy)
                        }

                        if canApply, let rel = updater.release {
                            Button("一键更新 v\(rel.version)") {
                                Task {
                                    let launched = await updater.downloadAndApply {
                                        _ = await peer.shutdown()
                                    }
                                    if launched {
                                        NSApp.terminate(nil)
                                    }
                                }
                            }
                            .buttonStyle(.borderedProminent)
                            .controlSize(.small)
                        }

                        if updater.release != nil && !isBusy {
                            Button("更新日志 ↗") {
                                updater.openReleaseNotes()
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                        }

                        if updater.phase == .downloading {
                            Button("取消") {
                                updater.cancelDownload()
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                        }
                    }
                }

                Text(updater.message)
                    .font(.system(size: 12))
                    .foregroundStyle(
                        updater.phase == .error ? Color.red
                        : (updater.phase == .available ? Theme.accent : Theme.secondary)
                    )
                    .fixedSize(horizontal: false, vertical: true)

                if updater.phase == .downloading {
                    ProgressView(value: updater.progress, total: 1.0)
                        .progressViewStyle(.linear)
                } else if updater.phase == .verifying || updater.phase == .applying {
                    ProgressView()
                        .progressViewStyle(.linear)
                }

                if let notes = updater.release?.notes, !notes.isEmpty {
                    ScrollView {
                        Text(notes)
                            .font(.system(size: 11.5))
                            .foregroundStyle(Theme.secondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .textSelection(.enabled)
                    }
                    .frame(maxHeight: 110)
                    .padding(10)
                    .background(Theme.surface, in: RoundedRectangle(cornerRadius: 10))
                    .overlay(RoundedRectangle(cornerRadius: 10).stroke(Theme.border, lineWidth: 1))
                }

                // Settings rows: Mirror, Update Channel, GitHub Issues
                VStack(spacing: 8) {
                    settingRow(
                        title: "下载加速线路",
                        subtitle: "用于检查更新与下载安装包，镜像失败或校验异常时自动回退 GitHub 官方直连"
                    ) {
                        Picker("", selection: Binding(
                            get: { mirrorPresetID },
                            set: { next in
                                mirrorPresetID = next
                                if let preset = AppUpdater.mirrorPresets.first(where: { $0.id == next }),
                                   let prefix = preset.prefix {
                                    customMirrorError = nil
                                    _ = updater.setMirrorPrefix(prefix)
                                    showToast("已切换：\(preset.label)")
                                } else {
                                    customMirrorInput = updater.mirrorPrefix
                                }
                            }
                        )) {
                            ForEach(AppUpdater.mirrorPresets) { preset in
                                Text(preset.label).tag(preset.id)
                            }
                        }
                        .labelsHidden()
                        .frame(width: 210)
                    }

                    if mirrorPresetID == "custom" {
                        VStack(alignment: .leading, spacing: 6) {
                            HStack(spacing: 8) {
                                TextField("https://gh-proxy.com/", text: $customMirrorInput)
                                    .textFieldStyle(.roundedBorder)
                                    .font(.system(size: 12))
                                Button("保存前缀") {
                                    if let err = updater.setMirrorPrefix(customMirrorInput) {
                                        customMirrorError = err
                                    } else {
                                        customMirrorError = nil
                                        showToast("自定义镜像前缀已保存")
                                    }
                                }
                                .buttonStyle(.borderedProminent)
                                .controlSize(.small)
                                Button("取消") {
                                    customMirrorError = nil
                                    syncMirrorControls()
                                }
                                .buttonStyle(.bordered)
                                .controlSize(.small)
                            }
                            if let err = customMirrorError {
                                Text(err)
                                    .font(.system(size: 11))
                                    .foregroundStyle(.red)
                            }
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 10))
                        .overlay(RoundedRectangle(cornerRadius: 10).stroke(Theme.border, lineWidth: 1))
                    }

                    settingRow(
                        title: "更新通道",
                        subtitle: "正式版稳定可靠；测试版 (-rc) 可抢先体验最新跨屏特性与修复"
                    ) {
                        Picker("", selection: Binding(
                            get: { updater.includePrerelease },
                            set: { next in
                                updater.setIncludePrerelease(next)
                                Task { await updater.checkForUpdates(silent: false) }
                            }
                        )) {
                            Text("仅正式版").tag(false)
                            Text("含测试版 (-rc)").tag(true)
                        }
                        .pickerStyle(.segmented)
                        .labelsHidden()
                        .frame(width: 210)
                    }

                    settingRow(
                        title: "反馈问题（GitHub Issues）",
                        subtitle: "自动复制脱敏环境摘要（不含密码与剪贴板内容）并前往 GitHub 提交工单"
                    ) {
                        Button("复制诊断并反馈 ↗") {
                            copyDiagnosticSummary()
                            updater.openExternalURL(AppUpdater.issuesNewURL)
                            showToast("已复制诊断摘要并打开 Issues")
                        }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                    }
                }
            }
            .padding(16)
            .background(Theme.background, in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))

            // 2. Diagnostics & Local Directories Card
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Text("系统诊断与本地目录")
                        .font(.system(size: 13.5, weight: .semibold))
                        .foregroundStyle(Theme.text)
                    Spacer()
                    Button("一键复制完整诊断信息") {
                        copyDiagnosticSummary()
                        showToast("已复制脱敏诊断摘要")
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                }

                let connectedCount = peer.peers.filter(\.connected).count
                let chText = peer.activeChannelLabel ?? (peer.selectedChannel == "auto" ? "自动择优" : peer.selectedChannel)
                LazyVGrid(columns: [GridItem(.flexible(), spacing: 10), GridItem(.flexible(), spacing: 10)], spacing: 10) {
                    diagCell(label: "当前版本与通道", value: "v\(updater.currentVersion) · \(updater.includePrerelease ? "含测试版 (-rc)" : "仅正式版")")
                    diagCell(label: "下载加速线路", value: updater.describeMirror())
                    diagCell(label: "网络与对端状态", value: connectedCount > 0 ? "已连接 \(connectedCount) 台 · \(chText)" : "状态：\(peer.state) · \(chText)")
                    diagCell(label: "本机监听地址", value: peer.listenEndpoint ?? "初始化中")
                }

                settingRow(title: "运行日志目录", subtitle: AppUpdater.logsDirURL.path) {
                    Button("打开目录") { updater.openFolder(AppUpdater.logsDirURL) }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                }

                settingRow(title: "用户配置目录", subtitle: AppUpdater.userDataDirURL.path) {
                    Button("打开目录") { updater.openFolder(AppUpdater.userDataDirURL) }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                }

                DisclosureGroup("内核环境详情（bridge JSON）", isExpanded: $showKernelJSON) {
                    VStack(alignment: .leading, spacing: 8) {
                        HStack {
                            Text(model.bridgeMessage)
                                .font(.system(size: 11.5))
                                .foregroundStyle(Theme.secondary)
                            Spacer()
                            Button("重新检查") { Task { await model.refreshBridge() } }
                                .buttonStyle(.bordered)
                                .controlSize(.mini)
                        }
                        ScrollView {
                            Text(model.environmentJSON.isEmpty ? "暂无环境信息。" : model.environmentJSON)
                                .font(.system(.caption, design: .monospaced))
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                        .frame(maxHeight: 130)
                        .padding(8)
                        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 8))
                    }
                    .padding(.top, 6)
                }
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(Theme.secondary)
            }
            .padding(16)
            .background(Theme.background, in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
        }
    }

    // MARK: - Tab 2: 社区与支持
    private var communityTab: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 2) {
                Text("社区与支持")
                    .font(.system(size: 18, weight: .semibold))
                    .foregroundStyle(Theme.text)
                Text("关注开源项目进展、参与问题讨论与查看实机演示视频")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.secondary)
            }

            // Community Hero Card (adapted 1:1 from HaloDeck)
            HStack(alignment: .top, spacing: 14) {
                ZStack {
                    RoundedRectangle(cornerRadius: 12)
                        .fill(Theme.accent.opacity(0.10))
                        .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.accent.opacity(0.22), lineWidth: 1))
                    Image(systemName: "rectangle.2.swap")
                        .font(.system(size: 21, weight: .semibold))
                        .foregroundStyle(Theme.accent)
                }
                .frame(width: 46, height: 46)

                VStack(alignment: .leading, spacing: 7) {
                    Text("开源 · 免费 · 专为多机桌面协同而生")
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(Theme.text)
                    Text("Crossflow 遵循 MIT 开源协议持续迭代，所有跨屏键鼠共享、Noise 端到端加密、剪贴板图片与文件直传及双端原生客户端均永久免费开放。欢迎在 GitHub 点亮 Star、提交反馈或共同参与贡献。")
                        .font(.system(size: 12.5))
                        .foregroundStyle(Theme.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    HStack(spacing: 8) {
                        pill("MIT 开源协议", bg: Color(red: 22/255, green: 163/255, blue: 74/255).opacity(0.12), fg: Color(red: 21/255, green: 128/255, blue: 61/255))
                        pill("永久免费 · 无广告", bg: Theme.surface, fg: Theme.secondary)
                        pill("本地直连 · 零遥测", bg: Theme.surface, fg: Theme.secondary)
                    }
                }
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.background, in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))

            // 3-Column Social / Community Cards (1:1 with HaloDeck)
            HStack(spacing: 12) {
                socialCard(
                    systemImage: "chevron.left.forwardslash.chevron.right",
                    accent: Color(red: 15/255, green: 23/255, blue: 42/255),
                    cta: "前往 Star ↗",
                    title: "Crossflow 项目仓库",
                    desc: "浏览源码、点亮 Star、参与 Issue 反馈与 PR 贡献",
                    urlLabel: "github.com/Lokilankaaa/crossflow",
                    url: AppUpdater.repoBaseURL
                )

                socialCard(
                    systemImage: "person.crop.circle",
                    accent: Theme.accent,
                    cta: "关注主页 ↗",
                    title: "作者 GitHub 主页",
                    desc: "关注 Lokilankaaa 开源动态、HaloDeck 与新工具发布",
                    urlLabel: "github.com/Lokilankaaa",
                    url: AppUpdater.authorGitHubURL
                )

                socialCard(
                    systemImage: "play.tv",
                    accent: Color(red: 251/255, green: 114/255, blue: 153/255),
                    cta: "前往 B 站 ↗",
                    title: "作者哔哩哔哩主页",
                    desc: "嗷嗷嗷嗷的猫神 · 版本演示、跨屏实战与折腾分享",
                    urlLabel: "b23.tv/pANRtSc",
                    url: AppUpdater.authorBilibiliURL
                )
            }
        }
    }

    // MARK: - Helpers
    private var statusBadge: some View {
        switch updater.phase {
        case .available:
            let tag = updater.release.map { "发现新版 v\($0.version)\($0.isPrerelease ? " RC" : "")" } ?? "发现新版本"
            return pill(tag, bg: Theme.accent.opacity(0.12), fg: Theme.accent)
        case .upToDate:
            return pill("已是最新版本", bg: Color(red: 22/255, green: 163/255, blue: 74/255).opacity(0.12), fg: Color(red: 21/255, green: 128/255, blue: 61/255))
        case .error:
            return pill("检查/更新异常", bg: Color.red.opacity(0.10), fg: Color.red)
        case .checking:
            return pill("检查中…", bg: Theme.accent.opacity(0.12), fg: Theme.accent)
        case .downloading, .verifying, .applying:
            return pill("更新中…", bg: Theme.accent.opacity(0.12), fg: Theme.accent)
        case .idle:
            return pill("点击检查更新", bg: Theme.surface, fg: Theme.secondary)
        }
    }

    private func pill(_ text: String, bg: Color, fg: Color) -> some View {
        Text(text)
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(fg)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(bg, in: Capsule())
    }

    private func settingRow<Content: View>(title: String, subtitle: String, @ViewBuilder control: () -> Content) -> some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.system(size: 12.5, weight: .semibold))
                    .foregroundStyle(Theme.text)
                Text(subtitle)
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
            control()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).stroke(Theme.border, lineWidth: 1))
    }

    private func diagCell(label: String, value: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(label)
                .font(.system(size: 11))
                .foregroundStyle(Theme.secondary)
            Text(value)
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Theme.text)
                .lineLimit(2)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 9))
        .overlay(RoundedRectangle(cornerRadius: 9).stroke(Theme.border, lineWidth: 1))
    }

    private func socialCard(
        systemImage: String,
        accent: Color,
        cta: String,
        title: String,
        desc: String,
        urlLabel: String,
        url: String
    ) -> some View {
        Button {
            updater.openExternalURL(url)
        } label: {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    ZStack {
                        RoundedRectangle(cornerRadius: 10)
                            .fill(accent.opacity(0.11))
                        Image(systemName: systemImage)
                            .font(.system(size: 15, weight: .semibold))
                            .foregroundStyle(accent)
                    }
                    .frame(width: 36, height: 36)
                    Spacer()
                    Text(cta)
                        .font(.system(size: 11.5, weight: .semibold))
                        .foregroundStyle(Theme.accent)
                }

                VStack(alignment: .leading, spacing: 4) {
                    Text(title)
                        .font(.system(size: 13.5, weight: .semibold))
                        .foregroundStyle(Theme.text)
                    Text(desc)
                        .font(.system(size: 11.5))
                        .foregroundStyle(Theme.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }

                Spacer(minLength: 4)

                Text(urlLabel)
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.secondary)
                    .lineLimit(1)
            }
            .padding(14)
            .frame(maxWidth: .infinity, minHeight: 162, alignment: .topLeading)
            .background(Theme.surface, in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
        }
        .buttonStyle(.plain)
    }

    private func copyDiagnosticSummary() {
        let connected = peer.peers.filter(\.connected).count
        let osVer = ProcessInfo.processInfo.operatingSystemVersionString
        let lines = [
            "### Crossflow 环境诊断摘要",
            "- 客户端版本: v\(updater.currentVersion) (macOS \(osVer))",
            "- 更新通道: \(updater.includePrerelease ? "含测试版 (-rc)" : "仅正式版")",
            "- 下载加速线路: \(updater.describeMirror())",
            "- 连接状态: \(peer.state) (已连接对端: \(connected))",
            "- 当前信道: \(peer.activeChannelLabel ?? peer.selectedChannel)",
            "- 共享开关: 剪贴板=\(peer.clipboardSync ? "开" : "关") · 输入法同步=\(peer.inputStateSync ? "开" : "关")"
        ]
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(lines.joined(separator: "\n"), forType: .string)
    }
}
