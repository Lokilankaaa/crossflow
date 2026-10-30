import Foundation

struct SavedDevice: Identifiable, Sendable, Equatable {
    let id: String
    let name: String
    let lastEndpoint: String?
}

struct SavedDevices: Sendable {
    let devices: [SavedDevice]
    let invalidRecords: Int
}

// Deliberately no CustomStringConvertible or debug/logging conformance: code
// events contain a short-lived secret intended only for the pairing view.
enum InvitationEvent: Sendable {
    case ready
    case code(String, lifetime: Int)
    case pending(attempt: String, name: String, platform: String, fingerprint: String)
    case attemptFailed(String)
    case discovery(String)
    case saved(id: String, port: Int)
}

struct SharedScreen: Sendable, Equatable, Identifiable {
    let id: String
    let deviceID: String
    let nativeID: String
    let x: Double
    let y: Double
    let width: Double
    let height: Double
}

struct SharedLayout: Sendable, Equatable {
    let sourceDeviceID: String
    let receiverDeviceID: String
    let screens: [SharedScreen]
}

struct ReceiverStatus: Sendable, Equatable {
    enum State: String, Sendable {
        case starting, listening, paused, sharing, stopping, stopped, error
        case startupRejected = "startup_rejected"
    }
    let state: State
    let listenEndpoint: String?
    let error: String?
    let routeEnabled: Bool
    let focusDeviceID: String?
    let focusScreenID: String?
    let sharedLayout: SharedLayout?
    let inputActive: Bool

    // Pair roles describe the negotiated layout, not the current physical
    // input source. Derive this Mac's identity from its authenticated peer.
    func localDeviceID(peerID: String?) -> String? {
        guard let peerID, let layout = sharedLayout else { return nil }
        if layout.sourceDeviceID == peerID { return layout.receiverDeviceID }
        if layout.receiverDeviceID == peerID { return layout.sourceDeviceID }
        return nil
    }
}

enum DeviceProtocolError: LocalizedError, Sendable {
    case invalidResult
    var errorDescription: String? { "设备服务返回了无效数据，请更新客户端后重试。" }
}

/// Boundary for the packaged Rust workers. Reject unknown fields and duplicate
/// keys before exposing any status. Decoder errors never escape with raw data.
enum DeviceProtocol {
    static let maxPeerBytes = 1_048_576
    static let maxLineBytes = 2048
    static let maxReceiverBytes = 65_536

    static func savedDevices(_ data: Data) throws -> SavedDevices {
        let root = try object(data, limit: maxPeerBytes)
        try fields(root, ["peers", "invalid_records"])
        guard case let .array(peers)? = root["peers"], peers.count <= 64 else { throw invalid }
        let invalidRecords = try integer(root, "invalid_records", maximum: 64)
        guard peers.count + invalidRecords <= 64 else { throw invalid }
        var seen = Set<String>()
        let devices = try peers.map { value -> SavedDevice in
            guard case let .object(peer) = value else { throw invalid }
            try fields(peer, ["schema", "id", "friendly_name", "last_endpoint"])
            guard try integer(peer, "schema", maximum: 1) == 1 else { throw invalid }
            let id = try string(peer, "id")
            let name = try string(peer, "friendly_name")
            guard hex(id, count: 64), seen.insert(id).inserted, validName(name, bytes: 256) else { throw invalid }
            let endpoint = try optionalString(peer, "last_endpoint")
            if let endpoint { guard validEndpoint(endpoint, routing: true) else { throw invalid } }
            return SavedDevice(id: id, name: name, lastEndpoint: endpoint)
        }
        return SavedDevices(devices: devices, invalidRecords: invalidRecords)
    }

