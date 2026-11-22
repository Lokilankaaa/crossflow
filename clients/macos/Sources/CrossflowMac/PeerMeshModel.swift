import Foundation
import Observation

struct NetworkChannelInfo: Sendable, Equatable, Decodable, Identifiable {
    let id: String
    let label: String
    let kind: String
    let ip: String?
    let interfaceName: String?

    enum CodingKeys: String, CodingKey {
        case id, label, kind, ip
        case interfaceName = "interface_name"
    }
}

struct PeerDisplayInfo: Sendable, Equatable, Decodable, Identifiable {
    let nativeID: String
    let wireID: UInt32
    let x: Double
    let y: Double
    let width: Double
    let height: Double
    var id: String { nativeID }

    enum CodingKeys: String, CodingKey {
        case nativeID = "native_id"
        case wireID = "wire_id"
        case x, y, width, height
    }
}

struct DiscoveredPeerInfo: Sendable, Equatable, Decodable, Identifiable {
    let id: String
    let name: String
    let platform: String
    let connected: Bool
}

struct PeerSharedScreenWire: Sendable, Equatable, Codable {
    let id: String
    let deviceID: String
    let nativeID: String
    let x: Double
    let y: Double
    let width: Double
    let height: Double

    enum CodingKeys: String, CodingKey {
        case id
        case deviceID = "device_id"
        case nativeID = "native_id"
        case x, y, width, height
    }
}

struct PeerSharedLayoutWire: Sendable, Equatable, Codable {
    let schema: Int
    let sourceDeviceID: String
    let receiverDeviceID: String
    let screens: [PeerSharedScreenWire]

    enum CodingKeys: String, CodingKey {
        case schema
        case sourceDeviceID = "source_device_id"
        case receiverDeviceID = "receiver_device_id"
        case screens
    }

    var toSharedLayout: SharedLayout {
        SharedLayout(
            sourceDeviceID: sourceDeviceID,
            receiverDeviceID: receiverDeviceID,
            screens: screens.map {
                SharedScreen(id: $0.id, deviceID: $0.deviceID, nativeID: $0.nativeID,
                             x: $0.x, y: $0.y, width: $0.width, height: $0.height)
            }
        )
    }

    init(from layout: SharedLayout) {
        self.schema = 1
        self.sourceDeviceID = layout.sourceDeviceID
        self.receiverDeviceID = layout.receiverDeviceID
        self.screens = layout.screens.map {
            PeerSharedScreenWire(id: $0.id, deviceID: $0.deviceID, nativeID: $0.nativeID,
                                 x: $0.x, y: $0.y, width: $0.width, height: $0.height)
        }
    }
}

struct TransferStatusInfo: Sendable, Equatable, Decodable {
    let direction: String
    let state: String
    let name: String
    let bytesDone: UInt64
    let bytesTotal: UInt64
    let filesDone: Int
    let filesTotal: Int
    let savedDir: String?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case direction, state, name, error
        case bytesDone = "bytes_done"
        case bytesTotal = "bytes_total"
        case filesDone = "files_done"
        case filesTotal = "files_total"
        case savedDir = "saved_dir"
    }
}

struct ManualPeerInfo: Sendable, Equatable, Decodable, Identifiable {
    let address: String
    let label: String?
    let channel: String
    var id: String { address }
}

struct PeerInfo: Sendable, Equatable, Decodable, Identifiable {
    let id: String
    let name: String
    let platform: String
    let connected: Bool
    let focused: Bool
    let slot: Int?
    let address: String?
    let activeChannelLabel: String?
    let preferredChannel: String

    enum CodingKeys: String, CodingKey {
        case id, name, platform, connected, focused, slot, address
        case activeChannelLabel = "active_channel_label"
        case preferredChannel = "preferred_channel"
    }
}

struct NumberedScreenInfo: Sendable, Equatable, Decodable, Identifiable {
    let number: Int
    let screenID: String
    let deviceID: String
    let deviceName: String
    let nativeID: String
    var id: String { screenID }

    enum CodingKeys: String, CodingKey {
        case number
        case screenID = "screen_id"
        case deviceID = "device_id"
        case deviceName = "device_name"
        case nativeID = "native_id"
    }
}

