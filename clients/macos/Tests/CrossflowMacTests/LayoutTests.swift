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
}
