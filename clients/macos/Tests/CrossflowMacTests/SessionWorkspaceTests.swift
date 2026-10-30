import XCTest
@testable import CrossflowMac

final class SessionWorkspaceTests: XCTestCase {
    func testAuthenticatedReceiverBoundary() {
        let checks = ReceiverSelfTest.checks()
        XCTAssertGreaterThan(checks.count, 30)
        for (name, passed) in checks { XCTAssertTrue(passed, name) }
    }

    @MainActor func testSessionUpdatesRetainStableScreenIDsAndClearDisconnectedFocus() throws {
        let model = WorkspaceModel()
        XCTAssertNil(model.sessionLayout)
        var packet = ReceiverSelfTest.fixture()
        let source = try DeviceProtocol.receiver(ReceiverSelfTest.data(packet))
        model.receiveSession(source)
        XCTAssertEqual(model.sessionLayout?.screens.count, 2)
        XCTAssertEqual(model.focusScreenID, "win-main")
        XCTAssertFalse(model.inputActive)
        let ids = model.sessionLayout?.screens.map(\.id)
        packet["focus_device_id"] = ReceiverSelfTest.receiver
        packet["focus_screen_id"] = "mac-main"
        packet["input_active"] = true
        model.receiveSession(try DeviceProtocol.receiver(ReceiverSelfTest.data(packet)))
        XCTAssertEqual(model.focusScreenID, "mac-main")
        XCTAssertTrue(model.inputActive)
        XCTAssertEqual(model.sessionLayout?.screens.map(\.id), ids)
        let repeated = try DeviceProtocol.receiver(ReceiverSelfTest.data(packet))
        model.receiveSession(repeated)
        XCTAssertEqual(model.sessionLayout, repeated.sharedLayout)
        model.receiveSession(nil)
        XCTAssertNil(model.sessionLayout)
        XCTAssertNil(model.focusScreenID)
        XCTAssertFalse(model.routeEnabled)
        XCTAssertFalse(model.inputActive)
    }

    @MainActor func testErrorAndStopSuppressFocusAndInputEvenWithRetainedMetadata() throws {
        for state in ["error", "stopping"] {
            var packet = ReceiverSelfTest.fixture()
            packet["state"] = state; packet["error"] = "transport closed"; packet["input_active"] = true
            let status = try DeviceProtocol.receiver(ReceiverSelfTest.data(packet))
            let model = WorkspaceModel()
            model.receiveSession(status)
            XCTAssertNil(model.sessionLayout)
            let devices = DeviceModel(executables: [:])
            devices.operation = .receiver; devices.receiverState = status.state; devices.receiverStatus = status
            XCTAssertFalse(devices.isSharing)
        }
    }

    @MainActor func testReceiverReaderAcceptsLargeBoundedLines() async throws {
        let line = String(repeating: "x", count: 4096) + "\n"
        let client = NativeProcessClient()
        let exit = try await client.run(executable: URL(fileURLWithPath: "/usr/bin/printf"),
                                        arguments: ["%s", line], timeout: 3,
                                        maxLineBytes: DeviceProtocol.maxReceiverBytes, onLine: { data in
            XCTAssertEqual(data.count, 4097)
        })
        XCTAssertEqual(exit, 0)
    }
}
