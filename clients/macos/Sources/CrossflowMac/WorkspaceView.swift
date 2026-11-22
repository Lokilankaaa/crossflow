import AppKit
import CoreGraphics
import SwiftUI

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

    private var statusColor: Color {
        switch peer.state {
        case "sharing": Color(red: 22/255, green: 163/255, blue: 74/255)
        case "paused", "searching", "starting": Theme.accent
        case "error", "startup_rejected": .red
        default: Theme.secondary
        }
    }

    private var sidebar: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
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

                // Unified Status Card (symmetric with Windows)
                VStack(alignment: .leading, spacing: 10) {
                    HStack(spacing: 8) {
                        Circle().fill(statusColor).frame(width: 9, height: 9)
                        Text(peer.statusTitle).font(.system(size: 14, weight: .semibold))
                    }
                    Text(peer.statusDetail)
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.secondary)
                        .fixedSize(horizontal: false, vertical: true)

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

                // LAN Devices section
                Text("局域网设备")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.secondary)
                    .padding(.top, 4)

                if peer.discoveredPeers.isEmpty && peer.peerName == nil {
                    Text("在另一台电脑打开 Crossflow 即可自动发现并连接。")
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
                                    Text(dev.connected && peer.peerAddress != nil ? peer.peerAddress! : (dev.platform == "macos" ? "macOS · 局域网" : "Windows · 局域网"))
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

                // Security Mode (optional shared passphrase)
                VStack(alignment: .leading, spacing: 8) {
                    Text("连接安全")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.secondary)

                    Toggle(isOn: $peer.usePassphrase) {
                        Text(peer.usePassphrase ? "仅限共享密码连接" : "局域网自动连接（免密码）")
                            .font(.system(size: 12))
                    }
                    .toggleStyle(.switch)
                    .controlSize(.small)
                    .onChange(of: peer.usePassphrase) { _, enabled in
                        if !enabled && peer.authMode == "passphrase" {
                            peer.passphrase = ""
                            peer.start(secret: "")
                        }
                    }

                    if peer.usePassphrase {
                        SecureField("输入两台设备一致的密码", text: $peer.passphrase)
                            .textFieldStyle(.roundedBorder)
                            .font(.system(size: 12))
                        Button("应用密码并重连") {
                            let trimmed = peer.passphrase.trimmingCharacters(in: .whitespacesAndNewlines)
                            if trimmed.isEmpty {
                                layoutNotice = "请输入共享密码，或关闭开关恢复免密码自动连接。"
                            } else {
                                layoutNotice = nil
                                peer.start(secret: trimmed)
                            }
                        }
                        .buttonStyle(.bordered)
                        .frame(maxWidth: .infinity)
                    }
                }

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
                StatusBadge(title: peer.statusTitle, active: peer.isSharing)
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

            HStack {
                Text(peer.peerName.map { "已连接：\($0)" + (peer.peerAddress.map { " (\($0))" } ?? "") } ?? "正在局域网自动发现设备…")
                Spacer()
                Text("对等架构 · Windows ↔ Mac 双向跨屏")
            }
            .font(.system(size: 12))
            .foregroundStyle(Theme.secondary)
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.background)
    }

    private func commitLayout(_ candidate: SharedLayout, recordUndo: Bool) {
        let previous = peer.sharedLayout
        let doc = LayoutDocument(
            screens: candidate.screens.map {
                Screen(id: $0.id, deviceID: $0.deviceID, x: $0.x, y: $0.y, width: $0.width, height: $0.height)
            },
            extra: [:]
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
            Text("正在自动发现局域网设备")
                .font(.headline)
            Text(peer.authMode == "passphrase"
                 ? "已启用共享密码模式。在另一台电脑设置相同密码后将自动连接并同步屏幕。"
                 : "在另一台电脑打开 Crossflow 后将自动连接、同步屏幕排列并开启共享，无需手动配对。")
                .foregroundStyle(Theme.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 440)
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