struct HotkeyConfigInfo: Sendable, Equatable, Codable {
    var toggleSharing: String?
    var lockScreen: String?
    var quickJumpNext: String?
    var quickJumpPrevScreen: String?
    var quickJumpNextScreen: String?
    var quickJumpHome: String?
    var quickJumpScreenPrefix: String?
    var quickJump1: String?
    var quickJump2: String?
    var emergencyStop: String?

    enum CodingKeys: String, CodingKey {
        case toggleSharing = "toggle_sharing"
        case lockScreen = "lock_screen"
        case quickJumpNext = "quick_jump_next"
        case quickJumpPrevScreen = "quick_jump_prev_screen"
        case quickJumpNextScreen = "quick_jump_next_screen"
        case quickJumpHome = "quick_jump_home"
        case quickJumpScreenPrefix = "quick_jump_screen_prefix"
        case quickJump1 = "quick_jump_1"
        case quickJump2 = "quick_jump_2"
        case emergencyStop = "emergency_stop"
    }

    // Explicitly encode nil as JSON `null` (default Codable omits nil keys, which
    // causes Rust serde's `#[serde(default = "...")]` to restore the default chord).
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(toggleSharing, forKey: .toggleSharing)
        try c.encode(lockScreen, forKey: .lockScreen)
        try c.encode(quickJumpNext, forKey: .quickJumpNext)
        try c.encode(quickJumpPrevScreen, forKey: .quickJumpPrevScreen)
        try c.encode(quickJumpNextScreen, forKey: .quickJumpNextScreen)
        try c.encode(quickJumpHome, forKey: .quickJumpHome)
        try c.encode(quickJumpScreenPrefix, forKey: .quickJumpScreenPrefix)
        try c.encode(quickJump1, forKey: .quickJump1)
        try c.encode(quickJump2, forKey: .quickJump2)
        try c.encode(emergencyStop, forKey: .emergencyStop)
    }

    static let defaults = HotkeyConfigInfo(
        toggleSharing: "Ctrl+Alt+S",
        lockScreen: "Ctrl+Alt+L",
        quickJumpNext: "Ctrl+Alt+Tab",
        quickJumpPrevScreen: nil,
        quickJumpNextScreen: nil,
        quickJumpHome: "Ctrl+Alt+H",
        quickJumpScreenPrefix: "Ctrl+Alt",
        quickJump1: nil,
        quickJump2: nil,
        emergencyStop: "Ctrl+Alt+Shift+Esc"
    )
}

struct PeerStatusWire: Sendable, Equatable, Decodable {
    let protocolVersion: Int
    let state: String
    let localDeviceID: String
    let localName: String
    let authMode: String
    let passphraseConfigured: Bool
    let smartKeymap: Bool
    let clipboardSync: Bool
    let inputStateSync: Bool?
    let screenLocked: Bool?
    let hotkeys: HotkeyConfigInfo?
    let selectedChannel: String
    let directPeer: String?
    let activeChannelLabel: String?
    let networkChannels: [NetworkChannelInfo]
    let manualPeers: [ManualPeerInfo]?
    let peers: [PeerInfo]?
    let routeEnabled: Bool
    let focusDeviceID: String?
    let focusScreenID: String?
    let peerDeviceID: String?
    let peerName: String?
    let peerAddress: String?
    let listenEndpoint: String?
    let localDisplays: [PeerDisplayInfo]
    let discoveredPeers: [DiscoveredPeerInfo]
    let sharedLayout: PeerSharedLayoutWire?
    let screenNumbers: [NumberedScreenInfo]?
    let transfer: TransferStatusInfo?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol"
        case state
        case localDeviceID = "local_device_id"
        case localName = "local_name"
        case authMode = "auth_mode"
        case passphraseConfigured = "passphrase_configured"
        case smartKeymap = "smart_keymap"
        case clipboardSync = "clipboard_sync"
        case inputStateSync = "input_state_sync"
        case screenLocked = "screen_locked"
        case hotkeys
        case selectedChannel = "selected_channel"
        case directPeer = "direct_peer"
        case activeChannelLabel = "active_channel_label"
        case networkChannels = "network_channels"
        case manualPeers = "manual_peers"
        case peers
        case routeEnabled = "route_enabled"
        case focusDeviceID = "focus_device_id"
        case focusScreenID = "focus_screen_id"
        case peerDeviceID = "peer_device_id"
        case peerName = "peer_name"
        case peerAddress = "peer_address"
        case listenEndpoint = "listen_endpoint"
        case localDisplays = "local_displays"
        case discoveredPeers = "discovered_peers"
        case sharedLayout = "shared_layout"
        case screenNumbers = "screen_numbers"
        case transfer
        case error
    }
}

