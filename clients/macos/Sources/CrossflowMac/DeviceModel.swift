import Foundation
import Observation

private final class DeviceOutput: @unchecked Sendable {
    private let lock = NSLock()
    private var data = Data()
    private var oversized = false
    func append(_ line: Data) {
        lock.withLock {
            guard data.count + line.count + 1 <= 1_048_576 else { oversized = true; return }
            data.append(line); data.append(10)
        }
    }
    func result() throws -> Data {
        try lock.withLock {
            guard !oversized else { throw DeviceModelError.invalid }
            return data
        }
    }
}

private enum DeviceModelError: Error { case invalid }

@MainActor @Observable
final class DeviceModel {
    struct Confirmation {
        let attempt: String
        let name: String
        let fingerprint: String
    }
    enum Operation { case invitation, receiver }
    var devices: [SavedDevice] = []
    var selectedID: String?
    var deviceName = "这台 Mac"
    var includeKeyboard = false
    var code = ""
    var confirmation: Confirmation?
    var approved = false
    var message = "选择已配对电脑，或添加新电脑。"
    var listMessage = ""
    var loading = false
    var operation: Operation?
    var receiverState: ReceiverStatus.State?
    var cleanupUnconfirmed = false
    var stopping = false
    var stopPending = false
    @ObservationIgnored private let process = NativeProcessClient()
    @ObservationIgnored private let listing = NativeProcessClient()
    @ObservationIgnored private var task: Task<Void, Never>?
    @ObservationIgnored private var listTask: Task<Void, Never>?
    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var savedID: String?
    @ObservationIgnored private var ports: [String: Int] = [:]
    @ObservationIgnored private var invalidEvent = false
    @ObservationIgnored private var shuttingDown = false
    @ObservationIgnored private var refreshAgain = false
    @ObservationIgnored private var expectedPeerID: String?
    @ObservationIgnored private let executableOverrides: [String: URL]?

    // Explicit executables support isolated process fixtures; the app uses only
    // helpers adjacent to its own executable.
    init(executables: [String: URL]? = nil) { executableOverrides = executables }

    var canStart: Bool { operation == nil && !stopPending && !cleanupUnconfirmed && !shuttingDown }
    var isSharing: Bool { operation == .receiver && receiverState == .sharing && !stopping }
    var invitationAvailable: Bool { executable("crossflow-invite") != nil }
    var selected: SavedDevice? { devices.first { $0.id == selectedID } }

    private func executable(_ name: String) -> URL? {
        if let executableOverrides { return executableOverrides[name] }
        guard let url = Bundle.main.executableURL?.deletingLastPathComponent().appendingPathComponent(name),
              FileManager.default.isExecutableFile(atPath: url.path) else { return nil }
        return url
    }

    func refresh() {
        guard !shuttingDown else { return }
        if loading { refreshAgain = true; return }
        loading = true
        listTask = Task {
            defer {
                loading = false; listTask = nil
                if refreshAgain { refreshAgain = false; refresh() }
            }
            do {
                guard let executable = executable("crossflow") else { throw DeviceModelError.invalid }
                let output = DeviceOutput()
                let exit = try await listing.run(executable: executable, arguments: ["peers"], timeout: 20,
                                                 onLine: { output.append($0) })
                guard exit == 0, !shuttingDown else { throw DeviceModelError.invalid }
                let result = try DeviceProtocol.savedDevices(output.result())
                devices = result.devices
                if let savedID, devices.contains(where: { $0.id == savedID }) { selectedID = savedID }
                if !devices.contains(where: { $0.id == selectedID }) { selectedID = devices.first?.id }
                listMessage = result.invalidRecords == 0 ? "" : "有设备记录暂时无法读取；已保留原记录。"
            } catch {
                if !shuttingDown { listMessage = "设备列表暂时无法读取，请重试。" }
            }
        }
    }

    func invite() {
        guard canStart, let executable = executable("crossflow-invite") else { return }
        do {
            let begin = try DeviceProtocol.begin(name: deviceName)
            beginOperation(.invitation)
            message = "正在生成配对码…"
            let current = generation
            task = Task { await run(executable, arguments: [], initialInput: begin, timeout: 150, generation: current) }
        } catch { message = "请使用简短的设备名称。" }
    }

    func approve() {
        guard operation == .invitation, !stopping, !approved, let confirmation else { return }
        do {
            try process.send(DeviceProtocol.approve(attempt: confirmation.attempt))
            approved = true
            expectedPeerID = confirmation.fingerprint
            message = "等待另一台电脑确认…"
        } catch { failAndStop("确认未能送达，正在取消配对…") }
    }