    static func invitation(_ data: Data) throws -> InvitationEvent {
        let value = try object(data, limit: maxLineBytes)
        switch try string(value, "event") {
        case "invitation_ready":
            try fields(value, ["event", "pairing_endpoint", "reserved_input_endpoint", "input_ready"])
            guard value["input_ready"] == .bool(false),
                  validEndpoint(try string(value, "pairing_endpoint")),
                  validEndpoint(try string(value, "reserved_input_endpoint")) else { throw invalid }
            return .ready
        case "code":
            try fields(value, ["event", "code", "lifetime_seconds"])
            let code = try string(value, "code")
            let lifetime = try integer(value, "lifetime_seconds", maximum: 120)
            guard code.utf8.count == 8, asciiDigits(code), lifetime == 120 else { throw invalid }
            return .code(code, lifetime: lifetime)
        case "pending":
            try fields(value, ["event", "attempt", "name", "platform", "fingerprint"])
            let attempt = try string(value, "attempt"), name = try string(value, "name")
            let platform = try string(value, "platform"), fingerprint = try string(value, "fingerprint")
            guard hex(attempt, count: 32), validName(name, bytes: 96),
                  ["windows", "macos"].contains(platform), hex(fingerprint, count: 64) else { throw invalid }
            return .pending(attempt: attempt, name: name, platform: platform, fingerprint: fingerprint)
        case "attempt_failed":
            try fields(value, ["event", "attempt"])
            let attempt = try string(value, "attempt")
            guard hex(attempt, count: 32) else { throw invalid }
            return .attemptFailed(attempt)
        case "discovery":
            try fields(value, ["event", "state"])
            let state = try string(value, "state")
            guard ["starting", "browsing", "stopping", "stopped", "error"].contains(state) else { throw invalid }
            return .discovery(state)
        case "saved":
            try fields(value, ["event", "id", "suggested_receive_port", "input_ready"])
            let id = try string(value, "id")
            let port = try integer(value, "suggested_receive_port", maximum: 65535)
            guard hex(id, count: 64), port > 0, value["input_ready"] == .bool(false) else { throw invalid }
            return .saved(id: id, port: port)
        default: throw invalid
        }
    }

    static func receiver(_ data: Data) throws -> ReceiverStatus {
        let value = try object(data, limit: maxReceiverBytes)
        try fields(value, ["protocol", "state", "listen_endpoint", "error", "route_enabled", "focus_device_id", "focus_screen_id", "shared_layout", "input_active"])
        guard try integer(value, "protocol", maximum: 2) == 2,
              let state = ReceiverStatus.State(rawValue: try string(value, "state")) else { throw invalid }
        let endpoint = try optionalString(value, "listen_endpoint")
        if let endpoint { guard validEndpoint(endpoint) else { throw invalid } }
        if [.listening, .paused, .sharing].contains(state), endpoint == nil { throw invalid }
        let error = try optionalString(value, "error")
        if let error {
            guard !error.isEmpty, error.utf8.count <= 512,
                  !error.unicodeScalars.contains(where: forbiddenScalar) else { throw invalid }
        }
        guard state != .error || error != nil, state != .stopped || error == nil else { throw invalid }
        if state == .startupRejected {
            guard endpoint == nil,
                  error == "accessibility permission required" || error == "input monitoring permission required" else { throw invalid }
        }
        let enabled = try boolean(value, "route_enabled")
        let active = try boolean(value, "input_active")
        let focusDevice = try optionalString(value, "focus_device_id")
        let focusScreen = try optionalString(value, "focus_screen_id")
        let layout = try sharedLayout(value["shared_layout"])
        guard (focusDevice == nil) == (focusScreen == nil), enabled || focusDevice == nil else { throw invalid }
        if let focusDevice, let focusScreen {
            guard let layout, layout.screens.contains(where: { $0.id == focusScreen && $0.deviceID == focusDevice }) else { throw invalid }
        }
        if layout == nil { guard focusDevice == nil else { throw invalid } }
        if [.starting, .listening, .stopped, .startupRejected].contains(state) {
            guard !enabled, !active, layout == nil, focusDevice == nil else { throw invalid }
        }
        return ReceiverStatus(state: state, listenEndpoint: endpoint, error: error,
                              routeEnabled: enabled, focusDeviceID: focusDevice, focusScreenID: focusScreen,
                              sharedLayout: layout, inputActive: active)
    }