private struct ApplyLayoutCommandWire: Encodable {
    let op = "apply_layout"
    let layout: PeerSharedLayoutWire
}

private struct SetChannelCommandWire: Encodable {
    let op = "set_channel"
    let channel: String
    let directPeer: String?

    enum CodingKeys: String, CodingKey {
        case op, channel
        case directPeer = "direct_peer"
    }
}

private struct SetPeerChannelCommandWire: Encodable {
    let op = "set_peer_channel"
    let deviceID: String
    let channel: String

    enum CodingKeys: String, CodingKey {
        case op
        case deviceID = "device_id"
        case channel
    }
}

private struct AddManualPeerCommandWire: Encodable {
    let op = "add_manual_peer"
    let address: String
    let label: String?
    let channel: String
}

private struct RemoveManualPeerCommandWire: Encodable {
    let op = "remove_manual_peer"
    let address: String
}

private struct SetPassphraseCommandWire: Encodable {
    let op = "set_passphrase"
    let passphrase: String?
}

private struct SetKeymapCommandWire: Encodable {
    let op = "set_keymap"
    let enabled: Bool
}

private struct SetClipboardCommandWire: Encodable {
    let op = "set_clipboard"
    let enabled: Bool
}

private struct SetInputStateSyncCommandWire: Encodable {
    let op = "set_input_state_sync"
    let enabled: Bool
}

private struct SetHotkeysCommandWire: Encodable {
    let op = "set_hotkeys"
    let hotkeys: HotkeyConfigInfo
}

private struct SetHotkeySuspendedCommandWire: Encodable {
    let op = "set_hotkey_suspended"
    let suspended: Bool
}

private struct SetScreenLockCommandWire: Encodable {
    let op = "set_screen_lock"
    let locked: Bool
}

private struct QuickJumpCommandWire: Encodable {
    let op = "quick_jump"
    let targetDeviceID: String?

    enum CodingKeys: String, CodingKey {
        case op
        case targetDeviceID = "target_device_id"
    }
}

private struct QuickJumpScreenCommandWire: Encodable {
    let op = "quick_jump_screen"
    let screenID: String

    enum CodingKeys: String, CodingKey {
        case op
        case screenID = "screen_id"
    }
}

private struct SendFilesCommandWire: Encodable {
    let op = "send_files"
    let targetDeviceID: String?
    let paths: [String]

    enum CodingKeys: String, CodingKey {
        case op
        case targetDeviceID = "target_device_id"
        case paths
    }
}

private final class PeerProcessRunner: @unchecked Sendable {
    private let queue = DispatchQueue(label: "local.crossflow.peer-mesh", qos: .userInitiated)
    private let lock = NSLock()
    private var process: Process?
    private var stdinPipe: Pipe?
    private var stopRequested = false