    func receive() {
        guard canStart, let selected, let executable = executable("crossflow") else { return }
        do {
            let logs = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                                   appropriateFor: nil, create: true)
                .appendingPathComponent("Crossflow/runs", isDirectory: true)
            try FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
            let log = logs.appendingPathComponent("receiver-\(UUID().uuidString).jsonl")
            var arguments = ["receive", "--peer", selected.id, "--bind", "0.0.0.0:\(ports[selected.id] ?? 24800)",
                             "--seconds", "3600", "--log", log.path, "--control-stdio", "--discoverable", "--inject"]
            if includeKeyboard { arguments.append("--inject-actions") }
            beginOperation(.receiver)
            message = "正在准备接收…"
            let current = generation
            task = Task { await run(executable, arguments: arguments, timeout: 3620, generation: current) }
        } catch { message = "无法创建本地运行记录，请检查磁盘空间。" }
    }

    private func beginOperation(_ value: Operation) {
        generation += 1
        operation = value; receiverState = nil; confirmation = nil; approved = false
        savedID = nil; code = ""; invalidEvent = false; stopping = false
        expectedPeerID = nil
    }

    private func run(_ executable: URL, arguments: [String], initialInput: Data? = nil,
                     timeout: TimeInterval, generation current: Int) async {
        guard current == generation, !stopping, !shuttingDown, !Task.isCancelled else {
            operation = nil; task = nil; code = ""; stopping = false
            return
        }
        let events = AsyncThrowingStream<Data, Error>.makeStream(bufferingPolicy: .bufferingOldest(32))
        let consumer = Task { @MainActor in
            do {
                for try await line in events.stream { accept(line, generation: current) }
            } catch { failAndStop("状态更新未能完成，正在停止…") }
        }
        do {
            let exit = try await process.run(executable: executable, arguments: arguments, initialInput: initialInput,
                                             timeout: timeout, onLine: { line in
                if case .dropped = events.continuation.yield(line) {
                    events.continuation.finish(throwing: DeviceModelError.invalid)
                }
            })
            events.continuation.finish()
            await consumer.value
            guard current == generation else { return }
            if operation == .receiver {
                if exit == 0 && receiverState == .stopped && !invalidEvent {
                    message = "接收已停止。"
                } else {
                    message = "接收未正常完成，请检查系统权限或运行记录。"
                    cleanupUnconfirmed = true
                }
            } else if exit == 0, let savedID, !invalidEvent {
                selectedID = savedID
                message = "配对成功，选择这台电脑后即可开始接收。"
            } else if stopping && !invalidEvent {
                message = "配对已取消；正在刷新已保存设备。"
            } else {
                message = "配对未完成或已过期，请重试。"
            }
        } catch {
            events.continuation.finish()
            await consumer.value
            if current == generation {
                message = operation == .receiver ? "接收已中断，停止结果尚未确认。" : "配对未完成，正在检查已保存设备。"
                if operation == .receiver { cleanupUnconfirmed = true }
                if let native = error as? NativeProcessError, case .cleanupUnconfirmed = native {
                    cleanupUnconfirmed = true
                }
            }
        }
        guard current == generation else { return }
        code = ""; confirmation = nil; approved = false
        operation = nil; stopping = false; task = nil
        if !shuttingDown { refresh() }
    }

    private func accept(_ line: Data, generation current: Int) {
        guard current == generation, operation != nil, !invalidEvent else { return }
        do {
            if operation == .invitation {
                switch try DeviceProtocol.invitation(line) {
                case .ready: break
                case .code(let value, lifetime: _):
                    guard !stopping else { return }
                    code = value; message = "在 Windows 的附近设备中选择这台 Mac，并输入配对码。"
                case .pending(let attempt, let name, let platform, let fingerprint):
                    guard !stopping, platform == "windows" else { throw DeviceModelError.invalid }
                    guard confirmation == nil, expectedPeerID == nil else { throw DeviceModelError.invalid }
                    confirmation = Confirmation(attempt: attempt, name: name, fingerprint: fingerprint)
                    approved = false; message = "请确认正在配对的电脑。"
                case .attemptFailed(let attempt):
                    if confirmation?.attempt == attempt { confirmation = nil; approved = false; expectedPeerID = nil }
                    message = "本次配对未成功，可在有效期内重试。"
                case .discovery(let state):
                    if state == "error" { failAndStop("附近设备发现失败，正在取消配对…") }
                case .saved(let id, let port):
                    guard expectedPeerID == id, savedID == nil else { throw DeviceModelError.invalid }
                    savedID = id; ports[id] = port; code = ""; confirmation = nil
                    message = "配对已保存，正在完成…"
                }
            } else {
                let status = try DeviceProtocol.receiver(line)
                receiverState = status.state
                if stopping { return }
                switch status.state {
                case .starting: message = "正在准备接收…"
                case .listening: message = "已准备好，请在 Windows 上连接并开始共享。"
                case .paused: message = "已连接，等待 Windows 开始共享。"
                case .sharing: message = "正在接收输入 · 本机按 Escape 可停止。"
                case .stopping: message = "正在停止并释放输入…"
                case .stopped: message = "接收已停止。"
                case .error: message = "接收出现错误，请检查系统权限或运行记录。"
                }
            }
        } catch { failAndStop("收到无法识别的状态，正在停止…") }
    }

    private func failAndStop(_ text: String) {
        invalidEvent = true; message = text
        stop()
    }

    func stop() {
        guard operation != nil, !stopping else { return }
        stopping = true; code = ""; confirmation = nil
        if !process.hasActiveOperation { task?.cancel() }
        stopPending = true
        let current = generation
        message = "正在停止…"
        Task {
            let stopped = await process.stop()
            guard current == generation else { return }
            stopPending = false
            if !stopped { cleanupUnconfirmed = true; message = "停止结果尚未确认，请检查本机输入。" }
        }
    }

    func shutdown() async -> Bool {
        shuttingDown = true
        code = ""; confirmation = nil; stopping = operation != nil
        if !process.hasActiveOperation { task?.cancel() }
        if !listing.hasActiveOperation { listTask?.cancel() }
        let stopped = await process.stop()
        let listStopped = await listing.stop()
        if let task { await task.value }
        if let listTask { await listTask.value }
        if !stopped { cleanupUnconfirmed = true }
        shuttingDown = false
        return stopped && listStopped && !cleanupUnconfirmed
    }
}