    private static func sharedLayout(_ value: Value?) throws -> SharedLayout? {
        if value == .null { return nil }
        guard case let .object(root)? = value else { throw invalid }
        try fields(root, ["schema", "source_device_id", "receiver_device_id", "screens"])
        guard try integer(root, "schema", maximum: 1) == 1 else { throw invalid }
        let source = try string(root, "source_device_id"), receiver = try string(root, "receiver_device_id")
        guard hex(source, count: 64), hex(receiver, count: 64), source != receiver,
              case let .array(items)? = root["screens"], !items.isEmpty, items.count <= 32 else { throw invalid }
        var ids = Set<String>(), nativeIDs = Set<String>()
        let screens = try items.map { item -> SharedScreen in
            guard case let .object(screen) = item else { throw invalid }
            try fields(screen, ["id", "device_id", "native_id", "x", "y", "width", "height"])
            let id = try string(screen, "id"), device = try string(screen, "device_id"), native = try string(screen, "native_id")
            guard validIdentifier(id, bytes: 128), validIdentifier(native, bytes: 256), ids.insert(id).inserted,
                  [source, receiver].contains(device), nativeIDs.insert(device + ":" + native).inserted else { throw invalid }
            let x = try number(screen, "x"), y = try number(screen, "y")
            let width = try number(screen, "width"), height = try number(screen, "height")
            guard width >= 1, height >= 1, [x, y, width, height].allSatisfy({ abs($0) <= 1_000_000 }) else { throw invalid }
            return SharedScreen(id: id, deviceID: device, nativeID: native, x: x, y: y, width: width, height: height)
        }
        guard screens.contains(where: { $0.deviceID == source }), screens.contains(where: { $0.deviceID == receiver }) else { throw invalid }
        for (index, first) in screens.enumerated() {
            for second in screens.dropFirst(index + 1) {
                guard !(first.x < second.x + second.width && second.x < first.x + first.width
                        && first.y < second.y + second.height && second.y < first.y + first.height) else { throw invalid }
            }
        }
        return SharedLayout(sourceDeviceID: source, receiverDeviceID: receiver, screens: screens)
    }

    private static func validIdentifier(_ value: String, bytes: Int) -> Bool {
        !value.isEmpty && value.utf8.count <= bytes && !value.unicodeScalars.contains(where: forbiddenScalar)
    }

    private static func boolean(_ value: [String: Value], _ key: String) throws -> Bool {
        guard case let .bool(result)? = value[key] else { throw invalid }
        return result
    }
    private static func number(_ value: [String: Value], _ key: String) throws -> Double {
        let result: Double
        switch value[key] {
        case let .number(number): result = number
        case let .integer(integer): result = Double(integer)
        default: throw invalid
        }
        guard result.isFinite else { throw invalid }
        return result
    }

    static func begin(name: String) throws -> Data {
        guard validName(name, bytes: 96) else { throw invalid }
        return try command(["op": "begin", "name": name])
    }
    static func approve(attempt: String) throws -> Data {
        guard hex(attempt, count: 32) else { throw invalid }
        return try command(["op": "approve", "attempt": attempt])
    }
    static func stop() -> Data { Data("{\"op\":\"stop\"}\n".utf8) }
    static func cancel() -> Data { Data("{\"op\":\"cancel\"}\n".utf8) }

    private static var invalid: DeviceProtocolError { .invalidResult }
    private static func command(_ values: [String: String]) throws -> Data {
        do {
            var data = try JSONEncoder().encode(values)
            data.append(10)
            guard data.count <= 1024 else { throw invalid }
            return data
        } catch { throw invalid }
    }
    private static func object(_ data: Data, limit: Int) throws -> [String: Value] {
        guard !data.isEmpty, data.count <= limit else { throw invalid }
        do {
            var syntax = DeviceUniqueJSON(data: data)
            try syntax.check()
            guard case let .object(value) = try JSONDecoder().decode(Value.self, from: data) else { throw invalid }
            return value
        } catch { throw invalid }
    }
    private static func fields(_ value: [String: Value], _ expected: Set<String>) throws {
        guard Set(value.keys) == expected else { throw invalid }
    }
    private static func string(_ value: [String: Value], _ key: String) throws -> String {
        guard case let .string(result)? = value[key] else { throw invalid }
        return result
    }
    private static func optionalString(_ value: [String: Value], _ key: String) throws -> String? {
        if value[key] == .null { return nil }
        return try string(value, key)
    }
    private static func integer(_ value: [String: Value], _ key: String, maximum: Int) throws -> Int {
        guard case let .integer(result)? = value[key], result <= UInt64(maximum) else { throw invalid }
        return Int(result)
    }
    private static func asciiDigits(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.allSatisfy { (48...57).contains($0) }
    }
    private static func hex(_ value: String, count: Int) -> Bool {
        value.utf8.count == count && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
    }
    private static func forbiddenScalar(_ scalar: Unicode.Scalar) -> Bool {
        CharacterSet.controlCharacters.contains(scalar)
            || [0x061c, 0x200e, 0x200f, 0xfeff].contains(scalar.value)
            || (0x2028...0x202e).contains(scalar.value) || (0x2066...0x206f).contains(scalar.value)
    }
    private static func validName(_ name: String, bytes: Int) -> Bool {
        !name.isEmpty && name.utf8.count <= bytes && name.unicodeScalars.count <= 64
            && name.trimmingCharacters(in: .whitespacesAndNewlines) == name
            && !name.unicodeScalars.contains(where: forbiddenScalar)
    }