    func start(
        executable: URL,
        arguments: [String],
        onStatus: @escaping @Sendable (PeerStatusWire) -> Void,
        onExit: @escaping @Sendable (Int32, Bool) -> Void
    ) {
        queue.async {
            self.stopSyncLocked()
            let proc = Process()
            proc.executableURL = executable
            proc.arguments = arguments
            var env = ProcessInfo.processInfo.environment
            env.removeValue(forKey: "CROSSFLOW_LAN_PASSPHRASE")
            proc.environment = env

            let stdinPipe = Pipe()
            let stdoutPipe = Pipe()
            let stderrPipe = Pipe()
            proc.standardInput = stdinPipe
            proc.standardOutput = stdoutPipe
            proc.standardError = stderrPipe

            self.lock.withLock {
                self.process = proc
                self.stdinPipe = stdinPipe
                self.stopRequested = false
            }

            do {
                try proc.run()
            } catch {
                self.lock.withLock {
                    self.process = nil
                    self.stdinPipe = nil
                }
                onExit(-1, false)
                return
            }

            DispatchQueue.global(qos: .utility).async {
                let handle = stderrPipe.fileHandleForReading
                while true {
                    let chunk = handle.availableData
                    if chunk.isEmpty { break }
                }
            }

            DispatchQueue.global(qos: .userInitiated).async {
                let handle = stdoutPipe.fileHandleForReading
                var buffer = Data()
                let decoder = JSONDecoder()
                while true {
                    let chunk = handle.availableData
                    if chunk.isEmpty { break }
                    buffer.append(chunk)
                    while let nl = buffer.firstIndex(of: 0x0A) {
                        let line = buffer.prefix(upTo: nl)
                        buffer.removeSubrange(buffer.startIndex...nl)
                        if line.isEmpty { continue }
                        if let parsed = try? decoder.decode(PeerStatusWire.self, from: Data(line)),
                           parsed.protocolVersion == 3 {
                            onStatus(parsed)
                        }
                    }
                }
                proc.waitUntilExit()
                let wasStopped = self.lock.withLock { () -> Bool in
                    let stopped = self.stopRequested
                    if self.process === proc {
                        self.process = nil
                        self.stdinPipe = nil
                    }
                    return stopped
                }
                onExit(proc.terminationStatus, wasStopped)
            }
        }
    }

    func send(_ line: Data) {
        queue.async {
            let pipe = self.lock.withLock { self.stopRequested ? nil : self.stdinPipe }
            guard let handle = pipe?.fileHandleForWriting else { return }
            var payload = line
            if payload.last != 0x0A { payload.append(0x0A) }
            try? handle.write(contentsOf: payload)
        }
    }

    func stop() async -> Bool {
        await withCheckedContinuation { continuation in
            queue.async {
                self.stopSyncLocked()
                continuation.resume(returning: true)
            }
        }
    }

    private func stopSyncLocked() {
        let (proc, pipe) = lock.withLock { () -> (Process?, Pipe?) in
            self.stopRequested = true
            return (self.process, self.stdinPipe)
        }
        guard let proc else { return }
        if let handle = pipe?.fileHandleForWriting {
            try? handle.write(contentsOf: Data("{\"op\":\"stop\"}\n".utf8))
            try? handle.close()
        }
        let deadline = Date().addingTimeInterval(3.0)
        while proc.isRunning && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.05)
        }
        if proc.isRunning {
            proc.terminate()
            let killDeadline = Date().addingTimeInterval(1.5)
            while proc.isRunning && Date() < killDeadline {
                Thread.sleep(forTimeInterval: 0.05)
            }
        }
        lock.withLock {
            if self.process === proc {
                self.process = nil
                self.stdinPipe = nil
            }
        }
    }
}

@MainActor @Observable
final class PeerMeshModel {
    var state: String = "starting"
    var localDeviceID: String?
    var localName: String = Host.current().localizedName ?? "这台 Mac"
    var authMode: String = "open"
    var passphraseConfigured: Bool = false
    var smartKeymap: Bool = true
    var clipboardSync: Bool = true
    var inputStateSync: Bool = true
    var screenLocked: Bool = false
    var hotkeys: HotkeyConfigInfo = .defaults
    var selectedChannel: String = "auto"
    var directPeer: String = ""
    var activeChannelLabel: String?
    var networkChannels: [NetworkChannelInfo] = []
    var manualPeers: [ManualPeerInfo] = []
    var peers: [PeerInfo] = []
    var routeEnabled: Bool = false
    var focusDeviceID: String?
    var focusScreenID: String?
    var peerDeviceID: String?
    var peerName: String?
    var peerAddress: String?
    var listenEndpoint: String?
    var localDisplays: [PeerDisplayInfo] = []
    var discoveredPeers: [DiscoveredPeerInfo] = []
    var sharedLayout: SharedLayout?
    var screenNumbers: [NumberedScreenInfo] = []
    var transfer: TransferStatusInfo?
    var error: String?

    @ObservationIgnored private let runner = PeerProcessRunner()
    @ObservationIgnored private var generation: Int = 0
    @ObservationIgnored private var shuttingDown = false
    @ObservationIgnored private var syncedDirectPeerOnce = false
    @ObservationIgnored var onStatusChange: (@MainActor () -> Void)?

