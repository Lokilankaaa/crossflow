import AppKit
import CoreGraphics
import SwiftUI
import UniformTypeIdentifiers

@MainActor
private final class RemoteCursorHider {
    static let shared = RemoteCursorHider()
    private var hidden = false
    private var backgroundEnabled = false

    private typealias CGSMainConnFn = @convention(c) () -> UInt32
    private typealias CGSSetPropFn = @convention(c) (UInt32, UInt32, CFString, CFTypeRef) -> Int32

    private func enableBackgroundCursorIfNeeded() {
        guard !backgroundEnabled else { return }
        if let handle = dlopen("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics", RTLD_LAZY),
           let mainSym = dlsym(handle, "CGSMainConnectionID"),
           let setSym = dlsym(handle, "CGSSetConnectionProperty") {
            let mainConn = unsafeBitCast(mainSym, to: CGSMainConnFn.self)
            let setProp = unsafeBitCast(setSym, to: CGSSetPropFn.self)
            let cid = mainConn()
            _ = setProp(cid, cid, "SetsCursorInBackground" as CFString, kCFBooleanTrue)
            backgroundEnabled = true
        }
    }

    func setHidden(_ hide: Bool) {
        if hide && !hidden {
            enableBackgroundCursorIfNeeded()
            if CGDisplayHideCursor(CGMainDisplayID()) == .success {
                hidden = true
            }
        } else if !hide && hidden {
            _ = CGDisplayShowCursor(CGMainDisplayID())
            hidden = false
        }
    }
}

struct WorkspaceView: View {
    @Bindable var model: WorkspaceModel
    @Bindable var peer: PeerMeshModel
    @ViewState private var showDetails = false
    @ViewState private var layoutNotice: String?
    @ViewState private var undoStack: [SharedLayout] = []
    @ViewState private var editingPassword = false
    @ViewState private var passwordInput = ""

