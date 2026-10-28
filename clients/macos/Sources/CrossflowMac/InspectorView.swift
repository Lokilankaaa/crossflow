import SwiftUI

struct InspectorView: View {
    @Bindable var model: WorkspaceModel

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                Text("屏幕属性").font(.system(size: 16, weight: .semibold))
                if let screen = model.selectedScreen {
                    VStack(alignment: .leading, spacing: 7) {
                        Label(screen.deviceID.isEmpty ? "未命名设备" : screen.deviceID, systemImage: "display")
                            .fontWeight(.medium)
                        Text(model.isExample ? "示例屏幕 · 未连接" : "草稿屏幕 · 未连接")
                            .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                    }
                    Divider()
                    VStack(alignment: .leading, spacing: 14) {
                        stringField("设备 ID", key: \.deviceID, screen: screen)
                        stringField("屏幕 ID", key: \.screenID, screen: screen)
                    }
                    VStack(alignment: .leading, spacing: 12) {
                        sectionTitle("位置")
                        HStack(spacing: 10) {
                            numberField("X", key: \.x, screen: screen)
                            numberField("Y", key: \.y, screen: screen)
                        }
                    }
                    VStack(alignment: .leading, spacing: 12) {
                        sectionTitle("尺寸")
                        HStack(spacing: 10) {
                            numberField("宽度", key: \.width, screen: screen)
                            numberField("高度", key: \.height, screen: screen)
                        }
                        Text("使用桌面逻辑坐标，不是物理毫米。")
                            .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                    }
                    Button("从草稿移除", systemImage: "minus.circle", role: .destructive) { model.removeSelected() }
                        .buttonStyle(.borderless)
                } else {
                    Text("选择一块屏幕以编辑属性。")
                        .foregroundStyle(Theme.secondary)
                }
                Divider()
                VStack(alignment: .leading, spacing: 12) {
                    sectionTitle("布局校验")
                    Label(model.validationMessage, systemImage: model.validationPassed ? "checkmark.circle.fill" : "circle.dashed")
                        .font(.system(size: Theme.captionSize))
                        .foregroundStyle(model.validationPassed ? Theme.deviceAccent : Theme.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Button {
                        Task { await model.validate() }
                    } label: {
                        HStack {
                            if model.validating { ProgressView().controlSize(.small) }
                            Text("校验布局")
                        }.frame(maxWidth: .infinity)
                    }
                    .disabled(!model.bridgeAvailable || model.validating)
                    .accessibilityIdentifier("validate-layout")
                    Text("由共享 Rust 内核检查。校验不会连接设备或启用共享。")
                        .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }.padding(20)
        }
        .background(Theme.surface)
        .textFieldStyle(.roundedBorder)
    }
    private func sectionTitle(_ title: String) -> some View {
        Text(title).font(.system(size: Theme.captionSize, weight: .semibold)).foregroundStyle(Theme.secondary)
    }
    private func stringField(_ title: String, key: WritableKeyPath<ScreenDraft, String>, screen: ScreenDraft) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            sectionTitle(title)
            TextField(title, text: Binding(get: { model.selectedScreen?[keyPath: key] ?? "" }, set: { value in
                guard var updated = model.selectedScreen else { return }
                updated[keyPath: key] = value; model.update(updated)
            }))
            .accessibilityLabel(title)
        }
    }
    private func numberField(_ title: String, key: WritableKeyPath<ScreenDraft, Double>, screen: ScreenDraft) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            sectionTitle(title)
            TextField(title, value: Binding(get: { model.selectedScreen?[keyPath: key] ?? 0 }, set: { value in
                guard var updated = model.selectedScreen else { return }
                updated[keyPath: key] = value; model.update(updated)
            }), format: .number.grouping(.never))
            .font(.system(size: Theme.bodySize, design: .monospaced)).accessibilityLabel(title)
        }
    }
}
