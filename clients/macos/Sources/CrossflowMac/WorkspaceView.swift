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
                        SessionStatusCard(model: devices)
                        if let layout = model.sessionLayout {
                            SessionLayoutView(layout: layout, focusScreenID: model.focusScreenID,
                                              routeEnabled: model.routeEnabled,
                                              sourceName: devices.selected?.name ?? "另一台电脑", localDeviceID: devices.localDeviceID)
                        } else {
                            sessionLayout
                        }
                    }
                    .padding(24).frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
            Divider().overlay(Theme.border)
            HStack {
                Text(devices.sessionTitle)
                    .lineLimit(1)
                Spacer()
                Text("本机按 Escape 可紧急停止接收").lineLimit(1)
            }
            .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
            .padding(.horizontal, 18).padding(.vertical, 10)
        }
        .background(Theme.background).foregroundStyle(Theme.text)
        .tint(Theme.accent).font(.system(size: Theme.bodySize))
        .task { devices.refresh(); model.receiveSession(devices.receiverStatus); await model.refreshBridge() }
        .onChange(of: devices.receiverStatus) { _, status in
            model.receiveSession(devices.stopping || devices.cleanupUnconfirmed ? nil : status)
        }
        .onChange(of: devices.stopping) { _, stopping in if stopping { model.receiveSession(nil) } }
        .onChange(of: devices.cleanupUnconfirmed) { _, unconfirmed in if unconfirmed { model.receiveSession(nil) } }
        .sheet(isPresented: $showDetails) {
            VStack(alignment: .leading, spacing: 16) {
                Text("内核信息").font(.title2.bold())
                Text(model.bridgeMessage).foregroundStyle(Theme.secondary)
                ScrollView { Text(model.environmentJSON.isEmpty ? "暂无环境信息。" : model.environmentJSON)
                    .font(.system(.caption, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading) }
                HStack {
                    Button("重新检查") { Task { await model.refreshBridge() } }
                    Spacer()
                    Button("完成") { showDetails = false }
                }
            }.padding(24).frame(width: 540, height: 380)
        }
    }

    private var topBar: some View {
        HStack(spacing: 12) {
            Image(systemName: "rectangle.2.swap").font(.system(size: 20, weight: .semibold)).foregroundStyle(Theme.accent)
            Text("Crossflow").font(.system(size: 18, weight: .semibold))
            Spacer()
            StatusBadge(title: devices.sessionTitle, active: devices.isSharing)
            Button { showDetails = true } label: { Image(systemName: "info.circle") }
                .buttonStyle(.plain).accessibilityLabel("内核信息")
        }
        .padding(.horizontal, 20).padding(.vertical, 14).background(Theme.surface)
    }

    private var heading: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text("屏幕排列").font(.system(size: Theme.titleSize, weight: .semibold))
            Text("查看当前会话的屏幕排列与键鼠焦点。")
                .foregroundStyle(Theme.secondary)
        }
    }

    private var sessionLayout: some View {
        VStack(spacing: 14) {
            Image(systemName: "rectangle.3.group").font(.system(size: 36, weight: .light)).foregroundStyle(Theme.secondary)
            Text("尚未收到屏幕排列").font(.headline)
            Text("在已配对的 Windows 电脑上排列屏幕并连接。这里仅显示会话确认的屏幕信息。")
                .foregroundStyle(Theme.secondary).multilineTextAlignment(.center).frame(maxWidth: 400)
            Button("管理设备") { showingDevices = true }.buttonStyle(.bordered)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity).padding(24)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).stroke(Theme.border, lineWidth: 1))
        .accessibilityIdentifier("session-layout-empty")
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 22) {
            VStack(spacing: 6) {
                Button { showingDevices = false } label: {
                    Label("工作台", systemImage: "square.grid.2x2.fill")
                        .frame(maxWidth: .infinity, alignment: .leading).padding(12)
                        .background(!showingDevices ? Theme.subtle : Color.clear, in: RoundedRectangle(cornerRadius: 10))
                }.accessibilityIdentifier("nav-workspace")
                Button { showingDevices = true } label: {
                    Label("设备", systemImage: "desktopcomputer")
                        .frame(maxWidth: .infinity, alignment: .leading).padding(12)
                        .background(showingDevices ? Theme.subtle : Color.clear, in: RoundedRectangle(cornerRadius: 10))
                }.accessibilityIdentifier("nav-devices")
            }
            .buttonStyle(.plain).foregroundStyle(Theme.accent)
            .font(.system(size: Theme.bodySize, weight: .semibold))
            Text("设备与屏幕").font(.system(size: Theme.captionSize, weight: .semibold)).foregroundStyle(Theme.secondary)
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    if devices.devices.isEmpty {
                        Text("添加设备后，在此查看已保存的电脑。")
                            .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                    }
                    ForEach(devices.devices) { device in
                        Button { devices.selectedID = device.id; showingDevices = true } label: {
                            HStack(alignment: .top, spacing: 9) {
                                Image(systemName: "desktopcomputer").foregroundStyle(Theme.accent)
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(device.name).lineLimit(2)
                                    Text("已配对").font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                                }
                            }.frame(maxWidth: .infinity, alignment: .leading)
                        }.buttonStyle(.plain).disabled(!devices.canStart)
                    }
                }
            }
            Spacer(minLength: 0)
            Text(devices.sessionTitle).fontWeight(.medium)
            Text("连接并开启共享后，两台电脑的键鼠均可接管。")
                .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(16).frame(maxHeight: .infinity).background(Theme.background)
    }
}
