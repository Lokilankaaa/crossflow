import Darwin
import Foundation

protocol BridgeRequesting: Sendable {
    func request(_ value: JSONValue) async throws -> JSONValue
}

/// The native UI owns bytes; Rust owns all layout validation and crossing logic.
/// This actor serializes C calls away from the main actor. No capture or receive API exists here.
actor BridgeClient: BridgeRequesting {
    typealias VersionFunction = @convention(c) () -> UInt32
    typealias RequestFunction = @convention(c) (
        UnsafePointer<UInt8>?, Int, UnsafeMutablePointer<UInt8>?, Int, UnsafeMutablePointer<Int>?
    ) -> Int32

    private var handle: UnsafeMutableRawPointer?
    private var requestFunction: RequestFunction?

    private func load() throws -> RequestFunction {
        if let requestFunction { return requestFunction }
        guard let folder = Bundle.main.privateFrameworksURL else { throw BridgeError.unavailable }
        let path = folder.appendingPathComponent("libcrossflow_bridge.dylib").path
        guard FileManager.default.fileExists(atPath: path),
              let library = dlopen(path, RTLD_NOW | RTLD_LOCAL) else { throw BridgeError.unavailable }
        guard let versionSymbol = dlsym(library, "crossflow_bridge_api_version"),
              let requestSymbol = dlsym(library, "crossflow_bridge_request_json") else {
            dlclose(library)
            throw BridgeError.protocolMismatch
        }
        let version = unsafeBitCast(versionSymbol, to: VersionFunction.self)
        guard version() == 1 else { dlclose(library); throw BridgeError.protocolMismatch }
        let request = unsafeBitCast(requestSymbol, to: RequestFunction.self)
        handle = library // Retain for the process lifetime; never unload code during an in-flight call.
        requestFunction = request
        return request
    }

    func request(_ value: JSONValue) throws -> JSONValue {
        let bytes = [UInt8](try value.data())
        guard bytes.count <= 1_048_576 else { throw BridgeError.requestTooLarge }
        let request = try load()
        var required = 0
        let probe = bytes.withUnsafeBufferPointer { input in
            request(input.baseAddress, input.count, nil, 0, &required)
        }
        guard probe == 1, required > 0, required <= 4_194_304 else { throw BridgeError.transport(probe) }
        var output = [UInt8](repeating: 0, count: required)
        let capacity = output.count
        let code = bytes.withUnsafeBufferPointer { input in
            output.withUnsafeMutableBufferPointer { destination in
                request(input.baseAddress, input.count, destination.baseAddress, capacity, &required)
            }
        }
        guard code == 0, required >= 0, required <= capacity else { throw BridgeError.transport(code) }
        let response = try JSONDecoder().decode(JSONValue.self, from: Data(output.prefix(required)))
        guard response["api_version"]?.number == 1 else { throw BridgeError.protocolMismatch }
        return response
    }
}

enum BridgeError: LocalizedError {
    case unavailable, protocolMismatch, requestTooLarge, transport(Int32)
    var errorDescription: String? {
        switch self {
        case .unavailable: "Rust 桥接库尚未加载。可以编辑和导出草稿，布局校验暂不可用。"
        case .protocolMismatch: "Rust 桥接接口版本不兼容，布局校验暂不可用。"
        case .requestTooLarge: "布局请求超过桥接接口的 1 MiB 限制。"
        case .transport(let code): "Rust 桥接调用失败（状态码 \(code)）。"
        }
    }
}
