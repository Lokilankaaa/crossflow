import Foundation
import XCTest
@testable import CrossflowMac

/// Isolated local helpers exercise the model's actual process ownership and
/// approval boundary. No discovery, Keychain access or OS input is started.
final class DeviceModelTests: XCTestCase {
    private struct Fixture {
        let folder: URL
        let helper: URL
        var launched: Bool { FileManager.default.fileExists(atPath: folder.appendingPathComponent("launched").path) }
        var approved: Bool { FileManager.default.fileExists(atPath: folder.appendingPathComponent("approved").path) }

        init(script: String) throws {
            folder = FileManager.default.temporaryDirectory.appendingPathComponent("crossflow-device-model-\(UUID().uuidString)")
            helper = folder.appendingPathComponent("invitation-fixture")
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false,
                                                     attributes: [.posixPermissions: 0o700])
            do {
                try ("#!/bin/sh\nset -eu\n: > launched\n" + script).write(to: helper, atomically: true, encoding: .utf8)
                try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
            } catch {
                try? FileManager.default.removeItem(at: folder)
                throw error
            }
        }
        func remove() { try? FileManager.default.removeItem(at: folder) }
    }

    private enum FixtureError: Error { case timeout }

    @MainActor private func waitUntil(_ predicate: () -> Bool) async throws {
        let deadline = ProcessInfo.processInfo.systemUptime + 5
        while !predicate() {
            guard ProcessInfo.processInfo.systemUptime < deadline else { throw FixtureError.timeout }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    @MainActor private func withModel(
        script: String,
        _ body: @MainActor (DeviceModel, Fixture) async throws -> Void
    ) async throws {
        let fixture = try Fixture(script: script)
        defer { fixture.remove() }
        let model = DeviceModel(executables: ["crossflow-invite": fixture.helper])
        do {
            try await body(model, fixture)
            let stopped = await model.shutdown()
            XCTAssertTrue(stopped, "Fixture cleanup must confirm child exit")
        } catch {
            _ = await model.shutdown()
            throw error
        }
    }

    @MainActor func testImmediateInvitationStopCannotLaunchScheduledHelper() async throws {
        try await withModel(script: "while IFS= read -r line; do :; done\n") { model, fixture in
            model.invite()
            model.stop() // No suspension between scheduling the start and stop.
            XCTAssertFalse(model.canStart)
            try await self.waitUntil { model.operation == nil && !model.stopPending }
            XCTAssertFalse(fixture.launched, "Cancelled scheduled work must not spawn a helper")
            XCTAssertTrue(model.code.isEmpty)
            XCTAssertNil(model.confirmation)
            XCTAssertFalse(model.cleanupUnconfirmed)
        }
    }

    @MainActor func testImmediateShutdownCannotLaunchScheduledHelper() async throws {
        try await withModel(script: "while IFS= read -r line; do :; done\n") { model, fixture in
            model.invite()
            let stopped = await model.shutdown() // First suspension follows shutdown's cancellation barrier.
            XCTAssertTrue(stopped)
            XCTAssertNil(model.operation)
            XCTAssertFalse(fixture.launched)
            XCTAssertTrue(model.code.isEmpty)
            XCTAssertNil(model.confirmation)
        }
    }

    @MainActor func testMalformedAndUnapprovedSavedFramesCannotClaimPairingSuccess() async throws {
        let id = String(repeating: "a", count: 64)
        let frames = [
            #"{"event":false}"#,
            "{\"event\":\"saved\",\"id\":\"\(id)\",\"suggested_receive_port\":24800,\"input_ready\":false}"
        ]
        for frame in frames {
            let script = "IFS= read -r begin || exit 0\nprintf '%s\\n' '\(frame)'\n"
            try await withModel(script: script) { model, fixture in
                model.invite()
                try await self.waitUntil { model.operation == nil && !model.stopPending }
                XCTAssertTrue(fixture.launched)
                XCTAssertNil(model.selectedID)
                XCTAssertFalse(model.message.contains("配对成功"))
                XCTAssertTrue(model.code.isEmpty)
                XCTAssertNil(model.confirmation)
            }
        }
    }

    @MainActor func testApprovedSavedIdentityMustMatchConfirmedFingerprint() async throws {
        let expected = String(repeating: "a", count: 64)
        let other = String(repeating: "c", count: 64)
        let attempt = String(repeating: "b", count: 32)
        for matches in [false, true] {
            let saved = matches ? expected : other
            let script = """
            IFS= read -r begin || exit 0
            printf '%s\\n' '{"event":"pending","attempt":"\(attempt)","name":"Fixture Windows","platform":"windows","fingerprint":"\(expected)"}'
            IFS= read -r approval || exit 0
            case "$approval" in
              *'"op":"approve"'*) : > approved ;;
              *) exit 9 ;;
            esac
            printf '%s\\n' '{"event":"saved","id":"\(saved)","suggested_receive_port":24800,"input_ready":false}'

            """
            try await withModel(script: script) { model, fixture in
                model.invite()
                try await self.waitUntil { model.confirmation != nil }
                XCTAssertFalse(model.approved)
                model.approve()
                try await self.waitUntil { model.operation == nil && !model.stopPending }
                XCTAssertTrue(fixture.approved, "Fixture must actually receive explicit approval")
                if matches {
                    XCTAssertEqual(model.selectedID, expected)
                    XCTAssertTrue(model.message.contains("配对成功"))
                } else {
                    XCTAssertNil(model.selectedID)
                    XCTAssertFalse(model.message.contains("配对成功"))
                }
                XCTAssertTrue(model.code.isEmpty)
                XCTAssertNil(model.confirmation)
            }
        }
    }

    @MainActor private func withReceiverModel(
        script: String, expectedShutdown: Bool,
        _ body: @MainActor (DeviceModel, Fixture) async throws -> Void
    ) async throws {
        let id = String(repeating: "a", count: 64)
        let fixture = try Fixture(script: """
        case "$1" in
          peers)
            printf '%s\\n' '{"peers":[{"schema":1,"id":"\(id)","friendly_name":"Fixture Windows","last_endpoint":null}],"invalid_records":0}'
            exit 0 ;;
          receive)
            printf 'receive\\n' >> receiver-launches
            \(script)
            ;;
          *) exit 99 ;;
        esac
        """)
        defer { fixture.remove() }
        let model = DeviceModel(executables: ["crossflow": fixture.helper],
                                logDirectory: fixture.folder.appendingPathComponent("logs"))
        model.devices = [SavedDevice(id: id, name: "Fixture Windows", lastEndpoint: nil)]
        model.selectedID = id
        do {
            try await body(model, fixture)
            let stopped = await model.shutdown()
            XCTAssertEqual(stopped, expectedShutdown)
        } catch {
            _ = await model.shutdown()
            throw error
        }
    }

    @MainActor func testPermissionRejectionAllowsRetryAndOrdinaryShutdown() async throws {
        for reason in ["accessibility permission required", "input monitoring permission required"] {
            let script = """
            printf '%s\\n' '{"protocol":1,"state":"startup_rejected","listen_endpoint":null,"error":"\(reason)"}'
            exit 1
            """
            try await withReceiverModel(script: script, expectedShutdown: true) { model, fixture in
                for _ in 0..<2 {
                    model.receive()
                    try await self.waitUntil { model.operation == nil && !model.stopPending && !model.loading }
                    XCTAssertFalse(model.cleanupUnconfirmed)
                    XCTAssertTrue(model.canStart)
                    XCTAssertFalse(model.isSharing)
                    XCTAssertTrue(model.message.contains("系统设置 → 隐私与安全性"))
                    XCTAssertTrue(model.message.contains("重新打开 App"))
                    XCTAssertTrue(model.message.contains(reason.hasPrefix("accessibility") ? "辅助功能" : "输入监控"))
                }
                let launches = try String(contentsOf: fixture.folder.appendingPathComponent("receiver-launches"), encoding: .utf8)
                XCTAssertEqual(launches.split(separator: "\n").count, 2, "Retry must actually launch a second receiver fixture")
            }
        }
    }

    @MainActor func testReceiveModeRequiresExplicitStartAndPassesSelectedInputScope() async throws {
        let script = """
        printf '%s\\n' "$@" > receiver-arguments
        printf '%s\\n' '{"protocol":1,"state":"startup_rejected","listen_endpoint":null,"error":"accessibility permission required"}'
        exit 1
        """
        try await withReceiverModel(script: script, expectedShutdown: true) { model, fixture in
            XCTAssertTrue(model.includeKeyboard)
            XCTAssertNil(model.operation)
            XCTAssertFalse(fixture.launched, "Selecting a default mode must not start input")
            for includeKeyboard in [true, false] {
                model.includeKeyboard = includeKeyboard
                XCTAssertNil(model.operation)
                model.receive()
                try await self.waitUntil { model.operation == nil && !model.stopPending && !model.loading }
                let arguments = try String(contentsOf: fixture.folder.appendingPathComponent("receiver-arguments"), encoding: .utf8)
                    .split(separator: "\n").map(String.init)
                XCTAssertTrue(arguments.contains("--inject"))
                XCTAssertTrue(arguments.contains("--control-stdio"))
                XCTAssertEqual(arguments.contains("--inject-actions"), includeKeyboard)
                XCTAssertTrue(model.canStart)
            }
        }
    }

    @MainActor func testReceiverErrorsAndInvalidRejectionsKeepCleanupGateClosed() async throws {
        let rejected = #"{"protocol":1,"state":"startup_rejected","listen_endpoint":null,"error":"accessibility permission required"}"#
        let frames = [
            #"{"protocol":1,"state":"error","listen_endpoint":null,"error":"receiver failed"}"#,
            #"{"protocol":1,"state":"startup_rejected","listen_endpoint":"0.0.0.0:24800","error":"accessibility permission required"}"#,
            #"{"protocol":1,"state":"startup_rejected","listen_endpoint":null,"error":"receiver failed"}"#,
            #"{"protocol":1,"state":"listening","listen_endpoint":"0.0.0.0:24800","error":null}"# + "\n" + rejected,
            #"{"protocol":1,"state":"error","listen_endpoint":null,"error":"receiver failed"}"# + "\n" + rejected,
            #"{"protocol":1,"state":"stopped","listen_endpoint":null,"error":null}"# + "\n" + rejected,
            rejected + "\n" + #"{"protocol":1,"state":"stopped","listen_endpoint":null,"error":null}"#
        ]
        let cases = frames.map { ($0, 1) } + [(rejected, 0)]
        for (frame, exitCode) in cases {
            try await withReceiverModel(script: "printf '%s\\n' '\(frame)'\nexit \(exitCode)", expectedShutdown: false) { model, _ in
                model.receive()
                try await self.waitUntil { model.operation == nil && !model.stopPending }
                XCTAssertTrue(model.cleanupUnconfirmed)
                XCTAssertFalse(model.canStart)
            }
        }
    }

    @MainActor func testSignalExitCannotConfirmStartupRejection() async throws {
        let script = """
        printf '%s\\n' '{"protocol":1,"state":"startup_rejected","listen_endpoint":null,"error":"accessibility permission required"}'
        kill -TERM $$
        """
        try await withReceiverModel(script: script, expectedShutdown: false) { model, _ in
            model.receive()
            try await self.waitUntil { model.operation == nil && !model.stopPending }
            XCTAssertTrue(model.cleanupUnconfirmed)
            XCTAssertFalse(model.canStart)
        }
    }
}
