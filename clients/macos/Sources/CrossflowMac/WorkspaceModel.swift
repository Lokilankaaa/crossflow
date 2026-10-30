import Foundation
import Observation

@MainActor @Observable
final class WorkspaceModel {
    private(set) var sessionLayout: SharedLayout?
    private(set) var focusScreenID: String?
    private(set) var focusDeviceID: String?
    private(set) var routeEnabled = false
    private(set) var inputActive = false
    var bridgeMessage = "正在检查内核…"
    var environmentJSON = ""
    @ObservationIgnored let bridge: any BridgeRequesting

    init(bridge: any BridgeRequesting = BridgeClient()) {
        self.bridge = bridge
    }

    func receiveSession(_ status: ReceiverStatus?) {
        guard let status, [.paused, .sharing].contains(status.state) else {
            sessionLayout = nil; focusScreenID = nil; focusDeviceID = nil
            routeEnabled = false; inputActive = false
            return
        }
        // Focus updates do not replace geometry. Identifiable screens retain
        // authenticated IDs across repeated 50 ms status snapshots.
        if sessionLayout != status.sharedLayout { sessionLayout = status.sharedLayout }
        routeEnabled = status.routeEnabled
        inputActive = status.inputActive
        focusScreenID = status.routeEnabled ? status.focusScreenID : nil
        focusDeviceID = status.routeEnabled ? status.focusDeviceID : nil
    }

    func refreshBridge() async {
        do {
            let response = try await bridge.request(.object(["op": .string("environment")]))
            guard response["ok"] == .bool(true) else { throw BridgeError.protocolMismatch }
            bridgeMessage = "布局内核已就绪"
            environmentJSON = String(data: try response.data(pretty: true), encoding: .utf8) ?? ""
        } catch {
            bridgeMessage = error.localizedDescription
        }
    }
}
