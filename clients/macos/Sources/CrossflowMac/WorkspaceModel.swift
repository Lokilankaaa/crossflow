import AppKit
import Observation
import UniformTypeIdentifiers

@MainActor @Observable
final class WorkspaceModel {
    var document = LayoutDocument(screens: [])
    var selectedID: UUID?
    var isExample = true
    var documentName = "多屏工作台"
    var hasChanges = false
    var fitGeneration = 0
    var banner = "拖动屏幕，安排你的工作空间。"
    var alertMessage: String?
    var bridgeAvailable = false
    var bridgeMessage = "正在检查 Rust 桥接…"
    var validationMessage = "尚未校验"
    var validationPassed = false
    var validating = false
    var environmentJSON = ""
    @ObservationIgnored let bridge: any BridgeRequesting
    private var undoStack: [LayoutDocument] = []
    private var redoStack: [LayoutDocument] = []
    @ObservationIgnored private var revision = 0

    init(bridge: any BridgeRequesting = BridgeClient(), initialDocument: LayoutDocument? = nil) {
        self.bridge = bridge
        do { document = try initialDocument ?? .bundledExample(); selectedID = document.screens.first?.id }
        catch { banner = error.localizedDescription }
    }
    var selectedScreen: ScreenDraft? { document.screens.first { $0.id == selectedID } }
    var canUndo: Bool { !undoStack.isEmpty }
    var canRedo: Bool { !redoStack.isEmpty }

    func update(_ screen: ScreenDraft) {
        guard let i = document.screens.firstIndex(where: { $0.id == screen.id }), document.screens[i] != screen else { return }
        checkpoint()
        document.screens[i] = screen
        changed()
    }
    func addScreen() {
        checkpoint()
        let suffix = UUID().uuidString.prefix(6).lowercased()
        let screen = ScreenDraft(screenID: "screen-\(suffix)", deviceID: "device-\(suffix)", x: 0, y: 1200, width: 1440, height: 900)
        document.screens.append(screen)
        selectedID = screen.id
        changed()
        fitGeneration += 1
    }
    func removeSelected() {
        guard let selectedID else { return }
        checkpoint()
        document.screens.removeAll { $0.id == selectedID }
        self.selectedID = document.screens.first?.id
        changed()
    }
    private func checkpoint() {
        undoStack.append(document)
        if undoStack.count > 100 { undoStack.removeFirst() }
        redoStack.removeAll()
    }
    private func changed() {
        revision += 1
        hasChanges = true
        validationPassed = false
        validationMessage = "草稿有更改 · 尚未校验"
    }
    func undo() {
        guard let previous = undoStack.popLast() else { return }
        redoStack.append(document); document = previous; changed(); reconcileSelection()
    }
    func redo() {
        guard let next = redoStack.popLast() else { return }
        undoStack.append(document); document = next; changed(); reconcileSelection()
    }
    private func reconcileSelection() {
        if selectedScreen == nil { selectedID = document.screens.first?.id }
    }
    func loadExample() {
        do {
            let example = try LayoutDocument.bundledExample()
            checkpoint(); document = example; selectedID = document.screens.first?.id
            isExample = true; documentName = "多屏工作台"; changed(); fitGeneration += 1
            banner = "已载入示例布局 · 设备信息不是在线发现结果。"
        } catch { alertMessage = error.localizedDescription }
    }
    func importDocument() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.json]; panel.allowsMultipleSelection = false
        panel.message = "导入布局草稿，不会连接设备或启动共享。"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0
            guard size <= 1_048_576 else { throw DocumentError.tooLarge }
            let data = try Data(contentsOf: url)
            guard data.count <= 1_048_576 else { throw DocumentError.tooLarge }
            let imported = try LayoutDocument(data: data)
            checkpoint(); document = imported; selectedID = imported.screens.first?.id
            isExample = false; documentName = url.deletingPathExtension().lastPathComponent
            changed(); hasChanges = false; fitGeneration += 1
            banner = "已导入布局草稿。设备连接状态尚未接入。"
        } catch { alertMessage = error.localizedDescription }
    }
    func exportDocument() async {
        let snapshot = document
        let snapshotRevision = revision
        do {
            // Always ask Rust about this exact export snapshot; never reuse an older valid badge.
            var snapshotPassed = false
            if bridgeAvailable {
                let response = try await bridge.request(.object(["op": .string("layout_validate"), "layout": snapshot.json]))
                guard snapshotRevision == revision else {
                    banner = "布局已更改，请重新导出当前草稿。"; return
                }
                snapshotPassed = response["ok"] == .bool(true)
                validationPassed = snapshotPassed
                validationMessage = snapshotPassed ? "Rust 校验通过" : "Rust 未通过：\(response["error"]?["message"]?.string ?? "布局无效")"
            }
            let data = try snapshot.json.data(pretty: true)
            let panel = NSSavePanel(); panel.allowedContentTypes = [.json]
            panel.nameFieldStringValue = "crossflow-layout.json"
            panel.message = snapshotPassed ? "导出已通过 Rust 校验的布局。" : "导出未通过校验或未校验的草稿；不会应用到设备。"
            guard panel.runModal() == .OK, let url = panel.url else { return }
            try data.write(to: url, options: .atomic)
            if snapshotRevision == revision { hasChanges = false }
            banner = snapshotPassed ? "布局已导出。" : "草稿已导出，尚不可应用。"
        } catch { alertMessage = error.localizedDescription }
    }
    func refreshBridge() async {
        do {
            let response = try await bridge.request(.object(["op": .string("environment")]))
            guard response["ok"] == .bool(true) else { throw BridgeError.protocolMismatch }
            bridgeAvailable = true
            bridgeMessage = "Rust 桥接已加载 · 会话状态尚未接入"
            environmentJSON = String(data: try response.data(pretty: true), encoding: .utf8) ?? ""
        } catch {
            bridgeAvailable = false
            bridgeMessage = error.localizedDescription
        }
    }
    func validate() async {
        guard !validating else { return }
        validating = true
        let checkedRevision = revision
        let layout = document.json
        defer { validating = false }
        do {
            let response = try await bridge.request(.object(["op": .string("layout_validate"), "layout": layout]))
            guard checkedRevision == revision else { return }
            validationPassed = response["ok"] == .bool(true)
            validationMessage = validationPassed ? "Rust 校验通过" : "Rust 未通过：\(response["error"]?["message"]?.string ?? "布局无效")"
        } catch {
            guard checkedRevision == revision else { return }
            validationPassed = false; validationMessage = error.localizedDescription
        }
    }
}
