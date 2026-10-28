import Darwin
import Foundation

/// One discovery-only child at a time. Process and pipe operations belong to the
/// private queue; the lock protects only the active slot/cancellation flag.
/// No blocking read, waitUntilExit, shell command, input API or trust write.
final class DiscoveryClient: @unchecked Sendable {
    private final class Scan: @unchecked Sendable {
        let lock = NSLock()
        private var cancelled = false
        // Only the supervisor queue accesses these resources.
        var process: Process?
        var output: Pipe?
        var errors: Pipe?
        func cancel() { lock.withLock { cancelled = true } }
        var isCancelled: Bool { lock.withLock { cancelled } }
    }

    private let lock = NSLock()
    private let queue = DispatchQueue(label: "local.crossflow.discovery", qos: .utility)
    private let executable: URL?
    private let arguments: [String]
    private let timeout: TimeInterval
    private var active: Scan?

    // Explicit URL/arguments are only for local non-network test fixtures.
    init(executable: URL? = nil, arguments: [String] = ["discover", "--seconds", "3"], timeout: TimeInterval = 8) {
        self.executable = executable; self.arguments = arguments; self.timeout = timeout
    }

    func scan() async throws -> DiscoveryResult {
        try Task.checkCancellation()
        let scan = try lock.withLock {
            guard active == nil else { throw DiscoveryError.cleanupPending }
            let scan = Scan(); active = scan; return scan
        }
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                queue.async {
                    let result: Result<DiscoveryResult, Error>
                    do { result = .success(try self.run(scan)) }
                    catch { result = .failure(error) }
                    let cleaned = self.cleanup(scan)
                    if cleaned { self.clear(scan) }
                    continuation.resume(with: cleaned ? result : .failure(DiscoveryError.cleanupPending))
                }
            }
        } onCancel: { scan.cancel() }
    }

    func cancel() { lock.withLock { active }?.cancel() }

    /// Async and bounded, including when called from app termination. A failed
    /// cleanup retains the active slot so a second scan cannot be launched.
    func stop() async -> Bool {
        guard let scan = lock.withLock({ active }) else { return true }
        scan.cancel()
        queue.async {
            // Also retries cleanup if the first supervisor pass could not reap.
            if self.cleanup(scan) { self.clear(scan) }
        }
        let deadline = ProcessInfo.processInfo.systemUptime + 5
        while ProcessInfo.processInfo.systemUptime < deadline {
            if lock.withLock({ active == nil }) { return true }
            // Cleanup must still wait cooperatively when its caller is cancelled.
            await withCheckedContinuation { continuation in
                DispatchQueue.global().asyncAfter(deadline: .now() + .milliseconds(25)) {
                    continuation.resume()
                }
            }
        }
        return false
    }

    private func clear(_ scan: Scan) {
        lock.withLock { if active === scan { active = nil } }
    }

    private func run(_ scan: Scan) throws -> DiscoveryResult {
        if scan.isCancelled { throw CancellationError() }
        let url = executable ?? Bundle.main.executableURL?.deletingLastPathComponent().appendingPathComponent("crossflow")
        guard let url, FileManager.default.isExecutableFile(atPath: url.path) else { throw DiscoveryError.unavailable }
        let process = Process()
        process.executableURL = url; process.arguments = arguments
        process.currentDirectoryURL = url.deletingLastPathComponent()
        let output = Pipe(), errors = Pipe()
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = output; process.standardError = errors
        scan.output = output; scan.errors = errors
        try nonblocking(output.fileHandleForReading.fileDescriptor)
        try nonblocking(errors.fileHandleForReading.fileDescriptor)
        if scan.isCancelled { throw CancellationError() }
        try process.run()
        scan.process = process
        // Foundation may already close these parent-side write handles in run().
        // Re-closing is harmless; read-side fcntl/read failures still propagate.
        try? output.fileHandleForWriting.close(); try? errors.fileHandleForWriting.close()
        let deadline = ProcessInfo.processInfo.systemUptime + timeout
        var bytes = Data(), discarded = Data()
        var outputCount = 0, errorCount = 0
        var outputEOF = false, errorEOF = false
        while true {
            if scan.isCancelled { throw CancellationError() }
            guard ProcessInfo.processInfo.systemUptime < deadline else { throw DiscoveryError.timeout }
            try drain(output.fileHandleForReading.fileDescriptor, into: &bytes, keep: true, count: &outputCount, eof: &outputEOF)
            try drain(errors.fileHandleForReading.fileDescriptor, into: &discarded, keep: false, count: &errorCount, eof: &errorEOF)
            if !process.isRunning && outputEOF && errorEOF { break }
            usleep(10_000) // Private queue only; never blocks SwiftUI.
        }
        guard process.terminationStatus == 0 else { throw DiscoveryError.failed }
        return try DiscoveryProtocol.parse(bytes)
    }

    private func nonblocking(_ descriptor: Int32) throws {
        let flags = fcntl(descriptor, F_GETFL)
        guard flags >= 0, fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 else { throw DiscoveryError.failed }
    }

    private func drain(_ descriptor: Int32, into output: inout Data, keep: Bool, count: inout Int, eof: inout Bool) throws {
        guard !eof else { return }
        var buffer = [UInt8](repeating: 0, count: 4096)
        for _ in 0..<16 { // Bound work so a flooding child cannot starve cancel.
            let size = buffer.withUnsafeMutableBytes { Darwin.read(descriptor, $0.baseAddress, $0.count) }
            if size == 0 { eof = true; return }
            if size < 0 {
                if errno == EINTR { continue }
                if errno == EAGAIN || errno == EWOULDBLOCK { return }
                throw DiscoveryError.failed
            }
            count += size
            guard count <= DiscoveryProtocol.maxOutputBytes else { throw DiscoveryError.invalidResult }
            if keep { output.append(contentsOf: buffer.prefix(size)) }
        }
    }

    private func cleanup(_ scan: Scan) -> Bool {
        if let process = scan.process, process.isRunning {
            process.terminate()
            wait(process, seconds: 0.5)
            if process.isRunning {
                _ = Darwin.kill(process.processIdentifier, SIGKILL)
                wait(process, seconds: 1.5)
            }
        }
        for pipe in [scan.output, scan.errors].compactMap({ $0 }) {
            try? pipe.fileHandleForReading.close(); try? pipe.fileHandleForWriting.close()
        }
        scan.output = nil; scan.errors = nil
        guard scan.process?.isRunning != true else { return false }
        scan.process = nil
        return true
    }

    private func wait(_ process: Process, seconds: TimeInterval) {
        let deadline = ProcessInfo.processInfo.systemUptime + seconds
        while process.isRunning && ProcessInfo.processInfo.systemUptime < deadline { usleep(10_000) }
    }
}
