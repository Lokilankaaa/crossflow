import XCTest
@testable import CrossflowMac

final class DeviceProtocolTests: XCTestCase {
    private let id = String(repeating: "a", count: 64)
    private let attempt = String(repeating: "b", count: 32)
    private func bytes(_ object: [String: Any]) throws -> Data {
        try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    }
    private func receiverBytes(_ object: [String: Any]) throws -> Data {
        var value: [String: Any] = ["route_enabled": false, "focus_device_id": NSNull(),
                                  "focus_screen_id": NSNull(), "shared_layout": NSNull(), "input_active": false]
        value.merge(object) { _, new in new }
        return try bytes(value)
    }
    private func peer(endpoint: Any = NSNull()) -> [String: Any] {
        ["schema": 1, "id": id, "friendly_name": "Windows 工作台", "last_endpoint": endpoint]
    }

    func testSavedPeerPreservesReceiverOnlyRecordAndInvalidCount() throws {
        let result = try DeviceProtocol.savedDevices(bytes(["peers": [peer()], "invalid_records": 1]))
        XCTAssertEqual(result.devices.count, 1)
        XCTAssertEqual(result.invalidRecords, 1)
        XCTAssertEqual(result.devices[0].id, id)
        XCTAssertNil(result.devices[0].lastEndpoint)
        let routed = try DeviceProtocol.savedDevices(bytes(["peers": [peer(endpoint: "[fe80::1%4]:24800")], "invalid_records": 0]))
        XCTAssertEqual(routed.devices[0].lastEndpoint, "[fe80::1%4]:24800")
    }

    func testSavedPeerRejectsAmbiguousAndUnsafeMetadata() throws {
        var missingEndpoint = peer(); missingEndpoint.removeValue(forKey: "last_endpoint")
        var unsafeID = peer(); unsafeID["id"] = "../identity"
        var unsafeName = peer(); unsafeName["friendly_name"] = "Mac\u{202e}hidden"
        var unsupportedSchema = peer(); unsupportedSchema["schema"] = true
        for item in [missingEndpoint, unsafeID, unsafeName, unsupportedSchema,
                     peer(endpoint: "host.example:24800"), peer(endpoint: "0.0.0.0:24800"),
                     peer(endpoint: "[::ffff:224.0.0.1]:24800"), peer(endpoint: "127.0.0.1:0")] {
            XCTAssertThrowsError(try DeviceProtocol.savedDevices(bytes(["peers": [item], "invalid_records": 0])))
        }
        XCTAssertThrowsError(try DeviceProtocol.savedDevices(bytes(["peers": [peer(), peer()], "invalid_records": 0])))
        XCTAssertThrowsError(try DeviceProtocol.savedDevices(bytes(["peers": [], "invalid_records": 65])))
        XCTAssertThrowsError(try DeviceProtocol.savedDevices(Data(repeating: 32, count: DeviceProtocol.maxPeerBytes + 1)))
    }

    func testInvitationRequiresValidatedAttemptAndNoAutomaticInput() throws {
        let ready = try DeviceProtocol.invitation(bytes(["event": "invitation_ready", "pairing_endpoint": "0.0.0.0:24801", "reserved_input_endpoint": "[::]:24800", "input_ready": false]))
        guard case .ready = ready else { return XCTFail("Expected invitation readiness") }
        let pending = try DeviceProtocol.invitation(bytes(["event": "pending", "attempt": attempt, "name": "Windows", "platform": "windows", "fingerprint": id]))
        guard case let .pending(token, name, platform, fingerprint) = pending else { return XCTFail("Expected authenticated pending peer") }
        XCTAssertEqual(token, attempt); XCTAssertEqual(name, "Windows")
        XCTAssertEqual(platform, "windows"); XCTAssertEqual(fingerprint, id)
        let saved = try DeviceProtocol.invitation(bytes(["event": "saved", "id": id, "suggested_receive_port": 24800, "input_ready": false]))
        guard case let .saved(savedID, port) = saved else { return XCTFail("Expected saved peer") }
        XCTAssertEqual(savedID, id); XCTAssertEqual(port, 24800)
        XCTAssertThrowsError(try DeviceProtocol.invitation(bytes(["event": "saved", "id": id, "suggested_receive_port": 24800, "input_ready": true])))
        XCTAssertThrowsError(try DeviceProtocol.invitation(bytes(["event": "pending", "attempt": "old", "name": "Windows", "platform": "windows", "fingerprint": id])))
        XCTAssertThrowsError(try DeviceProtocol.invitation(bytes(["event": "pending", "attempt": attempt, "name": "Windows", "platform": "linux", "fingerprint": id])))
    }

