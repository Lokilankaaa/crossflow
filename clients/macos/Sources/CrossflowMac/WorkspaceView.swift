import AppKit
import SwiftUI

struct WorkspaceView: View {
    @Bindable var model: WorkspaceModel
    let discovery: DiscoveryModel
    @Bindable var devices: DeviceModel
    @ViewState private var showDetails = false
    @ViewState private var showAddDevice = false

    var body: some View {
        HStack(spacing: 0) {
            sidebar.frame(width: 272)
            Divider().overlay(Theme.border)
            mainWorkspace
        }
        .background(Theme.background)
        .foregroundStyle(Theme.text)
        .tint(Theme.accent)
        .font(.system(size: Theme.bodySize))
        .task {
            devices.refresh()
            model.receiveSession(devices.receiverStatus)
            await model.refreshBridge()
        }
        .onChange(of: devices.devices) { _, _ in
            if !showAddDevice {
                devices.ensureListeningIfReady()
            }
        }
        .onChange(of: devices.receiverStatus) { _, status in
            model.receiveSession(devices.stopping || devices.cleanupUnconfirmed ? nil : status)
        }
        .onChange(of: devices.stopping) { _, stopping in
            if stopping { model.receiveSession(nil) }
        }
        .onChange(of: devices.cleanupUnconfirmed) { _, unconfirmed in
            if unconfirmed { model.receiveSession(nil) }
        }
        .sheet(isPresented: $showAddDevice, onDismiss: {
            if devices.operation == .invitation { devices.stop() }
            devices.ensureListeningIfReady()
        }) {
            AddDeviceSheet(devices: devices, isPresented: $showAddDevice)
        }
        .sheet(isPresented: $showDetails) {
            VStack(alignment: .leading, spacing: 16) {
                Text("内核信息").font(.title2.bold())
                Text(model.bridgeMessage).foregroundStyle(Theme.secondary)
                ScrollView {
                    Text(model.environmentJSON.isEmpty ? "暂无环境信息。" : model.environmentJSON)
                        .font(.system(.caption, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                HStack {
                    Button("重新检查") { Task { await model.refreshBridge() } }
                    Spacer()
                    Button("完成") { showDetails = false }
                }
            }
            .padding(24)
            .frame(width: 540, height: 380)
        }
    }

    private var statusColor: Color {
        if devices.cleanupUnconfirmed || devices.receiverState == .error || devices.receiverState == .startupRejected {
            return .red
        }
        if devices.isSharing { return Color(red: 22/255, green: 163/255, blue: 74/255) }
        if devices.operation == .receiver { return Theme.accent }
        return Theme.secondary
    }

    private var sidebar: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                // Brand
                HStack(spacing: 10) {
                    Image(systemName: "rectangle.2.swap")
                        .font(.system(size: 20, weight: .semibold))
                        .foregroundStyle(Theme.accent)
                    Text("Crossflow")
                        .font(.system(size: 20, weight: .semibold))
                    Spacer()
                    Button { showDetails = true } label: {
                        Image(systemName: "info.circle").foregroundStyle(Theme.secondary)
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("内核信息")
                }
                .padding(.bottom, 4)

                // Unified Status Card (matches Windows sidebar card)
                VStack(alignment: .leading, spacing: 10) {
                    HStack(spacing: 8) {
                        Circle().fill(statusColor).frame(width: 9, height: 9)
                        Text(devices.sessionTitle).font(.system(size: 14, weight: .semibold))
                    }
                    Text(devices.message)
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.secondary)
                        .fixedSize(horizontal: false, vertical: true)

                    if devices.cleanupUnconfirmed {
                        Button("重置并重新连接") { devices.resetErrorAndRetry() }
                            .buttonStyle(.borderedProminent)
                            .frame(maxWidth: .infinity)
                    } else if devices.operation == .receiver || devices.stopPending {
                        Button(devices.stopping ? "正在停止…" : "暂停共享") { devices.pauseListening() }
                            .buttonStyle(.bordered)
                            .frame(maxWidth: .infinity)
                            .disabled(devices.stopping || devices.stopPending)
                            .accessibilityIdentifier("session-stop")
                    } else if devices.selected != nil {
                        Button("开启共享") { devices.receive() }
                            .buttonStyle(.borderedProminent)
                            .frame(maxWidth: .infinity)
                            .disabled(!devices.canStart)
                            .accessibilityIdentifier("device-receive")
                    } else {
                        Button("添加设备") { openAddDevice() }
                            .buttonStyle(.borderedProminent)
                            .frame(maxWidth: .infinity)
                    }
                }
                .padding(14)
                .background(Theme.background, in: RoundedRectangle(cornerRadius: 12))
                .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))

                // Paired Devices Section (inline, no page jumps!)
                HStack {
                    Text("已配对设备")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.secondary)
                    Spacer()
                    if devices.loading { ProgressView().controlSize(.small) }
                    Button { devices.refresh() } label: {
                        Image(systemName: "arrow.clockwise").font(.system(size: 12))
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(Theme.secondary)
                    .disabled(devices.loading)
                }
                .padding(.top, 4)

                if devices.devices.isEmpty {
                    Text("暂无已配对设备，点击下方添加。")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.secondary)
                } else {
                    VStack(spacing: 6) {
                        ForEach(devices.devices) { device in
                            let isSelected = devices.selectedID == device.id
                            Button {
                                guard devices.selectedID != device.id else { return }
                                devices.selectedID = device.id
                                if devices.operation == .receiver {
                                    devices.stop()
                                }
                            } label: {
                                HStack(spacing: 10) {
                                    Image(systemName: "desktopcomputer")
                                        .foregroundStyle(isSelected ? Theme.accent : Theme.secondary)
                                    Text(device.name)
                                        .font(.system(size: 13, weight: isSelected ? .semibold : .regular))
                                        .lineLimit(1)
                                    Spacer()
                                    if isSelected {
                                        Image(systemName: "checkmark")
                                            .font(.system(size: 11, weight: .semibold))
                                            .foregroundStyle(Theme.accent)
                                    }
                                }
                                .padding(.horizontal, 10)
                                .padding(.vertical, 9)
                                .contentShape(Rectangle())
                                .background(isSelected ? Theme.subtle : Theme.background, in: RoundedRectangle(cornerRadius: 9))
                                .overlay(RoundedRectangle(cornerRadius: 9).stroke(isSelected ? Theme.accent : Theme.border, lineWidth: 1))
                            }
                            .buttonStyle(.plain)
                            .disabled(devices.operation == .receiver)
                        }
                    }
                }

