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

private enum RecordedInputEvent: Sendable {
    case keyDown(UInt16)
    case keyUp(UInt16)
    case flagsChanged(NSEvent.ModifierFlags)
    case otherMouseDown(Int)
    case otherMouseUp(Int)
}

@MainActor
private final class HotkeyKeyMonitor {
    static let shared = HotkeyKeyMonitor()
    private var monitor: Any?
    private var pollTimer: Timer?
    private var prevMid = false
    private var prevBack = false
    private var prevForward = false

    func start(onEvent: @escaping @MainActor (RecordedInputEvent) -> Bool) {
        stop()
        let mask: NSEvent.EventTypeMask = [.keyDown, .keyUp, .flagsChanged, .otherMouseDown, .otherMouseUp]
        monitor = NSEvent.addLocalMonitorForEvents(matching: mask) { event in
            let parsed: RecordedInputEvent?
            switch event.type {
            case .keyDown:
                if event.isARepeat { return nil }
                parsed = .keyDown(event.keyCode)
            case .keyUp:
                parsed = .keyUp(event.keyCode)
            case .flagsChanged:
                parsed = .flagsChanged(event.modifierFlags)
            case .otherMouseDown:
                parsed = .otherMouseDown(event.buttonNumber)
            case .otherMouseUp:
                parsed = .otherMouseUp(event.buttonNumber)
            default:
                parsed = nil
            }
            guard let ev = parsed else { return event }
            let consumed = MainActor.assumeIsolated {
                onEvent(ev)
            }
            return consumed ? nil : event
        }
        // Poll CGEventSource button state so Mouse4/Mouse5 (Back/Forward) are always
        // captured even if AppKit/SwiftUI swallows otherMouseDown for side buttons.
        prevMid = CGEventSource.buttonState(.combinedSessionState, button: .center)
        prevBack = CGMouseButton(rawValue: 3).map { CGEventSource.buttonState(.combinedSessionState, button: $0) } ?? false
        prevForward = CGMouseButton(rawValue: 4).map { CGEventSource.buttonState(.combinedSessionState, button: $0) } ?? false
        pollTimer = Timer.scheduledTimer(withTimeInterval: 0.015, repeats: true) { _ in
            Task { @MainActor in
                guard HotkeyKeyMonitor.shared.pollTimer != nil else { return }
                let mid = CGEventSource.buttonState(.combinedSessionState, button: .center)
                let back = CGMouseButton(rawValue: 3).map { CGEventSource.buttonState(.combinedSessionState, button: $0) } ?? false
                let forward = CGMouseButton(rawValue: 4).map { CGEventSource.buttonState(.combinedSessionState, button: $0) } ?? false
                if mid != HotkeyKeyMonitor.shared.prevMid {
                    HotkeyKeyMonitor.shared.prevMid = mid
                    _ = onEvent(mid ? .otherMouseDown(2) : .otherMouseUp(2))
                }
                if back != HotkeyKeyMonitor.shared.prevBack {
                    HotkeyKeyMonitor.shared.prevBack = back
                    _ = onEvent(back ? .otherMouseDown(3) : .otherMouseUp(3))
                }
                if forward != HotkeyKeyMonitor.shared.prevForward {
                    HotkeyKeyMonitor.shared.prevForward = forward
                    _ = onEvent(forward ? .otherMouseDown(4) : .otherMouseUp(4))
                }
            }
        }
    }

