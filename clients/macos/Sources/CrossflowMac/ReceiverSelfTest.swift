import Foundation

/// Pure protocol fixtures. No receiver, network, Keychain, window or OS input.
enum ReceiverSelfTest {
    static let source = String(repeating: "a", count: 64)
    static let receiver = String(repeating: "b", count: 64)

    static func fixture() -> [String: Any] {
        ["protocol": 2, "state": "sharing", "listen_endpoint": "0.0.0.0:24800", "error": NSNull(),
         "route_enabled": true, "input_active": false, "focus_device_id": source, "focus_screen_id": "win-main",
         "shared_layout": ["schema": 1, "source_device_id": source, "receiver_device_id": receiver,
                           "screens": [
                            ["id": "win-main", "device_id": source, "native_id": "DISPLAY1", "x": -1920.5, "y": 0,
                             "width": 1920.5, "height": 1080],
                            ["id": "mac-main", "device_id": receiver, "native_id": "1", "x": 0, "y": 0,
                             "width": 1470, "height": 956]]]]
    }

    static func data(_ object: [String: Any]) throws -> Data {
        try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    }

    static func checks() -> [String: Bool] {
        var checks: [String: Bool] = [:]
        func accepts(_ value: [String: Any]) -> ReceiverStatus? {
            guard let bytes = try? data(value) else { return nil }
            return try? DeviceProtocol.receiver(bytes)
        }
        func rejects(_ name: String, _ mutate: (inout [String: Any]) -> Void) {
            var value = fixture(); mutate(&value)
            checks["receiver_" + name] = accepts(value) == nil
        }
        let initial = accepts(fixture())
        checks["receiver_enabled_on_source"] = initial?.routeEnabled == true && initial?.inputActive == false
            && initial?.focusDeviceID == source && initial?.sharedLayout?.screens.first?.x == -1920.5
        var local = fixture(); local["focus_device_id"] = receiver; local["focus_screen_id"] = "mac-main"; local["input_active"] = true
        checks["receiver_local_input_active"] = accepts(local)?.inputActive == true
        var unknown = fixture(); unknown["focus_device_id"] = NSNull(); unknown["focus_screen_id"] = NSNull()
        checks["receiver_focus_pending_is_valid"] = accepts(unknown) != nil
        // Core status can update batch state and actual input separately.
        local["input_active"] = false
        checks["receiver_transient_focus_without_input"] = accepts(local) != nil
        var stopped = unknown; stopped["state"] = "listening"; stopped["route_enabled"] = false; stopped["shared_layout"] = NSNull()
        checks["receiver_disconnect_clears_metadata"] = accepts(stopped)?.sharedLayout == nil
        var nonrouted = unknown; nonrouted["shared_layout"] = NSNull(); nonrouted["input_active"] = true
        checks["receiver_nonrouted_mode"] = accepts(nonrouted)?.inputActive == true
        rejects("old_stdout_rejected") { $0["protocol"] = 1 }
        rejects("unknown_field_rejected") { $0["extra"] = true }
        rejects("missing_field_rejected") { $0.removeValue(forKey: "route_enabled") }
        rejects("numeric_bool_rejected") { $0["route_enabled"] = 1 }
        rejects("partial_focus_rejected") { $0["focus_screen_id"] = NSNull() }
        rejects("unknown_focus_rejected") { $0["focus_screen_id"] = "absent" }
        rejects("wrong_focus_owner_rejected") { $0["focus_device_id"] = receiver }
        rejects("paused_focus_rejected") { $0["route_enabled"] = false }
        rejects("stopped_metadata_rejected") { $0["state"] = "stopped" }
        for (name, field, bad) in [("schema", "schema", 2 as Any), ("source", "source_device_id", "local" as Any),
                                   ("same_peer", "receiver_device_id", source as Any)] {
            rejects(name + "_rejected") { value in
                var layout = value["shared_layout"] as! [String: Any]; layout[field] = bad; value["shared_layout"] = layout
            }
        }
        let changes: [(String, String, Any)] = [
            ("zero_width", "width", 0), ("subpoint_height", "height", 0.5), ("unbounded_x", "x", 1_000_001),
            ("negative_width", "width", -1), ("numeric_string", "x", "0"), ("bool_geometry", "x", true),
            ("wrong_owner", "device_id", String(repeating: "c", count: 64)),
            ("empty_id", "id", ""), ("long_id", "id", String(repeating: "x", count: 129)),
            ("control_native_id", "native_id", "display\n1"), ("extra_screen_field", "extra", 0)
        ]
        for (name, key, bad) in changes {
            rejects(name + "_rejected") { value in
                var layout = value["shared_layout"] as! [String: Any]
                var screens = layout["screens"] as! [[String: Any]]
                screens[0][key] = bad; layout["screens"] = screens; value["shared_layout"] = layout
            }
        }
        for name in ["overlap", "duplicate_id", "duplicate_native", "missing_native"] {
            rejects(name + "_rejected") { value in
                var layout = value["shared_layout"] as! [String: Any]
                var screens = layout["screens"] as! [[String: Any]]
                switch name {
                case "overlap": screens[1]["x"] = -100
                case "duplicate_id": screens[1]["id"] = "win-main"
                case "duplicate_native":
                    var third = screens[0]; third["id"] = "win-other"; third["y"] = 1080; screens.append(third)
                default: screens[0].removeValue(forKey: "native_id")
                }
                layout["screens"] = screens; value["shared_layout"] = layout
            }
        }
        for (name, count) in [("empty_layout", 0), ("duplicate_screens", 3), ("too_many_screens", 33)] {
            rejects(name + "_rejected") { value in
                var layout = value["shared_layout"] as! [String: Any]
                let screens = layout["screens"] as! [[String: Any]]
                layout["screens"] = Array(repeating: screens[0], count: count); value["shared_layout"] = layout
            }
        }
        if let bytes = try? data(fixture()), let raw = String(data: bytes, encoding: .utf8) {
            for (name, before, after) in [
                ("duplicate", "\"protocol\":2", "\"protocol\":2,\"protocol\":2"),
                ("escaped_duplicate", "\"protocol\":2", "\"protocol\":2,\"\\u0070rotocol\":2"),
                ("float_version", "\"protocol\":2", "\"protocol\":2.0"),
                ("exponent_version", "\"protocol\":2", "\"protocol\":2e0"),
                ("nonfinite_geometry", "-1920.5", "1e999")
            ] {
                checks["receiver_" + name + "_rejected"] = (try? DeviceProtocol.receiver(Data(raw.replacingOccurrences(of: before, with: after).utf8))) == nil
            }
            checks["receiver_large_line_supported"] = (try? DeviceProtocol.receiver(bytes + Data(repeating: 32, count: 4096))) != nil
            checks["receiver_size_limit"] = (try? DeviceProtocol.receiver(bytes + Data(repeating: 32, count: DeviceProtocol.maxReceiverBytes))) == nil
        }
        return checks
    }
}