    var isSharing: Bool { state == "sharing" && routeEnabled }
    var isConnected: Bool { state == "sharing" || state == "paused" }

    var statusTitle: String {
        switch state {
        case "sharing": "跨屏共享中"
        case "paused": "已连接 · 已暂停"
        case "searching": "正在发现设备"
        case "starting": "正在启动"
        case "startup_rejected": "需要系统权限"
        case "error": "连接异常"
        case "stopped": "已停止"
        default: "等待中"
        }
    }

    var statusDetail: String {
        if state == "startup_rejected" {
            let perm = (error ?? "").contains("accessibility") ? "设备控制和数据访问（辅助功能）" : "输入监控"
            return "请在系统设置 → 隐私与安全性 → \(perm) 中允许 Crossflow，然后点击重新检测。"
        }
        if let err = error, !err.isEmpty { return err }
        if state == "sharing" {
            let route = activeChannelLabel.map { ch in
                peerAddress.map { "\(ch) · \($0)" } ?? ch
            } ?? (peerAddress ?? "")
            if let focusDev = focusDeviceID {
                let devLabel = (focusDev == localDeviceID) ? "这台 Mac" : (peerName ?? "另一台电脑")
                return route.isEmpty ? "当前控制：\(devLabel)" : "当前控制：\(devLabel)\n\(route)"
            }
            return "已连接 \(peerName ?? "远端设备")" + (route.isEmpty ? "" : "（\(route)）")
        }
        if state == "paused" {
            return "已连接 \(peerName ?? "远端设备") · 点击恢复共享"
        }
        if state == "searching" {
            return "正在自动搜索附近设备…"
        }
        return "正在初始化自动发现…"
    }

    func start() {
        guard !shuttingDown else { return }
        guard let exe = crossflowExecutable() else {
            state = "error"
            error = "缺少随应用打包的共享内核（crossflow）。"
            return
        }
        generation += 1
        let gen = generation
        state = "starting"
        routeEnabled = false
        focusDeviceID = nil
        focusScreenID = nil
        error = nil

        let logURL = peerLogURL()
        let args = [
            "peer",
            "--bind", "0.0.0.0:24800",
            "--seconds", "86400",
            "--max-hz", "1000",
            "--name", Host.current().localizedName ?? "Mac",
            "--log", logURL.path
        ]

        runner.start(
            executable: exe,
            arguments: args,
            onStatus: { [weak self] status in
                Task { @MainActor [weak self] in
                    guard let self, self.generation == gen else { return }
                    self.apply(status)
                }
            },
            onExit: { [weak self] _, wasStopped in
                Task { @MainActor [weak self] in
                    guard let self, self.generation == gen, !self.shuttingDown else { return }
                    if !wasStopped && self.state != "startup_rejected" && self.state != "error" {
                        self.state = "stopped"
                        self.routeEnabled = false
                        self.focusDeviceID = nil
                        self.focusScreenID = nil
                    }
                }
            }
        )
    }

    private func apply(_ s: PeerStatusWire) {
        state = s.state
        localDeviceID = s.localDeviceID
        localName = s.localName
        authMode = s.authMode
        passphraseConfigured = s.passphraseConfigured
        smartKeymap = s.smartKeymap
        clipboardSync = s.clipboardSync
        inputStateSync = s.inputStateSync ?? true
        screenLocked = s.screenLocked ?? false
        if let hk = s.hotkeys {
            hotkeys = hk
        }
        selectedChannel = s.selectedChannel
        if !syncedDirectPeerOnce {
            syncedDirectPeerOnce = true
            directPeer = s.directPeer ?? ""
        }
        activeChannelLabel = s.activeChannelLabel
        networkChannels = s.networkChannels
        manualPeers = s.manualPeers ?? []
        peers = s.peers ?? []
        routeEnabled = s.routeEnabled
        focusDeviceID = s.focusDeviceID
        focusScreenID = s.focusScreenID
        peerDeviceID = s.peerDeviceID
        peerName = s.peerName
        peerAddress = s.peerAddress
        listenEndpoint = s.listenEndpoint
        localDisplays = s.localDisplays
        discoveredPeers = s.discoveredPeers
        sharedLayout = s.sharedLayout?.toSharedLayout
        screenNumbers = s.screenNumbers ?? []
        transfer = s.transfer
        error = s.error
        onStatusChange?()
    }