    func stop() {
        pollTimer?.invalidate()
        pollTimer = nil
        prevMid = false
        prevBack = false
        prevForward = false
        if let m = monitor {
            NSEvent.removeMonitor(m)
            monitor = nil
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
    @ViewState private var hotkeysExpanded = false
    @ViewState private var recordingHotkeyField: String?
    @ViewState private var recordHeldMods: Set<String> = []
    @ViewState private var recordHeldKeys: Set<String> = []
    @ViewState private var recordPeakMods: Set<String> = []
    @ViewState private var recordPeakKeys: Set<String> = []
    @ViewState private var expandedPeerID: String?
    @ViewState private var addingManualPeer = false
    @ViewState private var manualPeerAddress = ""
    @ViewState private var manualPeerLabel = ""
    @ViewState private var sidebarWidth: CGFloat = 304
    @ViewState private var dragStartSidebarWidth: CGFloat? = nil
    @ViewState private var hoveringSplitter = false

    var body: some View {
        HStack(spacing: 0) {
            sidebar.frame(width: sidebarWidth)
            ZStack {
                Rectangle().fill(Theme.border).frame(width: 1)
                if hoveringSplitter || dragStartSidebarWidth != nil {
                    Rectangle().fill(Theme.accent.opacity(0.65)).frame(width: 2)
                }
            }
            .frame(width: 6)
            .contentShape(Rectangle())
            .onHover { hovering in
                hoveringSplitter = hovering
                if hovering {
                    NSCursor.resizeLeftRight.push()
                } else {
                    NSCursor.pop()
                }
            }
            .gesture(
                DragGesture(minimumDistance: 1, coordinateSpace: .global)
                    .onChanged { value in
                        let base = dragStartSidebarWidth ?? sidebarWidth
                        if dragStartSidebarWidth == nil { dragStartSidebarWidth = base }
                        sidebarWidth = min(540, max(240, base + value.translation.width))
                    }
                    .onEnded { _ in
                        dragStartSidebarWidth = nil
                    }
            )
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
        .onDisappear {
            cancelHotkeyRecording()
            RemoteCursorHider.shared.setHidden(false)
        }
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

                // Multi-device list section (1:1 with Windows)
                Text("设备列表")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.secondary)
                    .padding(.top, 2)

                let displayPeers: [PeerInfo] = {
                    if !peer.peers.isEmpty { return peer.peers }
                    if !peer.discoveredPeers.isEmpty {
                        return peer.discoveredPeers.map {
                            PeerInfo(id: $0.id, name: $0.name, platform: $0.platform, connected: $0.connected, focused: false, slot: $0.connected ? 1 : nil, address: $0.connected ? peer.peerAddress : nil, activeChannelLabel: $0.connected ? peer.activeChannelLabel : nil, preferredChannel: "auto")
                        }
                    }
                    if let pname = peer.peerName, let pid = peer.peerDeviceID {
                        return [PeerInfo(id: pid, name: pname, platform: "windows", connected: true, focused: false, slot: 1, address: peer.peerAddress, activeChannelLabel: peer.activeChannelLabel, preferredChannel: "auto")]
                    }
                    return []
                }()

                if displayPeers.isEmpty {
                    Text("其他电脑打开 Crossflow 后自动发现并连接。")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.secondary)
                } else {
                    VStack(spacing: 6) {
                        ForEach(displayPeers) { dev in
                            let isExpanded = expandedPeerID == dev.id
                            VStack(alignment: .leading, spacing: 8) {
                                HStack(spacing: 8) {
                                    Image(systemName: dev.platform == "macos" ? "laptopcomputer" : "desktopcomputer")
                                        .foregroundStyle(dev.connected ? Color(red: 22/255, green: 163/255, blue: 74/255) : Theme.secondary)
                                    VStack(alignment: .leading, spacing: 2) {
                                        let slotTag = dev.slot.map { "#\($0) " } ?? ""
                                        Text("\(slotTag)\(dev.name)")
                                            .font(.system(size: 13, weight: dev.connected ? .semibold : .regular))
                                            .lineLimit(1)
                                        let sub: String = {
                                            if dev.connected, let addr = dev.address {
                                                if let ch = dev.activeChannelLabel { return "\(ch) · \(addr)" }
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
                                    Text(dev.focused ? "控制中" : (dev.connected ? "已连接" : "已发现"))
                                        .font(.system(size: 11, weight: .medium))
                                        .foregroundStyle(dev.focused ? Theme.accent : (dev.connected ? Color(red: 22/255, green: 163/255, blue: 74/255) : Theme.secondary))
                                    Button {
                                        expandedPeerID = isExpanded ? nil : dev.id
                                    } label: {
                                        Image(systemName: isExpanded ? "chevron.up" : "chevron.down")
                                            .font(.system(size: 10, weight: .semibold))
                                            .foregroundStyle(Theme.secondary)
                                            .padding(4)
                                    }
                                    .buttonStyle(.plain)
                                    .help("该设备信道与快捷操作")
                                }

                                if isExpanded {
                                    VStack(alignment: .leading, spacing: 6) {
                                        Text("此设备信道")
                                            .font(.system(size: 11))
                                            .foregroundStyle(Theme.secondary)
                                        let peerChBinding = Binding<String>(
                                            get: { dev.preferredChannel },
                                            set: { next in
                                                peer.setPeerChannel(deviceID: dev.id, channel: next)
                                            }
                                        )
                                        Picker("", selection: peerChBinding) {
                                            if peer.networkChannels.isEmpty {
                                                Text("跟随全局默认").tag("auto")
                                            } else {
                                                ForEach(Array(peer.networkChannels.enumerated()), id: \.element.id) { idx, ch in
                                                    Text(idx == 0 ? "跟随全局默认" : ch.label).tag(ch.id)
                                                }
                                            }
                                        }
                                        .labelsHidden()
                                        .controlSize(.small)
                                        .frame(maxWidth: .infinity)

                                        if dev.connected {
                                            HStack(spacing: 6) {
                                                Button("QuickJump") {
                                                    peer.quickJump(to: dev.id)
                                                }
                                                .buttonStyle(.bordered)
                                                .controlSize(.small)
                                                .frame(maxWidth: .infinity)

                                                Button("发文件…") {
                                                    pickAndSendFiles(targetDeviceID: dev.id)
                                                }
                                                .buttonStyle(.bordered)
                                                .controlSize(.small)
                                                .frame(maxWidth: .infinity)
                                            }
                                        }
                                    }
                                    .padding(.top, 2)
                                }
                            }
                            .padding(.horizontal, 10)
                            .padding(.vertical, 8)
                            .background(dev.connected ? Theme.subtle : Theme.background, in: RoundedRectangle(cornerRadius: 9))
                            .overlay(RoundedRectangle(cornerRadius: 9).stroke(dev.connected ? Theme.accent : Theme.border, lineWidth: 1))
                        }
                    }
                }

                // Multi-IP Manual Peers list (ZeroTier / cross-subnet static peers)
                if !peer.manualPeers.isEmpty {
                    VStack(spacing: 4) {
                        ForEach(peer.manualPeers) { mp in
                            HStack(spacing: 6) {
                                let title = (mp.label?.isEmpty == false) ? "\(mp.label!) · \(mp.address)" : mp.address
                                Text("手动 · \(title)")
                                    .font(.system(size: 11))
                                    .foregroundStyle(Theme.secondary)
                                    .lineLimit(1)
                                Spacer()
                                Button {
                                    peer.removeManualPeer(address: mp.address)
                                } label: {
                                    Image(systemName: "xmark.circle.fill")
                                        .font(.system(size: 12))
                                        .foregroundStyle(Theme.secondary.opacity(0.75))
                                }
                                .buttonStyle(.plain)
                                .help("移除该手动地址")
                            }
                            .padding(.horizontal, 8)
                            .padding(.vertical, 5)
                            .background(Theme.background, in: RoundedRectangle(cornerRadius: 7))
                            .overlay(RoundedRectangle(cornerRadius: 7).stroke(Theme.border, lineWidth: 1))
                        }
                    }
                }

                Button {
                    addingManualPeer.toggle()
                } label: {
                    Text("+ 手动添加设备 IP")
                        .font(.system(size: 11.5, weight: .medium))
                        .foregroundStyle(Theme.accent)
                }
                .buttonStyle(.plain)

                if addingManualPeer {
                    VStack(spacing: 6) {
                        TextField("对方 IP（如 192.168.194.10）", text: $manualPeerAddress)
                            .textFieldStyle(.roundedBorder)
                            .font(.system(size: 12))
                        TextField("备注名称（可选，如 办公室 Win）", text: $manualPeerLabel)
                            .textFieldStyle(.roundedBorder)
                            .font(.system(size: 12))
                        Button("添加到列表") {
                            let addr = manualPeerAddress.trimmingCharacters(in: .whitespacesAndNewlines)
                            if addr.isEmpty {
                                layoutNotice = "请输入有效的 IPv4 地址（可选 :端口）。"
                                return
                            }
                            layoutNotice = nil
                            peer.addManualPeer(address: addr, label: manualPeerLabel, channel: peer.selectedChannel)
                            manualPeerAddress = ""
                            manualPeerLabel = ""
                            addingManualPeer = false
                        }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                        .frame(maxWidth: .infinity)
                    }
                }

                Divider().overlay(Theme.border).padding(.vertical, 2)

                // Global Default Network Channel selector (1:1 with Windows)
                VStack(alignment: .leading, spacing: 6) {
                    Text("默认网络信道")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.secondary)

                    let channelBinding = Binding<String>(
                        get: { peer.selectedChannel },
                        set: { next in
                            peer.setChannel(next, directPeer: nil)
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

                // Sync toggles: Clipboard + Chinese/English IME & Caps Lock (1:1 with Windows)
                VStack(alignment: .leading, spacing: 6) {
                    Text("共享同步")
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

                    let inputStateBinding = Binding<Bool>(
                        get: { peer.inputStateSync },
                        set: { peer.setInputStateSync($0) }
                    )
                    Toggle(isOn: inputStateBinding) {
                        Text(peer.inputStateSync ? "中英文与大小写同步已开启" : "中英文与大小写同步已关闭")
                            .font(.system(size: 12))
                    }
                    .toggleStyle(.switch)
                    .controlSize(.small)

                    let screenLockBinding = Binding<Bool>(
                        get: { peer.screenLocked },
                        set: { peer.setScreenLock($0) }
                    )
                    Toggle(isOn: screenLockBinding) {
                        Text(peer.screenLocked ? "已锁定当前屏幕（防误触）" : "锁定当前屏幕（防误触）")
                            .font(.system(size: 12))
                    }
                    .toggleStyle(.switch)
                    .controlSize(.small)
                }

                Divider().overlay(Theme.border).padding(.vertical, 2)

                // Customizable Hotkeys (collapsible, 1:1 with Windows)
                hotkeysSection

                Divider().overlay(Theme.border).padding(.vertical, 2)

                // Screens subsection (aligned 1:1 with Windows sidebar!)
                Text("屏幕")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.secondary)

                sidebarScreenList

                Divider().overlay(Theme.border).padding(.vertical, 2)

                let screenHint = peer.hotkeys.quickJumpScreenPrefix.map { "\($0)+1~9" } ?? "未设置"
                Text("快捷键：\(screenHint) 直达屏幕 · \(peer.hotkeys.toggleSharing ?? "未设置") 暂停/恢复 · \(peer.hotkeys.lockScreen ?? "未设置") 锁屏")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(18)
        }
        .frame(maxHeight: .infinity)
        .background(Theme.surface)
    }

    private var screenNumberMap: [String: Int] {
        var map: [String: Int] = [:]
        for info in peer.screenNumbers {
            map[info.screenID] = info.number
        }
        if let layout = peer.sharedLayout, !layout.screens.isEmpty {
            let missing = layout.screens.contains { map[$0.id] == nil }
            if missing {
                let minY = layout.screens.map(\.y).min() ?? 0
                let ordered = layout.screens.sorted { a, b in
                    let ra = Int(floor((a.y - minY) / 360.0))
                    let rb = Int(floor((b.y - minY) / 360.0))
                    if ra != rb { return ra < rb }
                    let xa = Int(a.x.rounded())
                    let xb = Int(b.x.rounded())
                    if xa != xb { return xa < xb }
                    let ya = Int(a.y.rounded())
                    let yb = Int(b.y.rounded())
                    if ya != yb { return ya < yb }
                    return a.id < b.id
                }
                for (i, s) in ordered.enumerated() {
                    map[s.id] = i + 1
                }
            }
        }
        return map
    }

    private var hotkeysSection: some View {
        VStack(alignment: .leading, spacing: 6) {
            Button {
                hotkeysExpanded.toggle()
                if !hotkeysExpanded {
                    cancelHotkeyRecording()
                }
            } label: {
                HStack(spacing: 4) {
                    Text(hotkeysExpanded ? "▾ 快捷键自定义" : "▸ 快捷键自定义")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.secondary)
                    Spacer()
                }
            }
            .buttonStyle(.plain)

            if hotkeysExpanded {
                hotkeyRow(field: "quick_jump_screen_prefix", label: "屏幕 1–9 前缀", value: peer.hotkeys.quickJumpScreenPrefix)
                hotkeyRow(field: "quick_jump_prev_screen", label: "上一个屏幕", value: peer.hotkeys.quickJumpPrevScreen)
                hotkeyRow(field: "quick_jump_next_screen", label: "下一个屏幕", value: peer.hotkeys.quickJumpNextScreen)
                hotkeyRow(field: "quick_jump_home", label: "QuickJump 回本机", value: peer.hotkeys.quickJumpHome)
                hotkeyRow(field: "toggle_sharing", label: "暂停 / 恢复共享", value: peer.hotkeys.toggleSharing)
                hotkeyRow(field: "lock_screen", label: "锁定当前屏幕", value: peer.hotkeys.lockScreen)
                hotkeyRow(field: "emergency_stop", label: "紧急停止", value: peer.hotkeys.emergencyStop)

                HStack {
                    Text("支持任意键盘键 + 鼠标侧键 · Esc 取消")
                        .font(.system(size: 10.5))
                        .foregroundStyle(Theme.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer()
                    Button("恢复默认") {
                        cancelHotkeyRecording()
                        peer.setHotkeys(.defaults)
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.mini)
                }
                .padding(.top, 2)
            }
        }
    }

    private func isMouseToken(_ token: String) -> Bool {
        token == "MouseMid" || token == "Mouse4" || token == "Mouse5"
    }

    private func isStandaloneAllowedToken(_ token: String) -> Bool {
        if isMouseToken(token) || token == "ScrollLock" { return true }
        return token.hasPrefix("F") && token.count >= 2 && token.dropFirst().allSatisfy(\.isNumber)
    }

    private func isDigit1To9Token(_ token: String) -> Bool {
        token.count == 1 && token >= "1" && token <= "9"
    }

    private func formatRecordedCombo(mods: Set<String>, keys: Set<String>) -> String {
        var parts: [String] = []
        if mods.contains("Ctrl") { parts.append("Ctrl") }
        if mods.contains("Alt") { parts.append("Alt") }
        if mods.contains("Shift") { parts.append("Shift") }
        if mods.contains("Meta") { parts.append("Meta") }
        parts.append(contentsOf: keys.filter { !isMouseToken($0) }.sorted())
        parts.append(contentsOf: keys.filter { isMouseToken($0) }.sorted())
        return parts.joined(separator: "+")
    }

    private func hotkeyRow(field: String, label: String, value: String?) -> some View {
        let isRecording = recordingHotkeyField == field
        let isPrefix = field == "quick_jump_screen_prefix"
        let livePreview: String = {
            let mods = (!recordHeldMods.isEmpty || !recordHeldKeys.isEmpty) ? recordHeldMods : recordPeakMods
            let keys = (!recordHeldMods.isEmpty || !recordHeldKeys.isEmpty) ? recordHeldKeys : recordPeakKeys
            return formatRecordedCombo(mods: mods, keys: keys)
        }()
        let displayText: String = {
            if isRecording {
                if !livePreview.isEmpty {
                    return isPrefix ? "\(livePreview) + 1~9" : "\(livePreview) + …"
                }
                return isPrefix ? "按住前缀后松开…" : "按下键/侧键…"
            }
            if let v = value {
                return isPrefix ? "\(v) + 1~9" : v
            }
            return "未设置"
        }()
        return HStack(spacing: 4) {
            Text(label)
                .font(.system(size: 11.5))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
            Spacer()
            Button {
                toggleHotkeyRecording(field)
            } label: {
                Text(displayText)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(isRecording ? Theme.accent : (value == nil ? Theme.secondary : Theme.text))
                    .padding(.horizontal, 7)
                    .padding(.vertical, 3)
                    .frame(minWidth: 86)
                    .background(Theme.background, in: RoundedRectangle(cornerRadius: 6))
                    .overlay(RoundedRectangle(cornerRadius: 6).stroke(isRecording ? Theme.accent : Theme.border, lineWidth: 1))
            }
            .buttonStyle(.plain)
            if value != nil && !isRecording {
                Button {
                    clearHotkey(field)
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.secondary.opacity(0.75))
                }
                .buttonStyle(.plain)
                .help("清空该快捷键")
            }
        }
    }

    private func clearRecordingBuffers() {
        recordHeldMods.removeAll()
        recordHeldKeys.removeAll()
        recordPeakMods.removeAll()
        recordPeakKeys.removeAll()
    }

    private func commitRecordedHotkey(field: String, value: String?) {
        layoutNotice = nil
        cancelHotkeyRecording()
        var updated = peer.hotkeys
        switch field {
        case "quick_jump_screen_prefix":
            updated.quickJumpScreenPrefix = value
            updated.quickJump1 = nil
            updated.quickJump2 = nil
        case "quick_jump_prev_screen": updated.quickJumpPrevScreen = value
        case "quick_jump_next_screen": updated.quickJumpNextScreen = value
        case "toggle_sharing": updated.toggleSharing = value
        case "lock_screen": updated.lockScreen = value
        case "quick_jump_next": updated.quickJumpNext = value
        case "quick_jump_home": updated.quickJumpHome = value
        case "emergency_stop": updated.emergencyStop = value
        default: break
        }
        peer.setHotkeys(updated)
    }

    private func clearHotkey(_ field: String) {
        commitRecordedHotkey(field: field, value: nil)
    }

    private func toggleHotkeyRecording(_ field: String) {
        if recordingHotkeyField == field {
            cancelHotkeyRecording()
            return
        }
        recordingHotkeyField = field
        clearRecordingBuffers()
        peer.setHotkeySuspended(true)
        HotkeyKeyMonitor.shared.start { event in
            handleRecordedInputEvent(event, field: field)
        }
    }

    private func cancelHotkeyRecording() {
        if recordingHotkeyField != nil {
            recordingHotkeyField = nil
            clearRecordingBuffers()
            peer.setHotkeySuspended(false)
        }
        HotkeyKeyMonitor.shared.stop()
    }

    private func handleRecordedInputEvent(_ event: RecordedInputEvent, field: String) -> Bool {
        let isPrefix = field == "quick_jump_screen_prefix"

        func checkAllReleased() {
            guard recordHeldMods.isEmpty && recordHeldKeys.isEmpty else { return }
            if isPrefix {
                if !recordPeakMods.isEmpty || !recordPeakKeys.isEmpty {
                    let prefix = formatRecordedCombo(mods: recordPeakMods, keys: recordPeakKeys)
                    commitRecordedHotkey(field: field, value: prefix)
                }
                return
            }
            if recordPeakMods.isEmpty && recordPeakKeys.count == 1, let only = recordPeakKeys.first {
                if isStandaloneAllowedToken(only) {
                    commitRecordedHotkey(field: field, value: only)
                } else {
                    clearRecordingBuffers()
                    layoutNotice = "单个普通键请搭配修饰键或鼠标侧键（如 Alt+Mouse4、Q+Mouse5）。"
                }
            }
        }

        func handleKeyDownToken(_ token: String) -> Bool {
            if recordHeldKeys.contains(token) { return true }
            let noOtherHeld = recordHeldMods.isEmpty && recordHeldKeys.isEmpty
            if noOtherHeld && token == "Esc" {
                cancelHotkeyRecording()
                return true
            }
            if noOtherHeld && (token == "Backspace" || token == "Delete") {
                commitRecordedHotkey(field: field, value: nil)
                return true
            }
            if isPrefix && (token == "Esc" || isDigit1To9Token(token)) {
                layoutNotice = "屏幕 1–9 前缀不能包含数字 1–9 或 Esc。"
                return true
            }
            recordHeldKeys.insert(token)
            recordPeakKeys.insert(token)
            if isPrefix {
                return true
            }
            if !recordHeldMods.isEmpty || recordHeldKeys.count >= 2 {
                let chord = formatRecordedCombo(mods: recordHeldMods, keys: recordHeldKeys)
                commitRecordedHotkey(field: field, value: chord)
            }
            return true
        }

        func handleKeyUpToken(_ token: String) -> Bool {
            recordHeldKeys.remove(token)
            checkAllReleased()
            return true
        }

        switch event {
        case .flagsChanged(let rawFlags):
            let flags = rawFlags.intersection([.control, .option, .shift, .command])
            var currentMods: Set<String> = []
            if flags.contains(.control) { currentMods.insert("Ctrl") }
            if flags.contains(.option) { currentMods.insert("Alt") }
            if flags.contains(.shift) { currentMods.insert("Shift") }
            if flags.contains(.command) { currentMods.insert("Meta") }
            recordHeldMods = currentMods
            recordPeakMods.formUnion(currentMods)
            checkAllReleased()
            return true
        case .keyDown(let keyCode):
            guard let token = macKeyCodeToChordName(keyCode) else { return true }
            return handleKeyDownToken(token)
        case .keyUp(let keyCode):
            guard let token = macKeyCodeToChordName(keyCode) else { return true }
            return handleKeyUpToken(token)
        case .otherMouseDown(let buttonNumber):
            guard let token = macMouseButtonToToken(buttonNumber) else { return false }
            return handleKeyDownToken(token)
        case .otherMouseUp(let buttonNumber):
            guard let token = macMouseButtonToToken(buttonNumber) else { return false }
            return handleKeyUpToken(token)
        }
    }

    private func macMouseButtonToToken(_ buttonNumber: Int) -> String? {
        switch buttonNumber {
        case 2: "MouseMid"
        case 3: "Mouse4"
        case 4: "Mouse5"
        default: nil
        }
    }

    private func macKeyCodeToChordName(_ code: UInt16) -> String? {
        switch code {
        case 0: "A"; case 11: "B"; case 8: "C"; case 2: "D"; case 14: "E"
        case 3: "F"; case 5: "G"; case 4: "H"; case 34: "I"; case 38: "J"
        case 40: "K"; case 37: "L"; case 46: "M"; case 45: "N"; case 31: "O"
        case 35: "P"; case 12: "Q"; case 15: "R"; case 1: "S"; case 17: "T"
        case 32: "U"; case 9: "V"; case 13: "W"; case 7: "X"; case 16: "Y"
        case 6: "Z"
        case 18: "1"; case 19: "2"; case 20: "3"; case 21: "4"; case 23: "5"
        case 22: "6"; case 26: "7"; case 28: "8"; case 25: "9"; case 29: "0"
        case 36: "Enter"; case 53: "Esc"; case 51: "Backspace"; case 48: "Tab"; case 49: "Space"
        case 27: "-"; case 24: "="; case 33: "["; case 30: "]"; case 42: "\\"
        case 41: ";"; case 39: "'"; case 50: "`"; case 43: ","; case 47: "."; case 44: "/"
        case 122: "F1"; case 120: "F2"; case 99: "F3"; case 118: "F4"
        case 96: "F5"; case 97: "F6"; case 98: "F7"; case 100: "F8"
        case 101: "F9"; case 109: "F10"; case 103: "F11"; case 111: "F12"
        case 105: "F13"; case 107: "F14"; case 113: "F15"; case 106: "F16"
        case 64: "F17"; case 79: "F18"; case 80: "F19"; case 90: "F20"
        case 115: "Home"; case 116: "PageUp"; case 117: "Delete"
        case 119: "End"; case 121: "PageDown"
        case 123: "Left"; case 124: "Right"; case 125: "Down"; case 126: "Up"
        default: nil
        }
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
                    let peerMatch = peer.peers.first { $0.id == devID }
                    let devLabel = isLocal ? "\(peer.localName) (本机)" : (peerMatch?.name ?? peer.peerName ?? "远端设备")
                    let color: Color = {
                        if isLocal { return Theme.accent }
                        switch idx % 4 {
                        case 0: return Theme.accent
                        case 1: return Color(red: 13/255, green: 148/255, blue: 136/255)
                        case 2: return Color(red: 124/255, green: 58/255, blue: 237/255)
                        default: return Color(red: 217/255, green: 119/255, blue: 6/255)
                        }
                    }()
                    Text("\(devLabel) · \(group.count) 屏")
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(color)
                        .padding(.top, 2)
                    ForEach(Array(group.enumerated()), id: \.element.id) { sIdx, screen in
                        let focused = peer.routeEnabled && peer.focusScreenID == screen.id
                        let globalNum = screenNumberMap[screen.id] ?? 0
                        HStack(spacing: 8) {
                            Text(globalNum > 0 ? "\(globalNum)" : "·")
                                .font(.system(size: 11, weight: .bold))
                                .foregroundStyle(.white)
                                .frame(width: 20, height: 20)
                                .background(color, in: Circle())
                            Text("屏幕 \(sIdx + 1) (\(Int(screen.width))×\(Int(screen.height)))")
                                .font(.system(size: 12))
                                .lineLimit(1)
                            Spacer()
                            if peer.state == "sharing" {
                                Button {
                                    peer.quickJumpScreen(screen.id)
                                } label: {
                                    Image(systemName: "arrow.up.right.square")
                                        .font(.system(size: 12))
                                        .foregroundStyle(color)
                                }
                                .buttonStyle(.plain)
                                .help(globalNum >= 1 && globalNum <= 9 ? "QuickJump 至屏幕 #\(globalNum)" : "跳转至此屏幕")
                            }
                        }
                        .padding(.horizontal, 8)
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
                        Text("\(idx + 1)")
                            .font(.system(size: 11, weight: .bold))
                            .foregroundStyle(.white)
                            .frame(width: 20, height: 20)
                            .background(Theme.accent, in: Circle())
                        Text("屏幕 \(idx + 1) (\(Int(d.width))×\(Int(d.height)))")
                            .font(.system(size: 12))
                            .lineLimit(1)
                        Spacer()
                    }
                    .padding(.horizontal, 8)
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
                let nameMap = Dictionary(uniqueKeysWithValues: peer.peers.map { ($0.id, $0.name) })
                SessionLayoutView(
                    layout: layout,
                    focusScreenID: peer.focusScreenID,
                    routeEnabled: peer.routeEnabled,
                    sourceName: peer.peerName ?? "另一台电脑",
                    localDeviceID: peer.localDeviceID,
                    localName: peer.localName,
                    deviceNames: nameMap,
                    screenNumbers: screenNumberMap,
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

    private final class DroppedPathsBox: @unchecked Sendable {
        private let lock = NSLock()
        private var paths: [String] = []

        func append(_ path: String) {
            lock.withLock { paths.append(path) }
        }

        func snapshot() -> [String] {
            lock.withLock { paths }
        }
    }

    private func pickAndSendFiles(targetDeviceID: String?) {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = true
        panel.prompt = "发送"
        panel.title = "选择要发送的文件或文件夹"
        if panel.runModal() == .OK {
            let paths = panel.urls.map { (($0 as NSURL).filePathURL ?? $0.standardizedFileURL).path }
            if !paths.isEmpty {
                peer.sendFiles(paths, targetDeviceID: targetDeviceID)
            }
        }
    }

    private func handleFileDrop(_ providers: [NSItemProvider]) {
        let group = DispatchGroup()
        let box = DroppedPathsBox()
        for provider in providers where provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
            group.enter()
            provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier, options: nil) { item, _ in
                defer { group.leave() }
                if let data = item as? Data, let url = URL(dataRepresentation: data, relativeTo: nil) {
                    let resolved = (url as NSURL).filePathURL ?? url.standardizedFileURL
                    box.append(resolved.path)
                } else if let url = item as? URL {
                    let resolved = (url as NSURL).filePathURL ?? url.standardizedFileURL
                    box.append(resolved.path)
                }
            }
        }
        group.notify(queue: .main) {
            let paths = box.snapshot()
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
