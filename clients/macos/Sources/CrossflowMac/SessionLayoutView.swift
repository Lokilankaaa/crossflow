import SwiftUI

/// Interactive multi-display layout canvas with edge snapping and live focus highlight.
struct SessionLayoutView: View {
    let layout: SharedLayout
    let focusScreenID: String?
    let routeEnabled: Bool
    let sourceName: String
    let localDeviceID: String?
    var localName: String = "这台 Mac"
    var editable: Bool = false
    var onCommitLayout: ((SharedLayout) -> Void)? = nil

    @State private var draggingID: String?
    @State private var dragX: Double = 0
    @State private var dragY: Double = 0

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Label("屏幕排列画布", systemImage: "rectangle.3.group").fontWeight(.medium)
                Spacer()
                Text(routeEnabled ? "共享中 · 可直接拖动调整" : "已暂停 · 可拖动调整")
                    .font(.system(size: Theme.captionSize))
                    .foregroundStyle(Theme.secondary)
            }
            GeometryReader { area in
                let screens = layout.screens
                let left = screens.map(\.x).min() ?? 0
                let top = screens.map(\.y).min() ?? 0
                let right = screens.map { $0.x + $0.width }.max() ?? 1
                let bottom = screens.map { $0.y + $0.height }.max() ?? 1
                let pad: Double = 28
                let scale = max(0.000001, min((area.size.width - pad * 2) / max(1, right - left),
                                              (area.size.height - pad * 2) / max(1, bottom - top)))
                let originX = (area.size.width - (right - left) * scale) / 2
                let originY = (area.size.height - (bottom - top) * scale) / 2

                ZStack(alignment: .topLeading) {
                    // Subtle dotted grid background matching Windows Quiet White canvas
                    Canvas { ctx, size in
                        let dotColor = Theme.border
                        var x: CGFloat = 20
                        while x < size.width {
                            var y: CGFloat = 20
                            while y < size.height {
                                ctx.fill(Path(ellipseIn: CGRect(x: x, y: y, width: 2, height: 2)), with: .color(dotColor))
                                y += 28
                            }
                            x += 28
                        }
                    }

                    ForEach(screens) { screen in
                        let isDragging = draggingID == screen.id
                        let currentX = isDragging ? dragX : screen.x
                        let currentY = isDragging ? dragY : screen.y
                        let focused = routeEnabled && focusScreenID == screen.id
                        let local = screen.deviceID == localDeviceID
                        let ordinal = screens.prefix(while: { $0.id != screen.id })
                            .filter { $0.deviceID == screen.deviceID }.count + 1
                        let accentColor = local ? Theme.accent : Color(red: 13/255, green: 148/255, blue: 136/255)
                        let tileW = max(24, screen.width * scale)
                        let tileH = max(24, screen.height * scale)

                        RoundedRectangle(cornerRadius: 9)
                            .fill(accentColor.opacity(focused ? 0.18 : isDragging ? 0.14 : 0.08))
                            .overlay(
                                RoundedRectangle(cornerRadius: 9)
                                    .stroke(accentColor, lineWidth: focused ? 3 : isDragging ? 2 : 1.2)
                            )
                            .overlay {
                                ViewThatFits {
                                    VStack(spacing: 4) {
                                        Image(systemName: local ? "laptopcomputer" : "display")
                                            .foregroundStyle(accentColor)
                                        Text("\(local ? localName : sourceName) · 屏 \(ordinal)")
                                            .font(.system(size: 13, weight: .semibold))
                                            .lineLimit(1)
                                        Text("\(Int(screen.width)) × \(Int(screen.height))")
                                            .font(.caption2)
                                            .foregroundStyle(Theme.secondary)
                                        if focused {
                                            Text("键鼠焦点")
                                                .font(.caption2.weight(.semibold))
                                                .foregroundStyle(accentColor)
                                        }
                                    }
                                    .padding(6)
                                    VStack(spacing: 2) {
                                        Image(systemName: local ? "laptopcomputer" : "display")
                                            .foregroundStyle(accentColor)
                                        Text("屏 \(ordinal)").font(.caption2)
                                    }
                                    Image(systemName: local ? "laptopcomputer" : "display")
                                }
                            }
                            .frame(width: tileW, height: tileH)
                            .offset(x: originX + (currentX - left) * scale,
                                    y: originY + (currentY - top) * scale)
                            .zIndex(isDragging ? 10 : focused ? 5 : 1)
                            .gesture(
                                DragGesture(minimumDistance: 2, coordinateSpace: .named("layoutCanvas"))
                                    .onChanged { value in
                                        guard editable else { return }
                                        let rawX = screen.x + Double(value.translation.width) / scale
                                        let rawY = screen.y + Double(value.translation.height) / scale
                                        let (sx, sy) = snapPosition(for: screen, x: rawX, y: rawY, scale: scale)
                                        draggingID = screen.id
                                        dragX = sx
                                        dragY = sy
                                    }
                                    .onEnded { value in
                                        guard editable else { return }
                                        let rawX = screen.x + Double(value.translation.width) / scale
                                        let rawY = screen.y + Double(value.translation.height) / scale
                                        let (sx, sy) = snapPosition(for: screen, x: rawX, y: rawY, scale: scale)
                                        draggingID = nil
                                        let finalX = sx.rounded()
                                        let finalY = sy.rounded()
                                        guard finalX != screen.x || finalY != screen.y else { return }
                                        let updatedScreens = screens.map { item -> SharedScreen in
                                            guard item.id == screen.id else { return item }
                                            return SharedScreen(
                                                id: item.id,
                                                deviceID: item.deviceID,
                                                nativeID: item.nativeID,
                                                x: finalX,
                                                y: finalY,
                                                width: item.width,
                                                height: item.height
                                            )
                                        }
                                        let updated = SharedLayout(
                                            sourceDeviceID: layout.sourceDeviceID,
                                            receiverDeviceID: layout.receiverDeviceID,
                                            screens: updatedScreens
                                        )
                                        onCommitLayout?(updated)
                                    }
                            )
                            .accessibilityElement(children: .ignore)
                            .accessibilityLabel("\(local ? localName : sourceName)，屏幕 \(ordinal)，\(focused ? "当前键鼠焦点" : "非焦点屏幕")")
                    }
                }
                .coordinateSpace(name: "layoutCanvas")
                .background(Theme.background, in: RoundedRectangle(cornerRadius: 8))
                .clipped()
            }
            .frame(minHeight: 220)

            Text(editable ? "拖动屏幕贴合相邻边缘，松手后自动同步到双方电脑。" : "两端屏幕已同步。")
                .font(.system(size: Theme.captionSize))
                .foregroundStyle(Theme.secondary)
        }
        .padding(18)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
        .accessibilityIdentifier("session-layout")
    }

    private func snapPosition(for target: SharedScreen, x: Double, y: Double, scale: Double) -> (Double, Double) {
        let threshold = 10.0 / max(scale, 0.0001)
        var outX = x
        var outY = y
        for other in layout.screens where other.id != target.id {
            let xEdges = [other.x - target.width, other.x, other.x + other.width, other.x + other.width - target.width]
            for edge in xEdges where abs(outX - edge) < threshold {
                outX = edge
                break
            }
            let yEdges = [other.y - target.height, other.y, other.y + other.height, other.y + other.height - target.height]
            for edge in yEdges where abs(outY - edge) < threshold {
                outY = edge
                break
            }
        }
        return (outX, outY)
    }
}
