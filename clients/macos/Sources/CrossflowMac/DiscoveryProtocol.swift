import Darwin
import Foundation

struct NearbyDevice: Identifiable, Sendable {
    let id: String
    let name: String
    let platform: String
    let protocolVersion: UInt16
    let compatible: Bool
    let addresses: [String] // Diagnostics only; never an identity or authorization.
}

struct DiscoveryResult: Sendable {
    let devices: [NearbyDevice]
    let capacityLimited: Bool
}

enum DiscoveryError: LocalizedError, Sendable {
    case unavailable, invalidResult, notReady, failed, timeout, cleanupPending
    var errorDescription: String? {
        switch self {
        case .unavailable: "缺少发现内核，请重新构建或安装客户端。"
        case .invalidResult: "发现结果无效，请更新客户端后重试。"
        case .notReady: "发现服务尚未就绪，请检查网络后刷新。"
        case .failed: "扫描失败，请检查本地网络权限和网络连接。"
        case .timeout: "扫描超时，请稍后刷新。"
        case .cleanupPending: "上一次扫描尚未退出，暂不能开始新扫描。请重试停止或关闭。"
        }
    }
}

enum DiscoveryProtocol {
    static let maxOutputBytes = 262_144

    static func parse(_ data: Data) throws -> DiscoveryResult {
        guard data.count <= maxOutputBytes, String(data: data, encoding: .utf8) != nil else {
            throw DiscoveryError.invalidResult
        }
        do {
            // JSONDecoder's keyed containers alone cannot detect duplicate keys.
            var syntax = UniqueJSON(data: data)
            try syntax.check()
            let root = try JSONDecoder().decode(Snapshot.self, from: data)
            guard root.state == "browsing", root.error == nil else { throw DiscoveryError.notReady }
            guard root.candidates.count <= 64 else { throw DiscoveryError.invalidResult }
            var ids = Set<String>()
            let devices = try root.candidates.map { candidate -> NearbyDevice in
                guard candidate.id.utf8.count == 32,
                      candidate.id.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }),
                      ids.insert(candidate.id).inserted,
                      !candidate.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                      candidate.name.utf8.count <= 96,
                      !candidate.name.unicodeScalars.contains(where: forbiddenNameScalar),
                      ["windows", "macos", "linux", "ios", "ipados"].contains(candidate.platform),
                      candidate.protocolVersion > 0, !candidate.trusted,
                      (1...32).contains(candidate.addresses.count) else { throw DiscoveryError.invalidResult }
                var addresses = Set<String>()
                for address in candidate.addresses {
                    guard address.utf8.count <= 96,
                          addresses.insert(try endpointKey(address)).inserted else { throw DiscoveryError.invalidResult }
                }
                return NearbyDevice(id: candidate.id, name: candidate.name, platform: candidate.platform,
                                    protocolVersion: candidate.protocolVersion, compatible: candidate.compatible,
                                    addresses: candidate.addresses)
            }
            return DiscoveryResult(devices: devices, capacityLimited: root.capacityLimited)
        } catch let error as DiscoveryError { throw error }
        catch { throw DiscoveryError.invalidResult }
    }

    private static func forbiddenNameScalar(_ scalar: Unicode.Scalar) -> Bool {
        CharacterSet.controlCharacters.contains(scalar) || [0x061c, 0x200e, 0x200f].contains(scalar.value)
            || (0x202a...0x202e).contains(scalar.value) || (0x2066...0x2069).contains(scalar.value)
    }

    private static func endpointKey(_ endpoint: String) throws -> String {
        let host: String
        let portText: String
        let family: Int32
        if endpoint.hasPrefix("["), let end = endpoint.firstIndex(of: "]") {
            host = String(endpoint[endpoint.index(after: endpoint.startIndex)..<end])
            let tail = endpoint[endpoint.index(after: end)...]
            guard tail.hasPrefix(":") else { throw DiscoveryError.invalidResult }
            portText = String(tail.dropFirst()); family = AF_INET6
        } else {
            let pieces = endpoint.split(separator: ":", omittingEmptySubsequences: false)
            guard pieces.count == 2 else { throw DiscoveryError.invalidResult }
            host = String(pieces[0]); portText = String(pieces[1]); family = AF_INET
        }
        guard !portText.isEmpty, portText.utf8.allSatisfy({ (48...57).contains($0) }),
              let port = UInt16(portText), port > 0 else { throw DiscoveryError.invalidResult }
        let parts = host.split(separator: "%", omittingEmptySubsequences: false)
        guard (1...2).contains(parts.count), family == AF_INET6 || parts.count == 1 else { throw DiscoveryError.invalidResult }
        var scope: UInt32 = 0
        if parts.count == 2 {
            guard !parts[1].isEmpty, parts[1].utf8.allSatisfy({ (48...57).contains($0) }),
                  let parsed = UInt32(parts[1]), parsed > 0 else { throw DiscoveryError.invalidResult }
            scope = parsed
        }
        var bytes = [UInt8](repeating: 0, count: family == AF_INET ? 4 : 16)
        let valid = String(parts[0]).withCString { text in
            bytes.withUnsafeMutableBytes { storage in inet_pton(family, text, storage.baseAddress) }
        }
        guard valid == 1 else { throw DiscoveryError.invalidResult }
        if family == AF_INET {
            guard bytes[0] != 0, bytes[0] != 127, bytes[0] < 224 else { throw DiscoveryError.invalidResult }
        } else {
            guard !bytes.allSatisfy({ $0 == 0 }),
                  !(bytes.dropLast().allSatisfy({ $0 == 0 }) && bytes.last == 1),
                  !(bytes.prefix(10).allSatisfy({ $0 == 0 }) && bytes[10] == 255 && bytes[11] == 255),
                  bytes[0] != 255,
                  !(bytes[0] == 254 && bytes[1] & 0xc0 == 0x80 && scope == 0) else { throw DiscoveryError.invalidResult }
        }
        return bytes.map { String($0) }.joined(separator: ".") + "%\(scope):\(port)"
    }

    private struct Key: CodingKey {
        let stringValue: String
        var intValue: Int? { nil }
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { return nil }
    }
    private static func fields(_ decoder: Decoder, _ expected: Set<String>) throws {
        let keys = try decoder.container(keyedBy: Key.self).allKeys.map(\.stringValue)
        guard Set(keys) == expected else { throw DiscoveryError.invalidResult }
    }
    private struct Snapshot: Decodable {
        let state: String
        let candidates: [Candidate]
        let capacityLimited: Bool
        let error: String?
        enum CodingKeys: String, CodingKey { case state, candidates, capacityLimited = "capacity_limited", error }
        init(from decoder: Decoder) throws {
            try DiscoveryProtocol.fields(decoder, ["state", "candidates", "capacity_limited", "error"])
            let c = try decoder.container(keyedBy: CodingKeys.self)
            state = try c.decode(String.self, forKey: .state)
            candidates = try c.decode([Candidate].self, forKey: .candidates)
            capacityLimited = try c.decode(Bool.self, forKey: .capacityLimited)
            error = try c.decodeIfPresent(String.self, forKey: .error)
            guard state.utf8.count <= 16, (error?.utf8.count ?? 0) <= 512 else { throw DiscoveryError.invalidResult }
        }
    }
    private struct Candidate: Decodable {
        let id: String, name: String, platform: String
        let protocolVersion: UInt16
        let compatible: Bool, trusted: Bool
        let addresses: [String]
        enum CodingKeys: String, CodingKey {
            case id = "device_id", name = "friendly_name", platform, protocolVersion = "protocol", compatible, trusted, addresses
        }
        init(from decoder: Decoder) throws {
            try DiscoveryProtocol.fields(decoder, ["device_id", "friendly_name", "platform", "protocol", "compatible", "trusted", "addresses"])
            let c = try decoder.container(keyedBy: CodingKeys.self)
            id = try c.decode(String.self, forKey: .id); name = try c.decode(String.self, forKey: .name)
            platform = try c.decode(String.self, forKey: .platform)
            protocolVersion = try c.decode(UInt16.self, forKey: .protocolVersion)
            compatible = try c.decode(Bool.self, forKey: .compatible); trusted = try c.decode(Bool.self, forKey: .trusted)
            addresses = try c.decode([String].self, forKey: .addresses)
        }
    }
}