    /// Rust SocketAddr syntax only; never accepts DNS names or URL decoration.
    /// No networking or platform APIs are used by this parser.
    private static func validEndpoint(_ endpoint: String, routing: Bool = false) -> Bool {
        guard endpoint.utf8.count <= 96 else { return false }
        let host: String, portText: String
        let bytes: [UInt8]
        if endpoint.hasPrefix("["), let end = endpoint.firstIndex(of: "]") {
            host = String(endpoint[endpoint.index(after: endpoint.startIndex)..<end])
            let tail = endpoint[endpoint.index(after: end)...]
            guard tail.hasPrefix(":"), let address = ipv6(host) else { return false }
            portText = String(tail.dropFirst()); bytes = address
        } else {
            let pieces = endpoint.split(separator: ":", omittingEmptySubsequences: false)
            guard pieces.count == 2, let address = ipv4(String(pieces[0])) else { return false }
            host = String(pieces[0]); portText = String(pieces[1]); bytes = address
        }
        guard asciiDigits(portText), let port = UInt16(portText), port > 0 else { return false }
        guard routing else { return true }
        let normalized = bytes.count == 16 && bytes.prefix(10).allSatisfy({ $0 == 0 }) && bytes[10] == 255 && bytes[11] == 255
            ? Array(bytes.suffix(4)) : bytes
        guard !normalized.allSatisfy({ $0 == 0 }) else { return false }
        if normalized.count == 4 {
            return !(224...239).contains(normalized[0]) && !normalized.allSatisfy({ $0 == 255 })
        }
        return normalized[0] != 255
    }
    private static func ipv4(_ text: String) -> [UInt8]? {
        let parts = text.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 4 else { return nil }
        var bytes: [UInt8] = []
        for part in parts {
            guard asciiDigits(String(part)), part.count == 1 || part.first != "0", let byte = UInt8(part) else { return nil }
            bytes.append(byte)
        }
        return bytes
    }
    private static func ipv6(_ text: String) -> [UInt8]? {
        let scopeParts = text.split(separator: "%", omittingEmptySubsequences: false)
        guard (1...2).contains(scopeParts.count) else { return nil }
        if scopeParts.count == 2 {
            guard asciiDigits(String(scopeParts[1])), UInt32(scopeParts[1]) != nil else { return nil }
        }
        let address = String(scopeParts[0])
        let sides = address.components(separatedBy: "::")
        guard (1...2).contains(sides.count) else { return nil }
        func groups(_ side: String, allowIPv4: Bool) -> [UInt16]? {
            if side.isEmpty { return [] }
            let parts = side.split(separator: ":", omittingEmptySubsequences: false)
            var words: [UInt16] = []
            for (index, part) in parts.enumerated() {
                if part.contains(".") {
                    guard allowIPv4, index == parts.count - 1, let bytes = ipv4(String(part)) else { return nil }
                    words += [UInt16(bytes[0]) << 8 | UInt16(bytes[1]), UInt16(bytes[2]) << 8 | UInt16(bytes[3])]
                } else {
                    guard (1...4).contains(part.utf8.count),
                          part.utf8.allSatisfy({ (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0) }),
                          let word = UInt16(part, radix: 16) else { return nil }
                    words.append(word)
                }
            }
            return words
        }
        guard let left = groups(sides[0], allowIPv4: sides.count == 1) else { return nil }
        let words: [UInt16]
        if sides.count == 2 {
            guard let right = groups(sides[1], allowIPv4: true), left.count + right.count < 8 else { return nil }
            words = left + Array(repeating: 0, count: 8 - left.count - right.count) + right
        } else {
            guard left.count == 8 else { return nil }
            words = left
        }
        return words.flatMap { [UInt8($0 >> 8), UInt8($0 & 255)] }
    }

