import SwiftUI

struct WorkspaceView: View {
    @Bindable var model: WorkspaceModel
    let discovery: DiscoveryModel
    let devices: DeviceModel
    @ViewState private var showDetails = false
    @ViewState private var showingDevices = false

    var body: some View {
        VStack(spacing: 0) {
            topBar
            Divider().overlay(Theme.border)
            HStack(spacing: 0) {
                sidebar.frame(width: Theme.sidebarWidth)
                Divider().overlay(Theme.border)
                if showingDevices {
                    DeviceView(model: devices, discovery: discovery)
                } else {
                    VStack(alignment: .leading, spacing: 18) {
                        heading
                        ArrangementCanvas(model: model)
                        bridgeCard
                    }
                    .padding(24)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    Divider().overlay(Theme.border)
                    InspectorView(model: model).frame(width: Theme.inspectorWidth)
                }
            }
            Divider().overlay(Theme.border)
            HStack {
                Image(systemName: "doc.text").foregroundStyle(Theme.secondary)
                Text(model.banner).lineLimit(1)
                Spacer()
                Text(devices.operation != nil ? devices.message : "布局草稿 · 在设备页开始共享")
                    .lineLimit(1)
            }
            .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
            .padding(.horizontal, 18).padding(.vertical, 10)
        }
        .background(Theme.background).foregroundStyle(Theme.text)
        .tint(Theme.accent)
        .font(.system(size: Theme.bodySize))
        .task { await model.refreshBridge() }
        .alert("无法完成操作", isPresented: Binding(get: { model.alertMessage != nil }, set: { if !$0 { model.alertMessage = nil } })) {
            Button("好", role: .cancel) { model.alertMessage = nil }
        } message: { Text(model.alertMessage ?? "") }
        .sheet(isPresented: $showDetails) {
            VStack(alignment: .leading, spacing: 16) {
                Text("内核信息").font(.title2.bold())
                Text(model.bridgeMessage).foregroundStyle(Theme.secondary)
                ScrollView { Text(model.environmentJSON.isEmpty ? "暂无环境信息。" : model.environmentJSON)
                    .font(.system(.caption, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading) }
                Button("完成") { showDetails = false }.frame(maxWidth: .infinity, alignment: .trailing)
            }.padding(24).frame(width: 540, height: 380)
        }
    }
    private var topBar: some View {
        HStack(spacing: 12) {
            Image(systemName: "rectangle.2.swap").font(.system(size: 20, weight: .semibold)).foregroundStyle(Theme.accent)
            Text("Crossflow").font(.system(size: 18, weight: .semibold))
            Rectangle().fill(Theme.border).frame(width: 1, height: 18).padding(.horizontal, 6)
            Text(Theme.copy("draft_label", fallback: "布局草稿")).foregroundStyle(Theme.secondary)
            if model.hasChanges { Circle().fill(Theme.accent).frame(width: 5, height: 5).help("尚未导出更改") }
            Spacer()
            Button { model.undo() } label: { Image(systemName: "arrow.uturn.backward") }
                .disabled(!model.canUndo).help("撤销 ⌘Z").accessibilityLabel("撤销")
            Button { model.redo() } label: { Image(systemName: "arrow.uturn.forward") }
                .disabled(!model.canRedo).help("重做 ⇧⌘Z").accessibilityLabel("重做")
            Button("导入…", systemImage: "square.and.arrow.down") { model.importDocument() }
                .accessibilityIdentifier("import-layout")
            Button("导出草稿…", systemImage: "square.and.arrow.up") { Task { await model.exportDocument() } }
                .buttonStyle(.borderedProminent).accessibilityIdentifier("export-layout")
        }
        .buttonStyle(.bordered).padding(.horizontal, 20).padding(.vertical, 14)
        .background(Theme.surface)
    }
    private var heading: some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack {
                Text(Theme.copy("layout_title", fallback: "屏幕排列"))
                    .font(.system(size: Theme.titleSize, weight: .semibold))
                Spacer()
                StatusBadge(title: model.isExample ? "示例布局" : "已导入草稿")
            }
            Text(Theme.copy("layout_subtitle", fallback: "拖动屏幕，匹配你的桌面布局。"))
                .foregroundStyle(Theme.secondary)
        }
    }
    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 22) {
            VStack(spacing: 6) {
                Button { showingDevices = false } label: {
                    Label("工作空间", systemImage: "square.grid.2x2.fill")
                        .frame(maxWidth: .infinity, alignment: .leading).padding(12)
                        .background(!showingDevices ? Theme.subtle : Color.clear, in: RoundedRectangle(cornerRadius: 10))
                }
                Button { showingDevices = true } label: {
                    Label("附近设备", systemImage: "desktopcomputer")
                        .frame(maxWidth: .infinity, alignment: .leading).padding(12)
                        .background(showingDevices ? Theme.subtle : Color.clear, in: RoundedRectangle(cornerRadius: 10))
                }.accessibilityIdentifier("nearby-devices")
            }
            .buttonStyle(.plain).foregroundStyle(Theme.accent)
            .font(.system(size: Theme.bodySize, weight: .semibold))
            VStack(alignment: .leading, spacing: 12) {
                Text("布局中的屏幕").font(.system(size: Theme.captionSize, weight: .semibold)).foregroundStyle(Theme.secondary)
                ScrollView {
                    LazyVStack(spacing: 7) {
                        ForEach(model.document.screens) { screen in
                            Button { model.selectedID = screen.id } label: {
                                HStack(spacing: 9) {
                                    Image(systemName: "display").font(.system(size: 17)).foregroundStyle(Theme.accent)
                                    VStack(alignment: .leading, spacing: 4) {
                                        Text(screen.screenID.isEmpty ? "未命名屏幕" : screen.screenID).fontWeight(.medium).lineLimit(1)
                                        Text(screen.deviceID.isEmpty ? "未命名设备" : screen.deviceID).font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                                    }
                                    Spacer(minLength: 0)
                                }
                                .padding(10).contentShape(Rectangle())
                                .background(model.selectedID == screen.id ? Theme.surface : Color.clear, in: RoundedRectangle(cornerRadius: 9))
                            }.buttonStyle(.plain).accessibilityLabel("选择屏幕 \(screen.screenID)")
                        }
                    }
                }
                Button("添加屏幕", systemImage: "plus") { model.addScreen() }
                    .buttonStyle(.borderless).foregroundStyle(Theme.accent)
            }
            Spacer(minLength: 0)
            VStack(alignment: .leading, spacing: 9) {
                StatusBadge(title: devices.isSharing ? "共享中" : "设备连接", active: devices.isSharing)
                Text(devices.message)
                    .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                Text(model.isExample ? Theme.copy("sample_notice", fallback: "示例屏幕，不代表已发现的设备。") : "布局中的设备尚未连接。")
                    .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Divider()
            Button("载入示例布局", systemImage: "square.on.square") { model.loadExample() }.buttonStyle(.borderless)
        }
        .padding(16).frame(maxHeight: .infinity).background(Theme.background)
    }
    private var bridgeCard: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: model.bridgeAvailable ? "checkmark.shield" : "puzzlepiece.extension")
                .font(.system(size: 22)).foregroundStyle(Theme.accent).padding(.top, 2)
            VStack(alignment: .leading, spacing: 5) {
                Text(model.bridgeAvailable ? Theme.copy("core_ready", fallback: "布局内核已就绪") : "布局校验暂不可用")
                    .fontWeight(.semibold)
                Text(model.bridgeMessage).font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 4)
            Button { showDetails = true } label: { Image(systemName: "info.circle") }
                .buttonStyle(.plain).help("查看内核信息").accessibilityLabel("查看内核信息")
            Button("重试") { Task { await model.refreshBridge() } }.buttonStyle(.borderless)
        }
        .padding(16).background(Theme.surface, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
    }
}
