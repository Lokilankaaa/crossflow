import SwiftUI

/// Both pages render the same receiver state; showing a page never starts input.
struct SessionStatusCard: View {
    @Bindable var model: DeviceModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Label(model.sessionTitle, systemImage: model.isSharing ? "keyboard" : "rectangle.2.swap")
                    .font(.headline)
                Spacer()
                if model.operation != nil || model.stopPending {
                    Button(model.stopping ? "正在停止…" : "停止连接") { model.stop() }
                        .disabled(model.stopping || model.stopPending)
                        .accessibilityIdentifier("session-stop")
                } else {
                    Button("开启接收", systemImage: "arrow.down.forward") { model.receive() }
                        .buttonStyle(.borderedProminent)
                        .disabled(!model.canStart || model.selected == nil)
                        .accessibilityIdentifier("device-receive")
                }
            }
            Text(model.message).foregroundStyle(Theme.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if model.operation == nil {
                HStack {
                    Picker("来自", selection: $model.selectedID) {
                        Text("选择已配对设备").tag(String?.none)
                        ForEach(model.devices) { device in
                            Text(device.name).tag(Optional(device.id))
                        }
                    }.disabled(!model.canStart || model.loading)
                    Picker("接收内容", selection: $model.includeKeyboard) {
                        Text("键盘、鼠标与滚轮").tag(true)
                        Text("仅鼠标移动").tag(false)
                    }.disabled(!model.canStart)
                        .accessibilityIdentifier("device-receive-mode")
                }
                Text("开启后等待另一台电脑连接。跨屏共享由发送端开启，鼠标越过相邻边缘后才在本机接收输入。")
                    .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
            }
        }
        .padding(18).background(Theme.surface, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).stroke(Theme.border, lineWidth: 1))
        .accessibilityIdentifier("session-status")
    }
}