    private enum Value: Decodable, Equatable {
        case object([String: Value]), array([Value]), string(String), integer(UInt64), number(Double), bool(Bool), null
        init(from decoder: Decoder) throws {
            let value = try decoder.singleValueContainer()
            if value.decodeNil() { self = .null }
            else if let result = try? value.decode(Bool.self) { self = .bool(result) }
            else if let result = try? value.decode(UInt64.self) { self = .integer(result) }
            else if let result = try? value.decode(Double.self) { self = .number(result) }
            else if let result = try? value.decode(String.self) { self = .string(result) }
            else if let result = try? value.decode([Value].self) { self = .array(result) }
            else { self = .object(try value.decode([String: Value].self)) }
        }
    }
}

/// Reject duplicate/escaped-duplicate keys and excess nesting before decoding.
private struct DeviceUniqueJSON {
    let bytes: [UInt8]
    var index = 0
    init(data: Data) { bytes = Array(data) }
    mutating func check() throws {
        try value(depth: 0); whitespace()
        guard index == bytes.count else { throw DeviceProtocolError.invalidResult }
    }
    mutating func whitespace() { while index < bytes.count && [9, 10, 13, 32].contains(bytes[index]) { index += 1 } }
    mutating func consume(_ byte: UInt8) throws {
        whitespace()
        guard index < bytes.count, bytes[index] == byte else { throw DeviceProtocolError.invalidResult }
        index += 1
    }
    mutating func string() throws -> String {
        whitespace(); let start = index; try consume(34)
        var escaped = false
        while index < bytes.count {
            let byte = bytes[index]; index += 1
            if !escaped && byte == 34 { return try JSONDecoder().decode(String.self, from: Data(bytes[start..<index])) }
            if !escaped && byte == 92 { escaped = true } else { escaped = false }
        }
        throw DeviceProtocolError.invalidResult
    }
    mutating func value(depth: Int, key: String? = nil) throws {
        whitespace()
        guard depth < 8, index < bytes.count else { throw DeviceProtocolError.invalidResult }
        switch bytes[index] {
        case 123, 91:
            let object = bytes[index] == 123
            let end: UInt8 = object ? 125 : 93
            index += 1; whitespace()
            if index < bytes.count && bytes[index] == end { index += 1; return }
            var keys = Set<String>()
            while true {
                var childKey: String?
                if object {
                    childKey = try string()
                    guard keys.insert(childKey!).inserted else { throw DeviceProtocolError.invalidResult }
                    try consume(58)
                }
                try value(depth: depth + 1, key: childKey); whitespace()
                guard index < bytes.count else { throw DeviceProtocolError.invalidResult }
                if bytes[index] == end { index += 1; return }
                try consume(44)
            }
        case 34: _ = try string()
        default:
            let start = index
            while index < bytes.count && ![9, 10, 13, 32, 44, 93, 125].contains(bytes[index]) { index += 1 }
            guard index > start else { throw DeviceProtocolError.invalidResult }
            let token = bytes[start..<index]
            let literal = String(decoding: token, as: UTF8.self)
            if !["true", "false", "null"].contains(literal) {
                if let key, ["x", "y", "width", "height"].contains(key) {
                    guard let number = try? JSONDecoder().decode(Double.self, from: Data(token)), number.isFinite else { throw DeviceProtocolError.invalidResult }
                } else {
                    guard token.allSatisfy({ (48...57).contains($0) }), token.count == 1 || token.first != 48 else { throw DeviceProtocolError.invalidResult }
                }
            }
        }
    }
}
