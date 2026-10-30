import Foundation

/// Invoked only by --self-test. No NSApplication, file panels, input capture,
/// receiver, event creation, or event posting is started on this path.
enum SelfTest {
    static func run(output: URL) async -> Int32 {
        var checks = DiscoverySelfTest.checks()
        checks.merge(ReceiverSelfTest.checks()) { _, new in new }
        var details: [String: JSONValue] = [:]
        do {
            let bridge = BridgeClient()
            let environment = try await bridge.request(.object(["op": .string("environment")]))
            details["environment"] = environment
            checks["bridge_api_v1"] = environment["api_version"]?.number == 1 && environment["ok"] == .bool(true)

            let fourScreens = try LayoutDocument.bundledExample()
            checks["shared_four_screen_sample"] = fourScreens.screens.count == 4 && Set(fourScreens.screens.map(\.deviceID)).count == 2
            let fourValid = try await bridge.request(.object(["op": .string("layout_validate"), "layout": fourScreens.json]))
            checks["four_screen_sample_valid"] = fourValid["ok"] == .bool(true)
            let example = try LayoutDocument.bundledExample(resource: "layout-two-devices")
            guard example.screens.count == 2 else { throw DocumentError.shape }
            let layout = example.json
            let valid = try await bridge.request(.object(["op": .string("layout_validate"), "layout": layout]))
            checks["valid_layout_accepted"] = valid["ok"] == .bool(true)
            var overlap = example
            overlap.screens[1].x = 1900
            let invalid = try await bridge.request(.object(["op": .string("layout_validate"), "layout": overlap.json]))
            checks["overlap_rejected"] = invalid["ok"] == .bool(false)

            var unknown = example
            unknown.extra["extra"] = .bool(true)
            let decoded = try LayoutDocument(data: unknown.json.data())
            checks["unknown_fields_preserved"] = decoded.extra["extra"] == .bool(true)
            let unknownResult = try await bridge.request(.object(["op": .string("layout_validate"), "layout": decoded.json]))
            checks["unknown_fields_rejected_by_rust"] = unknownResult["ok"] == .bool(false)

            let crossing = try await bridge.request(.object([
                "op": .string("layout_crossing"), "layout": layout,
                "from_screen": .string("windows-main"),
                "start": .object(["x": .number(1919), "y": .number(500)]),
                "end": .object(["x": .number(1920.0.nextUp), "y": .number(500)])
            ]))
            details["crossing"] = crossing
            checks["crossing_targets_mac"] = crossing["ok"] == .bool(true) && crossing["result"]?["crossing"]?["target_screen_id"] == .string("mac-main") && crossing["result"]?["system_input_injected"] == .bool(false)

            var tinyOverlap = example
            tinyOverlap.screens[1].x = 1920.0.nextDown
            let tinyResult = try await bridge.request(.object(["op": .string("layout_validate"), "layout": tinyOverlap.json]))
            checks["one_ulp_overlap_rejected"] = tinyResult["ok"] == .bool(false)
            details["one_ulp_overlap"] = tinyResult
            let roundTrip = try LayoutDocument(data: tinyOverlap.json.data())
            checks["swift_double_roundtrip"] = roundTrip.screens[1].x == 1920.0.nextDown
        } catch {
            details["failure"] = .string(error.localizedDescription)
            checks["completed"] = false
        }
        let passed = !checks.isEmpty && checks.values.allSatisfy { $0 }
        let report: JSONValue = .object([
            "ok": .bool(passed), "actual_os_input": .bool(false), "window_created": .bool(false),
            "checks": .object(checks.mapValues(JSONValue.bool)), "details": .object(details)
        ])
        do {
            try report.data(pretty: true).write(to: output, options: .atomic)
            print("Native Swift bridge self-test: \(passed ? "PASS" : "FAIL") · \(output.path)")
        } catch {
            FileHandle.standardError.write(Data("Cannot write self-test report: \(error.localizedDescription)\n".utf8))
            return 2
        }
        return passed ? 0 : 1
    }
}
