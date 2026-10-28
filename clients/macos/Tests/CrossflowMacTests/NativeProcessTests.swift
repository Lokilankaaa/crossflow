import XCTest
@testable import CrossflowMac

/// Local fixture processes only. No network, Keychain, UI or OS input is used.
final class NativeProcessTests: XCTestCase {
    private final class Lines: @unchecked Sendable {
        private let lock = NSLock()
        private var values: [Data] = []
        func append(_ line: Data) { lock.withLock { values.append(line) } }
        func contains(_ value: String) -> Bool {
            lock.withLock { values.contains(Data(value.utf8)) }
        }
    }

    @MainActor private func waitFor(_ line: String, in lines: Lines) async throws {
        let deadline = ProcessInfo.processInfo.systemUptime + 3
        while !lines.contains(line) {
            guard ProcessInfo.processInfo.systemUptime < deadline else {
                throw NativeProcessError.timeout
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    @MainActor func testDuplexCommandsAndCooperativeEOFConfirmExit() async throws {
        let client = NativeProcessClient(), lines = Lines()
        let run = Task {
            try await client.run(executable: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c",
                "printf 'ready\\n'; while IFS= read -r line; do printf 'received\\n'; done; printf 'closed\\n'"],
                initialInput: Data("begin\n".utf8), timeout: 20, onLine: { lines.append($0) })
        }
        do {
            try await waitFor("received\n", in: lines)
            try client.send(Data("next\n".utf8))
            do {
                _ = try await client.run(executable: URL(fileURLWithPath: "/bin/true"),
                                         arguments: [], timeout: 1, onLine: { _ in })
                XCTFail("A second process started while the first was active")
            } catch NativeProcessError.busy { }
            let stopped = await client.stop()
            XCTAssertTrue(stopped)
            let status = try await run.value
            XCTAssertEqual(status, 0)
            XCTAssertTrue(lines.contains("closed\n"))
            let stoppedAgain = await client.stop()
            XCTAssertTrue(stoppedAgain)
        } catch {
            _ = await client.stop()
            _ = try? await run.value
            throw error
        }
    }

    @MainActor func testCommandValidationAndQueueAreBounded() async throws {
        let client = NativeProcessClient(), lines = Lines()
        let run = Task {
            try await client.run(executable: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c",
                "printf 'ready\\n'; while IFS= read -r line; do :; done"], timeout: 20,
                onLine: { lines.append($0) })
        }
        do {
            try await waitFor("ready\n", in: lines)
            for bytes in [Data(), Data("missing newline".utf8), Data("two\nlines\n".utf8),
                          Data(repeating: 120, count: 1024) + Data([10])] {
                XCTAssertThrowsError(try client.send(bytes)) { error in
                    guard case NativeProcessError.invalidCommand = error else {
                        return XCTFail("Unexpected invalid-command error")
                    }
                }
            }
            var full = false
            // The caller can enqueue much faster than the 10 ms I/O poll; even
            // with concurrent draining, a bounded queue must reject a burst.
            for _ in 0..<10_000 {
                do { try client.send(Data("command\n".utf8)) }
                catch NativeProcessError.commandQueueFull { full = true; break }
            }
            XCTAssertTrue(full)
            let stopped = await client.stop()
            XCTAssertTrue(stopped)
            let status = try await run.value
            XCTAssertEqual(status, 0)
        } catch {
            _ = await client.stop(); _ = try? await run.value
            throw error
        }
    }

    @MainActor func testMalformedAndFloodedOutputFailWithoutLoggingPayload() async throws {
        for script in [
            "i=0; while [ \"$i\" -lt 3000 ]; do printf x; i=$((i+1)); done; printf '\\n'",
            "i=0; while [ \"$i\" -lt 2000 ]; do printf 'x\\n'; i=$((i+1)); done"
        ] {
            let client = NativeProcessClient()
            do {
                _ = try await client.run(executable: URL(fileURLWithPath: "/bin/sh"),
                                         arguments: ["-c", script], timeout: 5, onLine: { _ in })
                XCTFail("Invalid child output was accepted")
            } catch NativeProcessError.invalidOutput { }
            let stopped = await client.stop()
            XCTAssertTrue(stopped)
        }
    }

    @MainActor func testTaskCancellationClosesInputAndWaitsForExit() async throws {
        let client = NativeProcessClient(), lines = Lines()
        let run = Task {
            try await client.run(executable: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c",
                "printf 'ready\\n'; while IFS= read -r line; do :; done; printf 'closed\\n'"],
                timeout: 20, onLine: { lines.append($0) })
        }
        do {
            try await waitFor("ready\n", in: lines)
            run.cancel()
            do { _ = try await run.value; XCTFail("Cancelled task reported success") }
            catch is CancellationError { }
            let stopped = await client.stop()
            XCTAssertTrue(stopped)
            XCTAssertTrue(lines.contains("closed\n"))
        } catch {
            _ = await client.stop(); _ = try? await run.value
            throw error
        }
    }

    @MainActor func testForcedExitRemainsUnconfirmedAfterReaping() async throws {
        let client = NativeProcessClient(), lines = Lines()
        let run = Task {
            try await client.run(executable: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c",
                "trap '' TERM; printf 'ready\\n'; exec /bin/sleep 30"], timeout: 60,
                onLine: { lines.append($0) })
        }
        do {
            try await waitFor("ready\n", in: lines)
            let began = ProcessInfo.processInfo.systemUptime
            let stopped = await client.stop()
            XCTAssertFalse(stopped)
            XCTAssertLessThan(ProcessInfo.processInfo.systemUptime - began, 12)
            do { _ = try await run.value; XCTFail("Forced exit reported success") }
            catch NativeProcessError.cleanupUnconfirmed { }
            let stoppedAgain = await client.stop()
            XCTAssertFalse(stoppedAgain, "A previous forced exit cannot become a clean stop")
            do {
                _ = try await client.run(executable: URL(fileURLWithPath: "/bin/true"),
                                         arguments: [], timeout: 1, onLine: { _ in })
                XCTFail("A client with uncertain cleanup was reused")
            } catch NativeProcessError.cleanupUnconfirmed { }
        } catch {
            _ = await client.stop(); _ = try? await run.value
            throw error
        }
    }
}