    func pause() {
        runner.send(Data("{\"op\":\"pause\"}".utf8))
    }

    func resume() {
        runner.send(Data("{\"op\":\"resume\"}".utf8))
    }

    func setChannel(_ channel: String, directPeer peerIP: String?) {
        let trimmed = peerIP?.trimmingCharacters(in: .whitespacesAndNewlines)
        let cmd = SetChannelCommandWire(
            channel: channel,
            directPeer: (trimmed?.isEmpty == false) ? trimmed : nil
        )
        if let data = try? JSONEncoder().encode(cmd) {
            selectedChannel = channel
            runner.send(data)
        }
    }

    func setPeerChannel(deviceID: String, channel: String) {
        let cmd = SetPeerChannelCommandWire(deviceID: deviceID, channel: channel)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func addManualPeer(address: String, label: String?, channel: String = "auto") {
        let addr = address.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !addr.isEmpty else { return }
        let cleanLabel = label?.trimmingCharacters(in: .whitespacesAndNewlines)
        let cmd = AddManualPeerCommandWire(
            address: addr,
            label: (cleanLabel?.isEmpty == false) ? cleanLabel : nil,
            channel: channel
        )
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func removeManualPeer(address: String) {
        let cmd = RemoveManualPeerCommandWire(address: address)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func setPassphrase(_ secret: String?) {
        let trimmed = secret?.trimmingCharacters(in: .whitespacesAndNewlines)
        let cmd = SetPassphraseCommandWire(passphrase: (trimmed?.isEmpty == false) ? trimmed : nil)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func setKeymap(_ enabled: Bool) {
        smartKeymap = enabled
        let cmd = SetKeymapCommandWire(enabled: enabled)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func setClipboard(_ enabled: Bool) {
        clipboardSync = enabled
        let cmd = SetClipboardCommandWire(enabled: enabled)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func setInputStateSync(_ enabled: Bool) {
        inputStateSync = enabled
        let cmd = SetInputStateSyncCommandWire(enabled: enabled)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func setHotkeys(_ updated: HotkeyConfigInfo) {
        hotkeys = updated
        let cmd = SetHotkeysCommandWire(hotkeys: updated)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func setHotkeySuspended(_ suspended: Bool) {
        let cmd = SetHotkeySuspendedCommandWire(suspended: suspended)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func setScreenLock(_ locked: Bool) {
        screenLocked = locked
        let cmd = SetScreenLockCommandWire(locked: locked)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func quickJump(to targetDeviceID: String?) {
        let cmd = QuickJumpCommandWire(targetDeviceID: targetDeviceID)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func quickJumpScreen(_ screenID: String) {
        let cmd = QuickJumpScreenCommandWire(screenID: screenID)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func sendFiles(_ paths: [String], targetDeviceID: String? = nil) {
        guard !paths.isEmpty else { return }
        let cmd = SendFilesCommandWire(targetDeviceID: targetDeviceID, paths: paths)
        if let data = try? JSONEncoder().encode(cmd) {
            runner.send(data)
        }
    }

    func cancelTransfer() {
        runner.send(Data("{\"op\":\"cancel_transfer\"}".utf8))
    }

    func applyLayout(_ layout: SharedLayout) {
        let cmd = ApplyLayoutCommandWire(layout: PeerSharedLayoutWire(from: layout))
        if let data = try? JSONEncoder().encode(cmd) {
            sharedLayout = layout
            runner.send(data)
        }
    }

    func shutdown() async -> Bool {
        shuttingDown = true
        generation += 1
        return await runner.stop()
    }

    private func crossflowExecutable() -> URL? {
        guard let url = Bundle.main.executableURL?.deletingLastPathComponent().appendingPathComponent("crossflow"),
              FileManager.default.isExecutableFile(atPath: url.path) else { return nil }
        return url
    }

    private func peerLogURL() -> URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let dir = base.appendingPathComponent("Crossflow/Logs", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let formatter = DateFormatter()
        formatter.dateFormat = "yyyyMMdd-HHmmss"
        let stamp = formatter.string(from: Date())
        return dir.appendingPathComponent("peer-\(stamp)-\(UUID().uuidString.prefix(8)).jsonl")
    }
}