                if !devices.listMessage.isEmpty {
                    Text(devices.listMessage)
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.secondary)
                }

                Button { openAddDevice() } label: {
                    Label("添加设备", systemImage: "plus")
                        .font(.system(size: 13))
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 8)
                }
                .buttonStyle(.bordered)
                .disabled(!devices.invitationAvailable || devices.stopping || devices.stopPending)
                .accessibilityIdentifier("device-invite")

                Divider().overlay(Theme.border).padding(.vertical, 4)

                Text("快捷键：本机按 Escape 随时紧急停止接收。")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(18)
        }
        .frame(maxHeight: .infinity)
        .background(Theme.surface)
    }

    private var mainWorkspace: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .center) {
                VStack(alignment: .leading, spacing: 4) {
                    Text("屏幕排列").font(.system(size: Theme.titleSize, weight: .semibold))
                    Text("自动同步已连接会话的屏幕排列与当前键鼠焦点。")
                        .font(.system(size: Theme.captionSize))
                        .foregroundStyle(Theme.secondary)
                }
                Spacer()
                StatusBadge(title: devices.sessionTitle, active: devices.isSharing)
            }

            if devices.receiverState == .startupRejected {
                permissionCard
            } else if let layout = model.sessionLayout {
                SessionLayoutView(
                    layout: layout,
                    focusScreenID: model.focusScreenID,
                    routeEnabled: model.routeEnabled,
                    sourceName: devices.selected?.name ?? "另一台电脑",
                    localDeviceID: devices.localDeviceID
                )
            } else {
                waitingCanvas
            }

            HStack {
                Text(devices.selected.map { "当前配对设备：\($0.name)" } ?? "尚未配对设备")
                Spacer()
                Text("支持 Windows ↔ Mac 双向跨屏键鼠")
            }
            .font(.system(size: 12))
            .foregroundStyle(Theme.secondary)
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.background)
    }

    private var permissionCard: some View {
        VStack(spacing: 14) {
            Image(systemName: "lock.shield")
                .font(.system(size: 38, weight: .light))
                .foregroundStyle(.orange)
            Text("需要系统输入权限").font(.headline)
            Text(devices.message)
                .foregroundStyle(Theme.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
            HStack(spacing: 12) {
                Button("打开隐私与安全性设置") {
                    if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility") {
                        NSWorkspace.shared.open(url)
                    }
                }
                .buttonStyle(.borderedProminent)
                Button("重新检测") { devices.receive() }
                    .buttonStyle(.bordered)
                    .disabled(!devices.canStart)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(24)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
    }

    private var waitingCanvas: some View {
        VStack(spacing: 14) {
            Image(systemName: devices.operation == .receiver ? "dot.radiowaves.left.and.right" : "rectangle.3.group")
                .font(.system(size: 38, weight: .light))
                .foregroundStyle(devices.operation == .receiver ? Theme.accent : Theme.secondary)
            Text(devices.devices.isEmpty ? "尚未配对电脑" : devices.operation == .receiver ? "已就绪，等待 Windows 连接" : "共享已暂停")
                .font(.headline)
            Text(devices.devices.isEmpty
                 ? "点击左侧「＋ 添加设备」显示配对码，在 Windows 上选择这台 Mac 完成配对。"
                 : devices.operation == .receiver
                     ? "在 Windows 客户端确认屏幕排列并点击「开启跨屏共享」，这里将自动显示同步画面。"
                     : "点击左侧「开启共享」即可恢复监听等待连接。")
                .foregroundStyle(Theme.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
            if devices.devices.isEmpty {
                Button("添加设备") { openAddDevice() }.buttonStyle(.borderedProminent)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(24)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
        .accessibilityIdentifier("session-layout-empty")
    }

    private func openAddDevice() {
        if devices.operation == .receiver {
            devices.stop()
        }
        showAddDevice = true
    }
}

private struct AddDeviceSheet: View {
    @Bindable var devices: DeviceModel
    @Binding var isPresented: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack {
                Text("添加设备").font(.title2.bold())
                Spacer()
                Button("关闭") {
                    if devices.operation == .invitation { devices.stop() }
                    isPresented = false
                }
            }

            if let confirmation = devices.confirmation {
                VStack(alignment: .leading, spacing: 12) {
                    Label(confirmation.name, systemImage: "desktopcomputer").font(.headline)
                    Text("请确认这是你要配对的 Windows 电脑。")
                        .foregroundStyle(Theme.secondary)
                    Text("设备标识：\(confirmation.fingerprint.suffix(12))")
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(Theme.secondary)
                    HStack(spacing: 10) {
                        Button(devices.approved ? "等待对方确认…" : "确认配对") { devices.approve() }
                            .buttonStyle(.borderedProminent)
                            .disabled(devices.approved || devices.stopping)
                            .accessibilityIdentifier("device-approve")
                        Button("取消") {
                            devices.stop()
                            isPresented = false
                        }
                        .disabled(devices.stopping)
                    }
                }
                .padding(16)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.background, in: RoundedRectangle(cornerRadius: 12))
                .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
            } else if !devices.code.isEmpty {
                VStack(spacing: 12) {
                    Text("在 Windows 点击「添加设备」，选择这台 Mac 并输入下方配对码：")
                        .foregroundStyle(Theme.secondary)
                        .multilineTextAlignment(.center)
                    Text(devices.code)
                        .font(.system(size: 38, weight: .semibold, design: .monospaced))
                        .tracking(8)
                        .foregroundStyle(Theme.accent)
                        .padding(.vertical, 8)
                    Text("配对码 2 分钟内有效 · 确认后自动连接")
                        .font(.caption)
                        .foregroundStyle(Theme.secondary)
                }
                .frame(maxWidth: .infinity)
                .padding(20)
                .background(Theme.background, in: RoundedRectangle(cornerRadius: 12))
                .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
            } else {
                HStack(spacing: 10) {
                    ProgressView().controlSize(.small)
                    Text(devices.message).foregroundStyle(Theme.secondary)
                }
                .padding(.vertical, 16)
            }
        }
        .padding(24)
        .frame(width: 440)
        .background(Theme.surface)
        .task {
            // Wait briefly if receiver was just stopping, then automatically generate pairing code!
            for _ in 0..<25 where !devices.canStart {
                try? await Task.sleep(for: .milliseconds(100))
            }
            if devices.canStart {
                devices.invite()
            }
        }
        .onChange(of: devices.operation) { oldOp, newOp in
            // Pairing completed cleanly -> auto-close sheet and auto-start listening!
            if oldOp == .invitation && newOp == nil && devices.message.contains("配对成功") {
                isPresented = false
            }
        }
    }
}
