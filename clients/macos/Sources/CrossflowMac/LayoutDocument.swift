import Foundation

/// JSON transport only. Geometry validity and crossing decisions belong to Rust.
enum JSONValue: Codable, Equatable, Sendable {
    case object([String: JSONValue]), array([JSONValue]), string(String)
    case number(Double), bool(Bool), null

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let v = try? c.decode(Bool.self) { self = .bool(v) }
        else if let v = try? c.decode(Double.self) { self = .number(v) }
        else if let v = try? c.decode(String.self) { self = .string(v) }
        else if let v = try? c.decode([JSONValue].self) { self = .array(v) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .object(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .string(let v): try c.encode(v)
        case .number(let v): try c.encode(v)
        case .bool(let v): try c.encode(v)
        case .null: try c.encodeNil()
        }
    }
    var object: [String: JSONValue]? { if case .object(let v) = self { v } else { nil } }
    var string: String? { if case .string(let v) = self { v } else { nil } }
    var number: Double? { if case .number(let v) = self { v } else { nil } }
    subscript(_ key: String) -> JSONValue? { object?[key] }
    func data(pretty: Bool = false) throws -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = pretty ? [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes] : [.sortedKeys]
        return try encoder.encode(self)
    }
}

struct ScreenDraft: Identifiable, Equatable, Sendable {
    /// Presentation identity must remain stable even while a user edits duplicate IDs.
    var id = UUID()
    var screenID: String
    var deviceID: String
    var x: Double
    var y: Double
    var width: Double
    var height: Double
    var extra: [String: JSONValue] = [:]

    var json: JSONValue {
        var fields = extra
        fields.merge([
            "id": .string(screenID), "device_id": .string(deviceID),
            "x": .number(x), "y": .number(y), "width": .number(width), "height": .number(height)
        ]) { _, new in new }
        return .object(fields)
    }
}

struct LayoutDocument: Equatable, Sendable {
    var screens: [ScreenDraft]
    var extra: [String: JSONValue] = [:]
    var json: JSONValue {
        var fields = extra
        fields["screens"] = .array(screens.map(\.json))
        return .object(fields)
    }

    init(screens: [ScreenDraft]) { self.screens = screens }

    init(data: Data) throws {
        let value = try JSONDecoder().decode(JSONValue.self, from: data)
        guard var root = value.object, case .array(let items) = root["screens"] else {
            throw DocumentError.shape
        }
        screens = try items.map { item in
            guard var fields = item.object,
                  let id = fields["id"]?.string, let device = fields["device_id"]?.string,
                  let x = fields["x"]?.number, let y = fields["y"]?.number,
                  let width = fields["width"]?.number, let height = fields["height"]?.number else {
                throw DocumentError.shape
            }
            for key in ["id", "device_id", "x", "y", "width", "height"] { fields.removeValue(forKey: key) }
            return ScreenDraft(screenID: id, deviceID: device, x: x, y: y, width: width, height: height, extra: fields)
        }
        root.removeValue(forKey: "screens")
        extra = root // Preserve unknown fields so Rust can reject them; never silently strip them.
    }

    static var example: Self {
        Self(screens: [
            ScreenDraft(screenID: "windows-main", deviceID: "windows", x: 0, y: 0, width: 1920, height: 1080),
            ScreenDraft(screenID: "mac-main", deviceID: "mac", x: 1920, y: 100, width: 1470, height: 956)
        ])
    }
}

enum DocumentError: LocalizedError {
    case shape, tooLarge
    var errorDescription: String? {
        switch self {
        case .shape: "无法读取布局结构。需要 screens 数组，以及每个屏幕的 ID、设备 ID 和数值坐标、尺寸。"
        case .tooLarge: "文件超过 1 MiB，请选择较小的布局 JSON。"
        }
    }
}