    var body: some View {
        HStack(spacing: 0) {
            sidebar.frame(width: 276)
            Divider().overlay(Theme.border)
            mainWorkspace
        }
        .background(Theme.background)
        .foregroundStyle(Theme.text)
        .tint(Theme.accent)
        .font(.system(size: Theme.bodySize))
        .task {
            await model.refreshBridge()
            peer.start()
        }
        .onChange(of: peer.focusDeviceID) { _, _ in updateCursorVisibility() }
        .onChange(of: peer.state) { _, _ in updateCursorVisibility() }
        .onChange(of: peer.routeEnabled) { _, _ in updateCursorVisibility() }
        .onDisappear { RemoteCursorHider.shared.setHidden(false) }
        .sheet(isPresented: $showDetails) {
            VStack(alignment: .leading, spacing: 16) {
                Text("内核信息").font(.title2.bold())
                Text(model.bridgeMessage).foregroundStyle(Theme.secondary)
                if let ep = peer.listenEndpoint {
                    Text("监听地址：\(ep)").font(.caption).foregroundStyle(Theme.secondary)
                }
                ScrollView {
                    Text(model.environmentJSON.isEmpty ? "暂无环境信息。" : model.environmentJSON)
                        .font(.system(.caption, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                HStack {
                    Button("重新检查") { Task { await model.refreshBridge() } }
                    Spacer()
                    Button("完成") { showDetails = false }
                }
            }
            .padding(24)
            .frame(width: 540, height: 380)
        }
    }

    private func updateCursorVisibility() {
        let remoteFocused = peer.isSharing && peer.focusDeviceID != nil && peer.focusDeviceID != peer.localDeviceID
        RemoteCursorHider.shared.setHidden(remoteFocused)
    }

    private var isAuthMismatch: Bool {
        guard let err = peer.error else { return false }
        return err.hasPrefix("等待密码匹配") || err.hasPrefix("连接验证失败")
    }

    private var statusColor: Color {
        if isAuthMismatch { return .orange }
        switch peer.state {
        case "sharing": return Color(red: 22/255, green: 163/255, blue: 74/255)
        case "paused", "searching", "starting": return Theme.accent
        case "error", "startup_rejected": return .red
        default: return Theme.secondary
        }
    }

    private var sidebar: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                // Brand
                HStack(spacing: 10) {
                    Image(systemName: "rectangle.2.swap")
                        .font(.system(size: 20, weight: .semibold))
                        .foregroundStyle(Theme.accent)
                    Text("Crossflow")
                        .font(.system(size: 20, weight: .semibold))
                    Spacer()
                    Button { showDetails = true } label: {
                        Image(systemName: "info.circle").foregroundStyle(Theme.secondary)
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("内核信息")
                }
                .padding(.bottom, 4)

                // Unified Status Card (1:1 with Windows)
                VStack(alignment: .leading, spacing: 8) {
                    HStack(spacing: 8) {
                        Circle().fill(statusColor).frame(width: 9, height: 9)
                        Text(isAuthMismatch ? "等待密码匹配" : peer.statusTitle).font(.system(size: 14, weight: .semibold))
                    }
                    Text(peer.statusDetail)
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(minHeight: 34, alignment: .topLeading)

                    if peer.state == "sharing" {
                        Button("暂停共享") { peer.pause() }
                            .buttonStyle(.bordered)
                            .frame(maxWidth: .infinity)
                            .accessibilityIdentifier("session-stop")
                    } else if peer.state == "paused" {
                        Button("恢复共享") { peer.resume() }
                            .buttonStyle(.borderedProminent)
                            .frame(maxWidth: .infinity)
                            .accessibilityIdentifier("device-receive")
                    } else if peer.state == "error" || peer.state == "stopped" || peer.state == "startup_rejected" {
                        Button("重新连接") { peer.start() }
                            .buttonStyle(.borderedProminent)
                            .frame(maxWidth: .infinity)
                    }
                }
                .padding(14)
                .background(Theme.background, in: RoundedRectangle(cornerRadius: 12))
                .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))

                // Nearby Devices section
                Text("附近设备")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.secondary)
                    .padding(.top, 2)

                if peer.discoveredPeers.isEmpty && peer.peerName == nil {
                    Text("另一台电脑打开 Crossflow 后自动连接。")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.secondary)
                } else {
                    VStack(spacing: 6) {
                        let list = peer.discoveredPeers.isEmpty && peer.peerName != nil
                            ? [DiscoveredPeerInfo(id: peer.peerDeviceID ?? "peer", name: peer.peerName ?? "远端设备", platform: "windows", connected: true)]
                            : peer.discoveredPeers
                        ForEach(list) { dev in
                            HStack(spacing: 10) {
                                Image(systemName: dev.platform == "macos" ? "laptopcomputer" : "desktopcomputer")
                                    .foregroundStyle(dev.connected ? Color(red: 22/255, green: 163/255, blue: 74/255) : Theme.secondary)
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(dev.name)
                                        .font(.system(size: 13, weight: dev.connected ? .semibold : .regular))
                                        .lineLimit(1)
                                    let sub: String = {
                                        if dev.connected, let addr = peer.peerAddress {
                                            if let ch = peer.activeChannelLabel { return "\(ch) · \(addr)" }
                                            return addr
                                        }
                                        return dev.platform == "macos" ? "macOS" : "Windows"
                                    }()
                                    Text(sub)
                                        .font(.system(size: 11))
                                        .foregroundStyle(Theme.secondary)
                                        .lineLimit(1)
                                }
                                Spacer()
                                Text(dev.connected ? "已连接" : "已发现")
                                    .font(.system(size: 11, weight: .medium))
                                    .foregroundStyle(dev.connected ? Color(red: 22/255, green: 163/255, blue: 74/255) : Theme.secondary)
                            }
                            .padding(.horizontal, 10)
                            .padding(.vertical, 8)
                            .background(dev.connected ? Theme.subtle : Theme.background, in: RoundedRectangle(cornerRadius: 9))
                            .overlay(RoundedRectangle(cornerRadius: 9).stroke(dev.connected ? Theme.accent : Theme.border, lineWidth: 1))
                        }
                    }
                }

                Divider().overlay(Theme.border).padding(.vertical, 2)

                // Network Channel selector (1:1 with Windows)
                VStack(alignment: .leading, spacing: 6) {
                    Text("网络信道")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.secondary)

                    let channelBinding = Binding<String>(
                        get: { peer.selectedChannel },
                        set: { next in
                            peer.setChannel(next, directPeer: next == "auto" ? nil : peer.directPeer)
                        }
                    )
                    Picker("", selection: channelBinding) {
                        if peer.networkChannels.isEmpty {
                            Text("自动选择最优").tag("auto")
                        } else {
                            ForEach(peer.networkChannels) { ch in
                                Text(ch.label).tag(ch.id)
                            }
                        }
                    }
                    .labelsHidden()
                    .frame(maxWidth: .infinity)

                    if peer.selectedChannel != "auto" {
                        TextField("对方 IP（可选，如 192.168.194.10）", text: $peer.directPeer)
                            .textFieldStyle(.roundedBorder)
                            .font(.system(size: 12))
                        Button("指定连接") {
                            peer.setChannel(peer.selectedChannel, directPeer: peer.directPeer)
                        }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                        .frame(maxWidth: .infinity)
                    }
                }

                Divider().overlay(Theme.border).padding(.vertical, 2)

                // Minimal Password Configuration (已配置 / 未配置 — 1:1 with Windows)
                VStack(alignment: .leading, spacing: 6) {
                    HStack {
                        Text("连接密码")
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundStyle(Theme.secondary)
                        Spacer()
                        Text(peer.passphraseConfigured ? "已配置" : "未配置")
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(peer.passphraseConfigured ? Theme.accent : Theme.secondary)
                    }

                    if editingPassword {
                        SecureField("输入共享密码", text: $passwordInput)
                            .textFieldStyle(.roundedBorder)
                            .font(.system(size: 12))
                        HStack(spacing: 6) {
                            Button("保存") {
                                let trimmed = passwordInput.trimmingCharacters(in: .whitespacesAndNewlines)
                                if trimmed.isEmpty {
                                    layoutNotice = "密码不能为空。"
                                } else {
                                    layoutNotice = nil
                                    passwordInput = ""
                                    editingPassword = false
                                    peer.setPassphrase(trimmed)
                                }
                            }
                            .buttonStyle(.borderedProminent)
                            .controlSize(.small)
                            Button("取消") {
                                passwordInput = ""
                                editingPassword = false
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                        }
                    } else {
                        HStack(spacing: 6) {
                            Button(peer.passphraseConfigured ? "修改" : "配置密码") {
                                passwordInput = ""
                                editingPassword = true
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                            if peer.passphraseConfigured {
                                Button("清除") {
                                    peer.setPassphrase(nil)
                                }
                                .buttonStyle(.bordered)
                                .controlSize(.small)
                            }
                        }
                    }
                }

                Divider().overlay(Theme.border).padding(.vertical, 2)

                // Clipboard sync toggle (1:1 with Windows)
                VStack(alignment: .leading, spacing: 6) {
                    Text("剪贴板")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.secondary)
                    let clipBinding = Binding<Bool>(
                        get: { peer.clipboardSync },
                        set: { peer.setClipboard($0) }
                    )
                    Toggle(isOn: clipBinding) {
                        Text(peer.clipboardSync ? "剪贴板共享已开启" : "剪贴板共享已关闭")
                            .font(.system(size: 12))
                    }
                    .toggleStyle(.switch)
                    .controlSize(.small)
                }

                Divider().overlay(Theme.border).padding(.vertical, 2)

                // Screens subsection (aligned 1:1 with Windows sidebar!)
                Text("屏幕")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.secondary)

                sidebarScreenList

                Divider().overlay(Theme.border).padding(.vertical, 2)

                Text("快捷键：本机按 Escape 随时暂停共享。")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(18)
        }
        .frame(maxHeight: .infinity)
        .background(Theme.surface)
    }

    @ViewBuilder
    private var sidebarScreenList: some View {
        if let layout = peer.sharedLayout, !layout.screens.isEmpty {
            let deviceOrder = layout.screens.reduce(into: [String]()) { acc, s in
                if !acc.contains(s.deviceID) { acc.append(s.deviceID) }
            }
            VStack(alignment: .leading, spacing: 6) {
                ForEach(Array(deviceOrder.enumerated()), id: \.element) { idx, devID in
                    let group = layout.screens.filter { $0.deviceID == devID }
                    let isLocal = devID == peer.localDeviceID
                    let devLabel = isLocal ? "\(peer.localName) (本机)" : (peer.peerName ?? "远端设备")
                    let color = idx % 2 == 0 ? Theme.accent : Color(red: 13/255, green: 148/255, blue: 136/255)
                    Text("\(devLabel) · \(group.count) 屏")
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(color)
                        .padding(.top, 2)
                    ForEach(Array(group.enumerated()), id: \.element.id) { sIdx, screen in
                        let focused = peer.routeEnabled && peer.focusScreenID == screen.id
                        HStack(spacing: 8) {
                            Image(systemName: screen.height > screen.width ? "rectangle.portrait" : (isLocal ? "laptopcomputer" : "display"))
                                .font(.system(size: 12))
                                .foregroundStyle(color)
                            Text("屏幕 \(sIdx + 1) (\(Int(screen.width))×\(Int(screen.height)))")
                                .font(.system(size: 12))
                                .lineLimit(1)
                            Spacer()
                        }
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                        .background(focused ? Theme.subtle : Theme.background, in: RoundedRectangle(cornerRadius: 8))
                    }
                }
            }
        } else if !peer.localDisplays.isEmpty {
            VStack(alignment: .leading, spacing: 6) {
                Text("\(peer.localName) (本机) · \(peer.localDisplays.count) 屏")
                    .font(.system(size: 11, weight: .medium))
                    .foregroundStyle(Theme.accent)
                ForEach(Array(peer.localDisplays.enumerated()), id: \.element.id) { idx, d in
                    HStack(spacing: 8) {
                        Image(systemName: "laptopcomputer")
                            .font(.system(size: 12))
                            .foregroundStyle(Theme.accent)
                        Text("屏幕 \(idx + 1) (\(Int(d.width))×\(Int(d.height)))")
                            .font(.system(size: 12))
                            .lineLimit(1)
                        Spacer()
                    }
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .background(Theme.background, in: RoundedRectangle(cornerRadius: 8))
                }
            }
        } else {
            Text("正在读取显示器…")
                .font(.system(size: 12))
                .foregroundStyle(Theme.secondary)
        }
    }

    private var mainWorkspace: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .center) {
                VStack(alignment: .leading, spacing: 4) {
                    Text("屏幕排列").font(.system(size: Theme.titleSize, weight: .semibold))
                    Text("拖动任意屏幕贴合相邻边缘，松手后自动同步到双方设备。")
                        .font(.system(size: Theme.captionSize))
                        .foregroundStyle(Theme.secondary)
                }
                Spacer()
                if !undoStack.isEmpty && peer.sharedLayout != nil {
                    Button("撤销") {
                        if let prev = undoStack.popLast() {
                            commitLayout(prev, recordUndo: false)
                        }
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
                }
                StatusBadge(title: isAuthMismatch ? "等待密码匹配" : peer.statusTitle, active: peer.isSharing)
            }

            if let notice = layoutNotice {
                HStack(spacing: 8) {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                    Text(notice).font(.system(size: 12))
                    Spacer()
                    Button { layoutNotice = nil } label: {
                        Image(systemName: "xmark").font(.system(size: 11))
                    }
                    .buttonStyle(.plain)
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .background(Color.orange.opacity(0.1), in: RoundedRectangle(cornerRadius: 8))
            }

            if peer.state == "startup_rejected" {
                permissionCard
            } else if let layout = peer.sharedLayout {
                SessionLayoutView(
                    layout: layout,
                    focusScreenID: peer.focusScreenID,
                    routeEnabled: peer.routeEnabled,
                    sourceName: peer.peerName ?? "另一台电脑",
                    localDeviceID: peer.localDeviceID,
                    localName: peer.localName,
                    editable: true,
                    onCommitLayout: { updated in
                        commitLayout(updated, recordUndo: true)
                    }
                )
            } else {
                waitingCanvas
            }

            if let xfer = peer.transfer {
                transferBar(xfer)
            }

            HStack {
                let connText: String = {
                    guard let name = peer.peerName else { return "正在自动发现附近设备…" }
                    let route = [peer.activeChannelLabel, peer.peerAddress].compactMap { $0 }.joined(separator: " · ")
                    return route.isEmpty ? "已连接：\(name)" : "已连接：\(name) (\(route))"
                }()
                Text(connText)
                Spacer()
                Text("支持复制文件或拖入窗口跨屏直传")
            }
            .font(.system(size: 12))
            .foregroundStyle(Theme.secondary)
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.background)
        .onDrop(of: [.fileURL], isTargeted: nil) { providers in
            guard peer.peerDeviceID != nil else { return false }
            handleFileDrop(providers)
            return true
        }
    }

    private func handleFileDrop(_ providers: [NSItemProvider]) {
        let group = DispatchGroup()
        let lock = NSLock()
        var collected: [String] = []
        for provider in providers where provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
            group.enter()
            provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier, options: nil) { item, _ in
                defer { group.leave() }
                if let data = item as? Data, let url = URL(dataRepresentation: data, relativeTo: nil) {
                    lock.withLock { collected.append(url.path) }
                } else if let url = item as? URL {
                    lock.withLock { collected.append(url.path) }
                }
            }
        }
        group.notify(queue: .main) {
            let paths = lock.withLock { collected }
            if !paths.isEmpty {
                peer.sendFiles(paths)
            }
        }
    }

    private func transferBar(_ xfer: TransferStatusInfo) -> some View {
        let pct = xfer.bytesTotal > 0 ? min(1.0, Double(xfer.bytesDone) / Double(xfer.bytesTotal)) : (xfer.state == "completed" ? 1.0 : 0.0)
        return HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 4) {
                Text(transferTitle(xfer))
                    .font(.system(size: 12, weight: .semibold))
                if xfer.state == "active" {
                    ProgressView(value: pct)
                        .progressViewStyle(.linear)
                }
                Text(transferSubtitle(xfer, pct: pct))
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.secondary)
            }
            Spacer()
            if xfer.state == "active" {
                Button("取消") { peer.cancelTransfer() }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
            } else if xfer.state == "completed", let dir = xfer.savedDir {
                Button("打开文件夹") {
                    NSWorkspace.shared.open(URL(fileURLWithPath: dir))
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).stroke(Theme.border, lineWidth: 1))
    }

    private func transferTitle(_ xfer: TransferStatusInfo) -> String {
        switch xfer.state {
        case "active": return (xfer.direction == "sending" ? "正在发送：" : "正在接收：") + xfer.name
        case "completed": return xfer.direction == "sending" ? "已发送：\(xfer.name)" : "已接收：\(xfer.name)（可直接粘贴）"
        case "cancelled": return "已取消传输：\(xfer.name)"
        default: return "传输失败：\(xfer.name)"
        }
    }

    private func transferSubtitle(_ xfer: TransferStatusInfo, pct: Double) -> String {
        switch xfer.state {
        case "active":
            return "\(formatBytes(xfer.bytesDone)) / \(formatBytes(xfer.bytesTotal)) · \(Int(pct * 100))%（\(xfer.filesDone)/\(xfer.filesTotal)）"
        case "completed":
            if let dir = xfer.savedDir { return "已保存至 \(dir)" }
            return "\(formatBytes(xfer.bytesTotal)) · 校验通过"
        case "cancelled":
            return "临时文件已清理"
        default:
            return xfer.error ?? "请重试"
        }
    }

    private func formatBytes(_ bytes: UInt64) -> String {
        if bytes >= 1024 * 1024 * 1024 { return String(format: "%.2f GB", Double(bytes) / 1_073_741_824.0) }
        if bytes >= 1024 * 1024 { return String(format: "%.1f MB", Double(bytes) / 1_048_576.0) }
        if bytes >= 1024 { return String(format: "%.1f KB", Double(bytes) / 1024.0) }
        return "\(bytes) B"
    }

    private func commitLayout(_ candidate: SharedLayout, recordUndo: Bool) {
        let previous = peer.sharedLayout
        let doc = LayoutDocument(
            screens: candidate.screens.map {
                ScreenDraft(screenID: $0.id, deviceID: $0.deviceID, x: $0.x, y: $0.y, width: $0.width, height: $0.height)
            }
        )
        Task {
            do {
                let response = try await BridgeClient().request(.object([
                    "op": .string("layout_validate"),
                    "layout": doc.json
                ]))
                guard response["ok"] == .bool(true) else {
                    layoutNotice = "屏幕位置不能重叠，请贴合边缘摆放。"
                    return
                }
                layoutNotice = nil
                if recordUndo, let previous {
                    undoStack.append(previous)
                    if undoStack.count > 20 { undoStack.removeFirst() }
                }
                peer.applyLayout(candidate)
            } catch {
                layoutNotice = "屏幕排列校验失败，请重试。"
            }
        }
    }

    private var permissionCard: some View {
        VStack(spacing: 14) {
            Image(systemName: "lock.shield")
                .font(.system(size: 38, weight: .light))
                .foregroundStyle(.orange)
            Text("需要系统输入权限").font(.headline)
            Text(peer.statusDetail)
                .foregroundStyle(Theme.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
            HStack(spacing: 12) {
                Button("打开隐私与安全性设置") {
                    if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility") {
                        NSWorkspace.shared.open(url)
                    }
                }
                .buttonStyle(.borderedProminent)
                Button("重新检测") { peer.start() }
                    .buttonStyle(.bordered)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(24)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
    }

    private var waitingCanvas: some View {
        VStack(spacing: 14) {
            Image(systemName: "dot.radiowaves.left.and.right")
                .font(.system(size: 38, weight: .light))
                .foregroundStyle(Theme.accent)
            Text("正在自动发现附近设备")
                .font(.headline)
            Text("另一台电脑打开 Crossflow 后将自动选择最优信道连接并同步屏幕排列。")
                .foregroundStyle(Theme.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
            if !peer.localDisplays.isEmpty {
                Text("已识别本机 \(peer.localDisplays.count) 块显示器")
                    .font(.caption)
                    .foregroundStyle(Theme.secondary)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(24)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
        .accessibilityIdentifier("session-layout-empty")
    }
}
