import SwiftUI

struct DeviceView: View {
    @Bindable var model: DeviceModel
    let discovery: DiscoveryModel

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                HStack {
                    VStack(alignment: .leading, spacing: 7) {
                        Text("设备").font(.system(size: Theme.titleSize, weight: .semibold))
                        Text("让电脑自然地连接在一起。 ").foregroundStyle(Theme.secondary)
                    }
                    Spacer()
                    StatusBadge(title: model.isSharing ? "共享中" : "本机接收", active: model.isSharing)
                }
                HStack(spacing: 10) {
                    Image(systemName: model.cleanupUnconfirmed ? "exclamationmark.circle" : "circle.inset.filled")
                        .foregroundStyle(model.cleanupUnconfirmed ? .orange : Theme.accent)
                    Text(model.message).fixedSize(horizontal: false, vertical: true)
                    Spacer()
                    if model.operation != nil || model.stopPending {
                        Button(model.stopping ? "停止中…" : "停止", role: .cancel) { model.stop() }
                            .disabled(model.stopping || model.stopPending)
                            .accessibilityIdentifier("device-stop")
                    }
                }
                .padding(16).background(Theme.subtle, in: RoundedRectangle(cornerRadius: 12))

                pairedCard
                invitationCard
                nearbyCard
                Text("本机按 Escape 可紧急停止。关闭窗口也会停止接收。首次接收需要在系统设置中允许辅助功能与输入监控。")
                    .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                if model.cleanupUnconfirmed {
                    Text("停止结果未确认，暂不能再次开启。请检查本机输入并重新打开 Crossflow。")
                        .font(.system(size: Theme.captionSize)).foregroundStyle(.orange)
                }
            }
            .padding(28)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.background)
        .task { model.refresh(); discovery.start() }
        .onDisappear { discovery.cancel() }
    }

    private var pairedCard: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Label("已配对电脑", systemImage: "checkmark.shield").font(.headline)
                Spacer()
                if model.loading { ProgressView().controlSize(.small) }
                Button("刷新") { model.refresh() }.buttonStyle(.borderless).disabled(model.loading)
            }
            if model.devices.isEmpty {
                Text("添加电脑后，会自动记住连接关系。")
                    .foregroundStyle(Theme.secondary).padding(.vertical, 8)
            }
            ForEach(model.devices) { device in
                Button { model.selectedID = device.id } label: {
                    HStack(spacing: 12) {
                        Image(systemName: "desktopcomputer").font(.system(size: 24)).foregroundStyle(Theme.accent)
                        Text(device.name).fontWeight(.medium)
                        Spacer()
                        Image(systemName: model.selectedID == device.id ? "checkmark.circle.fill" : "circle")
                            .foregroundStyle(model.selectedID == device.id ? Theme.accent : Theme.secondary)
                    }
                    .padding(14).contentShape(Rectangle())
                    .background(model.selectedID == device.id ? Theme.subtle : Theme.background,
                                in: RoundedRectangle(cornerRadius: 10))
                }
                .buttonStyle(.plain).disabled(!model.canStart)
                .accessibilityLabel("选择 \(device.name)")
            }
            if !model.listMessage.isEmpty { Text(model.listMessage).font(.caption).foregroundStyle(Theme.secondary) }
            Divider()
            HStack {
                Toggle("同时接收键盘、点击与滚轮（实验）", isOn: $model.includeKeyboard)
                    .toggleStyle(.checkbox).disabled(!model.canStart)
                Spacer()
                Button(model.includeKeyboard ? "接收键鼠" : "接收鼠标移动", systemImage: "arrow.down.forward") { model.receive() }
                    .buttonStyle(.borderedProminent).disabled(!model.canStart || model.selected == nil)
                    .accessibilityIdentifier("device-receive")
            }
        }
        .padding(20).background(Theme.surface, in: RoundedRectangle(cornerRadius: 14))
        .overlay(RoundedRectangle(cornerRadius: 14).stroke(Theme.border, lineWidth: 1))
    }

    private var invitationCard: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Label("添加电脑", systemImage: "plus.circle").font(.headline)
                Spacer()
                if model.operation != .invitation {
                    Button("生成配对码") { model.invite() }
                        .buttonStyle(.bordered).disabled(!model.canStart || !model.invitationAvailable)
                        .accessibilityIdentifier("device-invite")
                }
            }
            TextField("这台 Mac 的显示名称", text: $model.deviceName)
                .textFieldStyle(.roundedBorder).frame(maxWidth: 280).disabled(!model.canStart)
            if !model.invitationAvailable {
                Text("当前安装缺少配对组件，请安装完整客户端。").foregroundStyle(Theme.secondary)
            } else if model.code.isEmpty {
                Text("生成配对码后，在 Windows 的附近设备中选择这台 Mac。")
                    .foregroundStyle(Theme.secondary)
            } else {
                Text(model.code).font(.system(size: 34, weight: .medium, design: .monospaced))
                    .tracking(6).foregroundStyle(Theme.accent).privacySensitive()
                    .accessibilityLabel("配对码 \(model.code)")
                Text("短期有效 · 双方确认后建立连接")
                    .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
            }
            if let confirmation = model.confirmation {
                Divider()
                Label(confirmation.name, systemImage: "desktopcomputer").fontWeight(.semibold)
                Text("请确认这是你正在配对的电脑。")
                Text("设备标识 \(confirmation.fingerprint.suffix(12))")
                    .font(.system(size: Theme.captionSize, design: .monospaced)).foregroundStyle(Theme.secondary)
                HStack {
                    Button(model.approved ? "等待对方确认…" : "确认配对") { model.approve() }
                        .buttonStyle(.borderedProminent).disabled(model.approved || model.stopping)
                        .accessibilityIdentifier("device-approve")
                    Button("取消") { model.stop() }.disabled(model.stopping)
                }
            }
        }
        .padding(20).background(Theme.surface, in: RoundedRectangle(cornerRadius: 14))
        .overlay(RoundedRectangle(cornerRadius: 14).stroke(Theme.border, lineWidth: 1))
    }

    private var nearbyCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Label("附近设备", systemImage: "network").font(.headline)
                Spacer()
                Button("重新查找") { discovery.start() }.buttonStyle(.borderless).disabled(discovery.scanning)
            }
            Text(discovery.message).font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
            ForEach(discovery.devices) { device in
                HStack {
                    Image(systemName: "display").foregroundStyle(Theme.accent)
                    Text(device.name)
                    Spacer()
                    Text(device.compatible ? "局域网可见" : "版本不兼容")
                        .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                }.padding(.vertical, 5)
            }
            Text("发现设备不会自动授权。首次配对请使用上方配对码。")
                .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
        }
        .padding(20).background(Theme.surface, in: RoundedRectangle(cornerRadius: 14))
        .overlay(RoundedRectangle(cornerRadius: 14).stroke(Theme.border, lineWidth: 1))
    }
}
