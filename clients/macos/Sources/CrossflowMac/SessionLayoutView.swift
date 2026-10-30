import SwiftUI

/// Read-only authenticated geometry with stable screen identities.
struct SessionLayoutView: View {
    let layout: SharedLayout
    let focusScreenID: String?
    let routeEnabled: Bool
    let sourceName: String

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Label("会话屏幕排列", systemImage: "rectangle.3.group").fontWeight(.medium)
                Spacer()
                Text(routeEnabled ? "共享已开启" : "共享已暂停")
                    .foregroundStyle(Theme.secondary)
            }
            GeometryReader { area in
                let left = layout.screens.map(\.x).min() ?? 0
                let top = layout.screens.map(\.y).min() ?? 0
                let right = layout.screens.map { $0.x + $0.width }.max() ?? 1
                let bottom = layout.screens.map { $0.y + $0.height }.max() ?? 1
                let scale = max(0.000001, min((area.size.width - 24) / max(1, right - left),
                                            (area.size.height - 24) / max(1, bottom - top)))
                let originX = (area.size.width - (right - left) * scale) / 2
                let originY = (area.size.height - (bottom - top) * scale) / 2
                ZStack(alignment: .topLeading) {
                    Color.clear
                    ForEach(layout.screens) { screen in
                        let focused = routeEnabled && focusScreenID == screen.id
                        let local = screen.deviceID == layout.receiverDeviceID
                        let ordinal = layout.screens.prefix(while: { $0.id != screen.id })
                            .filter { $0.deviceID == screen.deviceID }.count + 1
                        RoundedRectangle(cornerRadius: 8)
                            .fill(focused ? Theme.subtle : Theme.background)
                            .overlay(RoundedRectangle(cornerRadius: 8)
                                .stroke(focused ? Theme.accent : Theme.border, lineWidth: focused ? 3 : 1))
                            .overlay {
                                ViewThatFits {
                                    VStack(spacing: 5) {
                                        Image(systemName: local ? "laptopcomputer" : "display")
                                        Text(local ? "这台 Mac" : sourceName).fontWeight(.medium).lineLimit(1)
                                        Text("屏幕 \(ordinal)").font(.caption).lineLimit(1)
                                        if focused { Text("键鼠焦点").font(.caption).foregroundStyle(Theme.accent) }
                                    }.padding(8)
                                    Image(systemName: local ? "laptopcomputer" : "display")
                                    Color.clear
                                }
                            }
                            .frame(width: screen.width * scale, height: screen.height * scale)
                            .offset(x: originX + (screen.x - left) * scale, y: originY + (screen.y - top) * scale)
                            .accessibilityElement(children: .ignore)
                            .accessibilityLabel("\(local ? "这台 Mac" : sourceName)，屏幕 \(ordinal)，\(focused ? "当前键鼠焦点" : "非焦点屏幕")")
                    }
                }.clipped()
            }.frame(minHeight: 180)
            Text("由 Windows 管理排列 · 更改会通过当前连接同步。")
                .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
        }
        .padding(18).background(Theme.surface, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).stroke(Theme.border, lineWidth: 1))
        .accessibilityIdentifier("session-layout")
    }
}