/// A bounded syntax walk checks duplicate (including escaped) keys and depth;
/// JSONDecoder still validates all primitive grammar, UTF-8 and field types.
private struct UniqueJSON {
    let bytes: [UInt8]
    var index = 0
    init(data: Data) { bytes = Array(data) }
    mutating func check() throws {
        try value(depth: 0); whitespace()
        guard index == bytes.count else { throw DiscoveryError.invalidResult }
    }
    mutating func whitespace() { while index < bytes.count && [9, 10, 13, 32].contains(bytes[index]) { index += 1 } }
    mutating func consume(_ byte: UInt8) throws {
        whitespace()
        guard index < bytes.count, bytes[index] == byte else { throw DiscoveryError.invalidResult }
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
        throw DiscoveryError.invalidResult
    }
    mutating func value(depth: Int) throws {
        whitespace()
        guard depth < 8, index < bytes.count else { throw DiscoveryError.invalidResult }
        switch bytes[index] {
        case 123, 91:
            let object = bytes[index] == 123
            let end: UInt8 = object ? 125 : 93
            index += 1; whitespace()
            if index < bytes.count && bytes[index] == end { index += 1; return }
            var keys = Set<String>()
            while true {
                if object {
                    guard keys.insert(try string()).inserted else { throw DiscoveryError.invalidResult }
                    try consume(58)
                }
                try value(depth: depth + 1); whitespace()
                guard index < bytes.count else { throw DiscoveryError.invalidResult }
                if bytes[index] == end { index += 1; return }
                try consume(44)
            }
        case 34: _ = try string()
        default:
            let start = index
            while index < bytes.count && ![9, 10, 13, 32, 44, 93, 125].contains(bytes[index]) { index += 1 }
            guard index > start else { throw DiscoveryError.invalidResult }
            let token = bytes[start..<index]
            let literal = String(decoding: token, as: UTF8.self)
            if !["true", "false", "null"].contains(literal) {
                // The only numeric field is a positive UInt16 protocol version.
                // Match the Windows integer boundary; do not coerce 2.0 or 2e0.
                guard token.allSatisfy({ (48...57).contains($0) }),
                      token.count == 1 || token.first != 48 else { throw DiscoveryError.invalidResult }
            }
        }
    }
}