    func testCodeParsingNeverIncludesRejectedSecretInErrors() throws {
        let fixtureCode = "12345678"
        let code = try DeviceProtocol.invitation(bytes(["event": "code", "code": fixtureCode, "lifetime_seconds": 120]))
        guard case let .code(parsed, lifetime) = code else { return XCTFail("Expected code") }
        XCTAssertEqual(parsed, fixtureCode); XCTAssertEqual(lifetime, 120)
        let invalidCodes: [[String: Any]] = [
            ["event": "code", "code": fixtureCode, "lifetime_seconds": 121],
            ["event": "code", "code": fixtureCode, "lifetime_seconds": 120, "extra": true],
            ["event": "code", "code": "１２３４５６７８", "lifetime_seconds": 120]
        ]
        for object in invalidCodes {
            XCTAssertThrowsError(try DeviceProtocol.invitation(bytes(object))) { error in
                XCTAssertTrue(error is DeviceProtocolError)
                XCTAssertFalse(error.localizedDescription.contains(fixtureCode))
            }
        }
        XCTAssertThrowsError(try DeviceProtocol.invitation(Data(repeating: 32, count: 2049)))
    }

    func testReceiverStatesDistinguishReadinessAndFinalFailure() throws {
        let listening = try DeviceProtocol.receiver(receiverBytes(["protocol": 2, "state": "listening", "listen_endpoint": "0.0.0.0:24800", "error": NSNull()]))
        XCTAssertEqual(listening.state, .listening)
        let stopping = try DeviceProtocol.receiver(receiverBytes(["protocol": 2, "state": "stopping", "listen_endpoint": "[::1]:24800", "error": "control output closed"]))
        XCTAssertEqual(stopping.state, .stopping)
        XCTAssertNotNil(stopping.error)
        let failed = try DeviceProtocol.receiver(receiverBytes(["protocol": 2, "state": "error", "listen_endpoint": NSNull(), "error": "receiver failed"]))
        XCTAssertEqual(failed.state, .error)
        let invalidStates: [[String: Any]] = [
            ["protocol": 2, "state": "connected", "listen_endpoint": "0.0.0.0:24800", "error": NSNull()],
            ["protocol": 2, "state": "sharing", "listen_endpoint": NSNull(), "error": NSNull()],
            ["protocol": 2, "state": "error", "listen_endpoint": NSNull(), "error": NSNull()],
            ["protocol": 2, "state": "stopped", "listen_endpoint": NSNull(), "error": "failed"],
            ["protocol": 1, "state": "starting", "listen_endpoint": NSNull(), "error": NSNull()],
            ["protocol": 2, "state": "listening", "listen_endpoint": "[1:2:3]:24800", "error": NSNull()]
        ]
        for item in invalidStates { XCTAssertThrowsError(try DeviceProtocol.receiver(receiverBytes(item))) }
    }

    func testDuplicateEscapedKeysAndNonIntegerVersionsAreRejected() {
        for raw in [
            #"{"protocol":1,"protocol":1,"state":"starting","listen_endpoint":null,"error":null}"#,
            #"{"protocol":1,"\u0070rotocol":1,"state":"starting","listen_endpoint":null,"error":null}"#,
            #"{"protocol":1.0,"state":"starting","listen_endpoint":null,"error":null}"#,
            #"{"protocol":1e0,"state":"starting","listen_endpoint":null,"error":null}"#,
            #"{"protocol":true,"state":"starting","listen_endpoint":null,"error":null}"#,
            #"{"protocol":1,"state":"starting","listen_endpoint":null,"error":null,"extra":1}"#
        ] { XCTAssertThrowsError(try DeviceProtocol.receiver(Data(raw.utf8))) }
    }

    func testStartupRejectionRequiresKnownPermissionAndNoListener() throws {
        for reason in ["accessibility permission required", "input monitoring permission required"] {
            let status = try DeviceProtocol.receiver(receiverBytes([
                "protocol": 2, "state": "startup_rejected", "listen_endpoint": NSNull(), "error": reason
            ]))
            XCTAssertEqual(status.state, .startupRejected)
            XCTAssertEqual(status.error, reason)
        }
        for (endpoint, reason) in [
            (NSNull() as Any, NSNull() as Any),
            (NSNull() as Any, "receiver failed" as Any),
            (NSNull() as Any, "accessibility permission required " as Any),
            ("0.0.0.0:24800" as Any, "accessibility permission required" as Any)
        ] {
            XCTAssertThrowsError(try DeviceProtocol.receiver(receiverBytes([
                "protocol": 2, "state": "startup_rejected", "listen_endpoint": endpoint, "error": reason
            ])))
        }
    }

    func testCommandsAreBoundedSingleLinesAndValidateBeforeEncoding() throws {
        let begin = try DeviceProtocol.begin(name: "MacBook Air")
        let approve = try DeviceProtocol.approve(attempt: attempt)
        for data in [begin, approve, DeviceProtocol.stop(), DeviceProtocol.cancel()] {
            XCTAssertEqual(data.last, 10)
            XCTAssertEqual(data.filter { $0 == 10 }.count, 1)
            XCTAssertLessThanOrEqual(data.count, 1024)
            XCTAssertNoThrow(try JSONSerialization.jsonObject(with: data))
        }
        XCTAssertThrowsError(try DeviceProtocol.begin(name: " Mac"))
        XCTAssertThrowsError(try DeviceProtocol.begin(name: "Mac\nother"))
        XCTAssertThrowsError(try DeviceProtocol.begin(name: String(repeating: "a", count: 65)))
        XCTAssertThrowsError(try DeviceProtocol.approve(attempt: "\"}\n{\"op\":\"approve\"}"))
    }
}
