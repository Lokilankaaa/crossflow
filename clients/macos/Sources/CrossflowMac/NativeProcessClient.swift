import Darwin
import Foundation

enum NativeProcessError: Error, LocalizedError, Sendable {
    case busy, unavailable, invalidCommand, commandQueueFull, invalidOutput
    case timeout, failed, cleanupUnconfirmed

    var errorDescription: String? {
        switch self {
        case .busy: "上一次操作仍在结束，请稍候。"
        case .unavailable: "所需组件暂不可用，请重新安装完整客户端。"
        case .invalidCommand, .commandQueueFull, .invalidOutput, .failed: "操作未完成，请重试。"
        case .timeout: "操作超时，已请求停止。"
        case .cleanupUnconfirmed: "尚未确认操作已安全结束，请检查连接状态。"
        }
    }
}

/// One private child at a time. All Process and pipe operations stay on `queue`.
/// The lock protects the slot and bounded controls, never a blocking pipe call.
/// Lines can contain invitation codes: never log them or include them in errors.
final class NativeProcessClient: @unchecked Sendable {
    private final class Operation: @unchecked Sendable {
        let lock = NSLock()
        var commands: [Data] = []
        var commandOffset = 0
        var stopRequested = false
        var taskCancelled = false
        var finished = false
        var confirmed = false
        // Resources below belong exclusively to the supervisor queue.
        var process: Process?
        var input: Pipe?
        var output: Pipe?
        var inputClosed = false
        var outputEOF = false
        var runFinished = false

        func requestStop(cancelTask: Bool = false) {
            lock.withLock {
                stopRequested = true
                taskCancelled = taskCancelled || cancelTask
                commands.removeAll()
                commandOffset = 0
            }
        }
        var isStopping: Bool { lock.withLock { stopRequested } }
        var isCancelled: Bool { lock.withLock { taskCancelled } }
        var outcome: Bool? { lock.withLock { finished ? confirmed : nil } }
        func enqueue(_ data: Data) throws {
            try lock.withLock {
                guard !stopRequested, !finished else { throw NativeProcessError.failed }
                guard commands.count < 8 else { throw NativeProcessError.commandQueueFull }
                commands.append(data)
            }
        }
    }

    private let lock = NSLock()
    private let queue = DispatchQueue(label: "local.crossflow.native-process", qos: .userInitiated)
    private var active: Operation?
    // Forced termination must never turn into a later implicit success.
    private var cleanupHistoryConfirmed = true

    var hasActiveOperation: Bool { lock.withLock { active != nil } }

