import Foundation

/// Pure decoder checks shared by --self-test and XCTest. No process or network.
enum DiscoverySelfTest {
    static let validSnapshot = #"{"state":"browsing","candidates":[{"device_id":"0123456789abcdef0123456789abcdef","friendly_name":"测试 Mac","platform":"macos","protocol":3,"compatible":true,"trusted":false,"addresses":["192.168.1.2:24800","[fe80::123%7]:24800"]}],"capacity_limited":false,"error":null}"#

    static func checks() -> [String: Bool] {
        var checks: [String: Bool] = [:]
        if let result = try? DiscoveryProtocol.parse(Data(validSnapshot.utf8)) {
            checks["discovery_valid_snapshot"] = result.devices.count == 1
                && result.devices.first?.name == "测试 Mac"
                && result.devices.first?.compatible == true
                && result.devices.first?.addresses.count == 2 && !result.capacityLimited
        } else { checks["discovery_valid_snapshot"] = false }
        let legacy = validSnapshot.replacingOccurrences(of: "\"protocol\":3", with: "\"protocol\":2")
        checks["discovery_legacy_wire_incompatible"] = (try? DiscoveryProtocol.parse(Data(legacy.utf8)))?
            .devices.first?.compatible == false
        let future = validSnapshot.replacingOccurrences(of: "\"protocol\":3", with: "\"protocol\":4")
        checks["discovery_future_wire_incompatible"] = (try? DiscoveryProtocol.parse(Data(future.utf8)))?
            .devices.first?.compatible == false
        let mutations: [(String, String, String)] = [
            ("trusted_rejected", "\"trusted\":false", "\"trusted\":true"),
            ("duplicate_key_rejected", "\"trusted\":false", "\"trusted\":false,\"trusted\":false"),
            ("escaped_duplicate_rejected", "\"trusted\":false", "\"trusted\":false,\"tru\\u0073ted\":false"),
            ("unknown_field_rejected", "\"trusted\":false", "\"trusted\":false,\"unknown\":0"),
            ("missing_field_rejected", "\"trusted\":false,", ""),
            ("numeric_bool_rejected", "\"compatible\":true", "\"compatible\":1"),
            ("zero_protocol_rejected", "\"protocol\":3", "\"protocol\":0"),
            ("float_protocol_rejected", "\"protocol\":3", "\"protocol\":3.0"),
            ("exponent_protocol_rejected", "\"protocol\":3", "\"protocol\":3e0"),
            ("bidi_name_rejected", "测试 Mac", "伪装\\u202eMac"),
            ("loopback_rejected", "192.168.1.2:24800", "127.0.0.1:24800"),
            ("unscoped_linklocal_rejected", "[fe80::123%7]:24800", "[fe80::123]:24800"),
            ("duplicate_endpoint_rejected", "[fe80::123%7]:24800", "192.168.1.2:24800"),
            ("daemon_error_rejected", "\"error\":null", "\"error\":\"daemon_failed\"")
        ]
        for (name, before, after) in mutations {
            checks["discovery_" + name] = rejects(Data(validSnapshot.replacingOccurrences(of: before, with: after).utf8))
        }
        checks["discovery_invalid_utf8_rejected"] = rejects(Data([0xff]))
        checks["discovery_output_limit"] = rejects(Data(repeating: 32, count: DiscoveryProtocol.maxOutputBytes + 1))
        return checks
    }

    private static func rejects(_ data: Data) -> Bool {
        do { _ = try DiscoveryProtocol.parse(data); return false }
        catch { return true }
    }
}
