import XCTest
@testable import CrossflowMac

final class DiscoveryTests: XCTestCase {
    private let valid = DiscoverySelfTest.validSnapshot

    func testStrictUntrustedDiscoveryBoundary() throws {
        let checks = DiscoverySelfTest.checks()
        XCTAssertEqual(checks.count, 19)
        XCTAssertEqual(checks["discovery_legacy_wire_incompatible"], true)
        XCTAssertEqual(checks["discovery_future_wire_incompatible"], true)
        for (name, passed) in checks { XCTAssertTrue(passed, name) }
    }

    // Local shell fixtures only: these tests do not open discovery sockets,
    // capture input, inject events or create a UI. Run on a native Mac SDK.
    @MainActor func testChildSuccessCancellationAndRefresh() async throws {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("crossflow-discovery-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: folder) }
        let script = folder.appendingPathComponent("fixture.sh")
        try "printf '%s' '\(valid)'\n".write(to: script, atomically: true, encoding: .utf8)
        let client = DiscoveryClient(executable: URL(fileURLWithPath: "/bin/sh"), arguments: [script.path])
        let result = try await client.scan()
        XCTAssertEqual(result.devices.count, 1)
        let second = try await client.scan()
        XCTAssertEqual(second.devices.count, 1)
        let initiallyStopped = await client.stop()
        XCTAssertTrue(initiallyStopped)

        try "exec /bin/sleep 10\n".write(to: script, atomically: true, encoding: .utf8)
        let waiting = Task { try await client.scan() }
        try await Task.sleep(for: .milliseconds(100))
        let start = ProcessInfo.processInfo.systemUptime
        waiting.cancel()
        do { _ = try await waiting.value; XCTFail("cancelled scan succeeded") }
        catch is CancellationError { }
        catch { XCTFail("unexpected cancellation result: \(error)") }
        XCTAssertLessThan(ProcessInfo.processInfo.systemUptime - start, 4)
        let stopped = await client.stop()
        XCTAssertTrue(stopped)
        try "printf '%s' '\(valid)'\n".write(to: script, atomically: true, encoding: .utf8)
        let refreshed = try await client.scan()
        XCTAssertEqual(refreshed.devices.count, 1)
    }

    @MainActor func testImmediatePageReentryQueuesRefreshAfterCancellation() async throws {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("crossflow-discovery-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: folder) }
        let script = folder.appendingPathComponent("fixture.sh")
        try "printf '%s' '\(valid)'\n".write(to: script, atomically: true, encoding: .utf8)
        let client = DiscoveryClient(executable: URL(fileURLWithPath: "/bin/sh"), arguments: [script.path])
        let model = DiscoveryModel(client: client)
        model.start()
        model.cancel()
        model.start() // The first task still exists; this must not lose the scan.
        let deadline = ProcessInfo.processInfo.systemUptime + 3
        while model.devices.isEmpty && ProcessInfo.processInfo.systemUptime < deadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertEqual(model.devices.count, 1)
        XCTAssertFalse(model.scanning)
        let stopped = await model.shutdown()
        XCTAssertTrue(stopped)
    }

    @MainActor func testFloodAndTimeoutAreBounded() async throws {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("crossflow-discovery-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: folder) }
        let script = folder.appendingPathComponent("fixture.sh")
        for body in ["exec /usr/bin/yes x\n", "exec /bin/sleep 10\n"] {
            try body.write(to: script, atomically: true, encoding: .utf8)
            let client = DiscoveryClient(executable: URL(fileURLWithPath: "/bin/sh"), arguments: [script.path], timeout: 0.15)
            let start = ProcessInfo.processInfo.systemUptime
            do { _ = try await client.scan(); XCTFail("invalid fixture succeeded") }
            catch { XCTAssertTrue(error is DiscoveryError) }
            XCTAssertLessThan(ProcessInfo.processInfo.systemUptime - start, 4)
            let stopped = await client.stop()
            XCTAssertTrue(stopped)
        }
    }
}
