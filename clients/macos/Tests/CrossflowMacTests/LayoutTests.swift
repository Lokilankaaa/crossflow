import XCTest
@testable import CrossflowMac

final class LayoutTests: XCTestCase {
    func testImportPreservesUnknownFieldsForRustRejection() throws {
        let data = Data(#"{"screens":[{"id":"a","device_id":"d","x":0,"y":0,"width":10,"height":10,"unknown_screen":7}],"extra":{"nested":true}}"#.utf8)
        let doc = try LayoutDocument(data: data)
        let expected = try JSONDecoder().decode(JSONValue.self, from: data)
        XCTAssertEqual(doc.json, expected)
    }
    func testDraftDoesNotSilentlyValidateOrDeduplicate() throws {
        let data = Data(#"{"screens":[{"id":"a","device_id":"d","x":0,"y":0,"width":-1,"height":10},{"id":"a","device_id":"d","x":0,"y":0,"width":10,"height":10}]}"#.utf8)
        let doc = try LayoutDocument(data: data)
        XCTAssertEqual(doc.screens.count, 2)
        XCTAssertEqual(doc.screens[0].width, -1)
        XCTAssertNotEqual(doc.screens[0].id, doc.screens[1].id)
        XCTAssertEqual(doc.screens[0].screenID, doc.screens[1].screenID)
    }
    func testNumericTransportPreservesNextRepresentableDouble() throws {
        var document = fixture()
        document.screens[0].x = 1920.0.nextDown
        let roundTrip = try LayoutDocument(data: document.json.data())
        XCTAssertEqual(roundTrip.screens[0].x, 1920.0.nextDown)
        XCTAssertNotEqual(roundTrip.screens[0].x, 1920)
    }
    func testWrongFieldTypesAreImportErrors() {
        XCTAssertThrowsError(try LayoutDocument(data: Data(#"{"screens":[{"id":"a","device_id":"d","x":"0","y":0,"width":10,"height":10}]}"#.utf8)))
    }
    @MainActor func testUndoRedoKeepsSelectionAndInvalidatesValidation() {
        let model = WorkspaceModel(initialDocument: fixture())
        var screen = model.document.screens[0]
        model.validationPassed = true
        screen.x = 37
        model.update(screen)
        XCTAssertFalse(model.validationPassed)
        XCTAssertTrue(model.canUndo)
        model.undo()
        XCTAssertEqual(model.selectedScreen?.x, 0)
        XCTAssertTrue(model.canRedo)
        model.redo()
        XCTAssertEqual(model.selectedScreen?.x, 37)
    }
    @MainActor func testOldValidResponseCannotOverwriteEditedDraft() async {
        let bridge = DeferredBridge()
        let model = WorkspaceModel(bridge: bridge, initialDocument: fixture())
        let validation = Task { await model.validate() }
        await bridge.waitForRequest()
        var changed = model.document.screens[0]
        changed.width = -10
        model.update(changed)
        await bridge.resolve(.object(["ok": .bool(true), "api_version": .number(1)]))
        await validation.value
        XCTAssertFalse(model.validationPassed)
        XCTAssertEqual(model.validationMessage, "草稿有更改 · 尚未校验")
    }
    private func fixture() -> LayoutDocument {
        LayoutDocument(screens: [ScreenDraft(screenID: "a", deviceID: "d", x: 0, y: 0, width: 10, height: 10)])
    }
}

private actor DeferredBridge: BridgeRequesting {
    var result: CheckedContinuation<JSONValue, any Error>?
    var started: CheckedContinuation<Void, Never>?
    func request(_ value: JSONValue) async throws -> JSONValue {
        try await withCheckedThrowingContinuation { continuation in
            result = continuation
            started?.resume(); started = nil
        }
    }
    func waitForRequest() async {
        if result != nil { return }
        await withCheckedContinuation { started = $0 }
    }
    func resolve(_ value: JSONValue) {
        result?.resume(returning: value); result = nil
    }
}