    func run(
        executable: URL,
        arguments: [String],
        initialInput: Data? = nil,
        timeout: TimeInterval,
        onLine: @escaping @Sendable (Data) -> Void
    ) async throws -> Int32 {
        try Task.checkCancellation()
        guard timeout.isFinite, timeout > 0, timeout <= 86_400 else { throw NativeProcessError.failed }
        if let initialInput { try Self.validateCommand(initialInput) }
        let operation = Operation()
        if let initialInput { try operation.enqueue(initialInput) }
        try lock.withLock {
            guard active == nil else { throw NativeProcessError.busy }
            guard cleanupHistoryConfirmed else { throw NativeProcessError.cleanupUnconfirmed }
            active = operation
        }
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                queue.async {
                    let result = self.supervise(operation, executable: executable, arguments: arguments,
                                                timeout: timeout, onLine: onLine)
                    operation.runFinished = true
                    continuation.resume(with: result)
                }
            }
        } onCancel: { operation.requestStop(cancelTask: true) }
    }

    func send(_ data: Data) throws {
        try Self.validateCommand(data)
        guard let operation = lock.withLock({ active }) else { throw NativeProcessError.failed }
        try operation.enqueue(data)
    }

    /// Closes stdin while continuing to drain output. A true result confirms
    /// process cleanup, not the meaning of the child's final protocol frame.
    /// Cancellation of this caller must not skip the cooperative cleanup wait.
    func stop() async -> Bool {
        guard let operation = lock.withLock({ active }) else {
            return lock.withLock { cleanupHistoryConfirmed }
        }
        operation.requestStop()
        queue.async {
            // If a previous kill could not confirm exit, retain ownership and
            // allow an explicit later stop to retry reaping that same process.
            if operation.runFinished, operation.process?.isRunning == true {
                self.retryReap(operation)
            }
        }
        let deadline = ProcessInfo.processInfo.systemUptime + 11
        while ProcessInfo.processInfo.systemUptime < deadline {
            if let confirmed = operation.outcome {
                return confirmed && lock.withLock { cleanupHistoryConfirmed }
            }
            await withCheckedContinuation { continuation in
                DispatchQueue.global().asyncAfter(deadline: .now() + .milliseconds(20)) {
                    continuation.resume()
                }
            }
        }
        return false
    }

    private static func validateCommand(_ data: Data) throws {
        guard !data.isEmpty, data.count <= 1024, data.last == 10,
              !data.dropLast().contains(10) else { throw NativeProcessError.invalidCommand }
    }

    private func supervise(
        _ operation: Operation, executable: URL, arguments: [String],
        timeout: TimeInterval, onLine: @escaping @Sendable (Data) -> Void
    ) -> Result<Int32, Error> {
        var failure: Error?
        var forced = false
        var output = Data()
        var stopBegan: TimeInterval?
        var terminationSent = false
        var killSent = false
        var rateStarted = ProcessInfo.processInfo.systemUptime
        var lineCount = 0
        let deadline = rateStarted + timeout
        do {
            guard !operation.isStopping else { throw CancellationError() }
            guard executable.isFileURL, FileManager.default.isExecutableFile(atPath: executable.path) else {
                throw NativeProcessError.unavailable
            }
            let process = Process(), input = Pipe(), stdout = Pipe()
            operation.input = input; operation.output = stdout
            process.executableURL = executable
            process.arguments = arguments
            process.currentDirectoryURL = executable.deletingLastPathComponent()
            process.standardInput = input
            process.standardOutput = stdout
            process.standardError = FileHandle.nullDevice
            try nonblocking(input.fileHandleForWriting.fileDescriptor)
            try nonblocking(stdout.fileHandleForReading.fileDescriptor)
            guard fcntl(input.fileHandleForWriting.fileDescriptor, F_SETNOSIGPIPE, 1) == 0 else {
                throw NativeProcessError.failed
            }
            guard !operation.isStopping else { throw CancellationError() }
            try process.run()
            operation.process = process
            try? input.fileHandleForReading.close()
            try? stdout.fileHandleForWriting.close()
        } catch {
            if error is CancellationError { failure = CancellationError() }
            else { failure = error as? NativeProcessError ?? NativeProcessError.failed }
        }
        while let process = operation.process {
            let now = ProcessInfo.processInfo.systemUptime
            if now >= deadline && failure == nil {
                failure = NativeProcessError.timeout
                operation.requestStop()
            }
            if failure != nil { operation.requestStop() }
            if operation.isStopping {
                if stopBegan == nil { stopBegan = now }
                closeInput(operation)
            }
            do {
                try drain(operation, buffer: &output, rateStarted: &rateStarted, lineCount: &lineCount,
                          deliver: failure == nil, onLine: onLine)
                if !operation.isStopping { try writeCommand(operation) }
            } catch {
                if failure == nil { failure = error as? NativeProcessError ?? NativeProcessError.failed }
                operation.requestStop()
            }
            if !process.isRunning && operation.outputEOF { break }
            if let stopBegan {
                let elapsed = now - stopBegan
                if elapsed >= 8 && !terminationSent {
                    if process.isRunning { process.terminate(); forced = true }
                    terminationSent = true
                }
                if elapsed >= 9 && !killSent {
                    if process.isRunning { _ = Darwin.kill(process.processIdentifier, SIGKILL); forced = true }
                    killSent = true
                }
                if elapsed >= 10 { break }
            }
            usleep(10_000)
        }
        let exited = operation.process?.isRunning != true
        let confirmed = exited && !forced && (operation.process == nil || operation.outputEOF)
        let status = exited ? operation.process?.terminationStatus ?? 0 : -1
        finish(operation, confirmed: confirmed, release: exited)
        if !confirmed { return .failure(NativeProcessError.cleanupUnconfirmed) }
        if operation.isCancelled { return .failure(CancellationError()) }
        if let failure { return .failure(failure) }
        if let process = operation.process, process.terminationReason != .exit {
            return .failure(NativeProcessError.failed)
        }
        return .success(status)
    }

    private func nonblocking(_ descriptor: Int32) throws {
        let flags = fcntl(descriptor, F_GETFL)
        guard flags >= 0, fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 else {
            throw NativeProcessError.failed
        }
    }

    private func writeCommand(_ operation: Operation) throws {
        guard !operation.inputClosed, let input = operation.input else { return }
        try operation.lock.withLock {
            guard !operation.stopRequested, let command = operation.commands.first else { return }
            let written = command.withUnsafeBytes { bytes in
                Darwin.write(input.fileHandleForWriting.fileDescriptor,
                             bytes.baseAddress!.advanced(by: operation.commandOffset),
                             command.count - operation.commandOffset)
            }
            if written < 0 {
                if errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR { return }
                throw NativeProcessError.failed
            }
            guard written > 0 else { throw NativeProcessError.failed }
            operation.commandOffset += written
            if operation.commandOffset == command.count {
                operation.commands.removeFirst(); operation.commandOffset = 0
            }
        }
    }

    private func drain(
        _ operation: Operation, buffer: inout Data, rateStarted: inout TimeInterval,
        lineCount: inout Int, deliver: Bool, onLine: @escaping @Sendable (Data) -> Void
    ) throws {
        guard !operation.outputEOF, let stdout = operation.output else { return }
        var bytes = [UInt8](repeating: 0, count: 4096)
        for _ in 0..<8 {
            let count = bytes.withUnsafeMutableBytes {
                Darwin.read(stdout.fileHandleForReading.fileDescriptor, $0.baseAddress, $0.count)
            }
            if count == 0 {
                operation.outputEOF = true
                guard buffer.isEmpty else { throw NativeProcessError.invalidOutput }
                return
            }
            if count < 0 {
                if errno == EINTR { continue }
                if errno == EAGAIN || errno == EWOULDBLOCK { return }
                throw NativeProcessError.failed
            }
            if !deliver { buffer.removeAll(keepingCapacity: false); continue }
            buffer.append(contentsOf: bytes.prefix(count))
            while let end = buffer.firstIndex(of: 10) {
                let size = buffer.distance(from: buffer.startIndex, to: end) + 1
                guard size <= 2048 else { throw NativeProcessError.invalidOutput }
                let now = ProcessInfo.processInfo.systemUptime
                if now - rateStarted >= 1 { rateStarted = now; lineCount = 0 }
                lineCount += 1
                // Pretty-printed bounded peer snapshots can legitimately contain
                // hundreds of lines. Streaming inputs remain individually bounded.
                guard lineCount <= 1024 else { throw NativeProcessError.invalidOutput }
                let line = Data(buffer.prefix(size))
                buffer.removeFirst(size)
                onLine(line) // Caller must return promptly; never dispatch synchronously to UI.
            }
            guard buffer.count < 2048 else { throw NativeProcessError.invalidOutput }
        }
    }

    private func closeInput(_ operation: Operation) {
        guard !operation.inputClosed else { return }
        operation.inputClosed = true
        try? operation.input?.fileHandleForWriting.close()
    }

    private func finish(_ operation: Operation, confirmed: Bool, release: Bool) {
        closeInput(operation)
        if release {
            try? operation.input?.fileHandleForReading.close()
            try? operation.output?.fileHandleForReading.close()
            try? operation.output?.fileHandleForWriting.close()
            operation.input = nil; operation.output = nil; operation.process = nil
        }
        operation.lock.withLock {
            operation.finished = true; operation.confirmed = confirmed
            operation.commands.removeAll(); operation.commandOffset = 0
        }
        lock.withLock {
            cleanupHistoryConfirmed = cleanupHistoryConfirmed && confirmed
            if release, active === operation { active = nil }
        }
    }

    private func retryReap(_ operation: Operation) {
        guard let process = operation.process else { return }
        if process.isRunning { _ = Darwin.kill(process.processIdentifier, SIGKILL) }
        let deadline = ProcessInfo.processInfo.systemUptime + 1
        while process.isRunning && ProcessInfo.processInfo.systemUptime < deadline { usleep(10_000) }
        if !process.isRunning { finish(operation, confirmed: false, release: true) }
    }
}
