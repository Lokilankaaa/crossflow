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
}
