import SwiftUI

struct ArrangementCanvas: View {
    @Bindable var model: WorkspaceModel
    @ViewState private var center = CGPoint(x: 1695, y: 540)
    @ViewState private var fitScale: CGFloat = 0.14
    @ViewState private var zoom = 1.0

    var body: some View {
        GeometryReader { geometry in
            VStack(spacing: 0) {
                HStack {
                    Label("布局画布", systemImage: "rectangle.3.group").fontWeight(.medium)
                    Spacer()
                    Button { zoom = max(0.4, zoom - 0.2) } label: { Image(systemName: "minus") }.accessibilityLabel("缩小画布")
                    Text("\(Int(zoom * 100))%") .monospacedDigit().frame(width: 42)
                    Button { zoom = min(3, zoom + 0.2) } label: { Image(systemName: "plus") }.accessibilityLabel("放大画布")
                    Button { fit(geometry.size) } label: { Image(systemName: "arrow.up.left.and.arrow.down.right") }.help("适应所有屏幕").accessibilityLabel("适应所有屏幕")
                }
                .buttonStyle(.borderless).font(.system(size: Theme.captionSize))
                .padding(14).background(Theme.surface)
                Divider().overlay(Theme.border)
                GeometryReader { area in
                    let scale = fitScale * zoom
                    ZStack {
                        dotGrid
                        if model.document.screens.isEmpty {
                            VStack(spacing: 12) {
                                Image(systemName: "display").font(.system(size: 32)).foregroundStyle(Theme.secondary)
                                Text("从一块屏幕开始").font(.headline)
                                Button("添加屏幕") { model.addScreen() }
                            }
                        }
                        ForEach(Array(model.document.screens.enumerated()), id: \.element.id) { index, screen in
                            ScreenTile(screen: screen, selected: model.selectedID == screen.id,
                                       example: model.isExample, index: index, scale: scale,
                                       onSelect: { model.selectedID = screen.id }, onMove: { x, y in
                                var moved = screen; moved.x = x; moved.y = y; model.update(moved)
                            })
                            .position(x: area.size.width / 2 + (screen.x + displayWidth(screen) / 2 - center.x) * scale,
                                      y: area.size.height / 2 + (screen.y + displayHeight(screen) / 2 - center.y) * scale)
                        }
                    }
                    .clipped()
                }
                HStack {
                    Image(systemName: "hand.draw")
                    Text("拖动调整位置 · 右侧可输入精确坐标")
                    Spacer()
                    Text("逻辑坐标")
                }.font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary).padding(12)
            }
            .background(Theme.surface)
            .clipShape(RoundedRectangle(cornerRadius: 14))
            .overlay(RoundedRectangle(cornerRadius: 14).stroke(Theme.border, lineWidth: 1))
            .onAppear { fit(geometry.size) }
            .onChange(of: geometry.size) { _, size in fit(size) }
            .onChange(of: model.fitGeneration) { _, _ in fit(geometry.size) }
        }
        .frame(minHeight: 280, maxHeight: .infinity)
    }
    private var dotGrid: some View {
        Canvas { context, size in
            for x in stride(from: 14.0, to: size.width, by: 22) {
                for y in stride(from: 14.0, to: size.height, by: 22) {
                    context.fill(Path(ellipseIn: CGRect(x: x, y: y, width: 1.5, height: 1.5)), with: .color(Theme.border))
                }
            }
        }.background(Theme.background.opacity(0.6)).accessibilityHidden(true)
    }
    // Rendering safeguards only: invalid draft geometry is still passed unchanged to Rust.
    private func displayWidth(_ s: ScreenDraft) -> Double { min(max(s.width, 1), 100_000) }
    private func displayHeight(_ s: ScreenDraft) -> Double { min(max(s.height, 1), 100_000) }
    private func fit(_ size: CGSize) {
        let screens = model.document.screens
        guard !screens.isEmpty else { return }
        let left = screens.map(\.x).min() ?? 0
        let top = screens.map(\.y).min() ?? 0
        let right = screens.map { $0.x + displayWidth($0) }.max() ?? 1
        let bottom = screens.map { $0.y + displayHeight($0) }.max() ?? 1
        center = CGPoint(x: left / 2 + right / 2, y: top / 2 + bottom / 2)
        fitScale = min(max((size.width - 90) / max(right - left, 1), 0.000001), max((size.height - 150) / max(bottom - top, 1), 0.000001), 0.3)
        zoom = 1
    }
}

private struct ScreenTile: View {
    let screen: ScreenDraft
    let selected: Bool
    let example: Bool
    let index: Int
    let scale: CGFloat
    let onSelect: () -> Void
    let onMove: (Double, Double) -> Void
    @GestureState private var translation = CGSize.zero

    private var tint: Color { index % 2 == 0 ? Theme.accent : Theme.deviceAccent }
    private var accessibilityTitle: String {
        let kind = example ? "示例" : "草稿"
        let state = selected ? "已选择" : "未选择"
        return "\(kind)屏幕 \(screen.deviceID)，\(state)"
    }
    var body: some View {
        card
            .offset(translation)
            .onTapGesture(perform: onSelect)
            .gesture(drag)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(Text(accessibilityTitle))
            .accessibilityHint("选择后可在右侧编辑坐标和尺寸")
            .accessibilityAddTraits(.isButton)
            .accessibilityAction { onSelect() }
    }
    private var card: some View {
        VStack(spacing: 9) {
            HStack {
                Text(String(format: "%02d", index + 1)).font(.system(size: 11, weight: .semibold, design: .monospaced))
                Spacer(minLength: 0)
                Text(example ? "示例" : "草稿").font(.system(size: 10, weight: .medium))
            }.foregroundStyle(tint)
            Spacer(minLength: 0)
            Image(systemName: screen.deviceID.lowercased().contains("mac") ? "laptopcomputer" : "display")
                .font(.system(size: 26, weight: .light)).foregroundStyle(tint)
            Text(screen.deviceID.isEmpty ? "未命名" : screen.deviceID).font(.system(size: 14, weight: .semibold)).lineLimit(1)
            Text("\(Int(clamping: screen.width)) × \(Int(clamping: screen.height))")
                .font(.system(size: 11, design: .monospaced)).foregroundStyle(Theme.secondary).lineLimit(1)
            Spacer(minLength: 0)
        }
        .padding(12)
        .frame(width: max(112, min(screen.width, 100_000) * scale), height: max(120, min(screen.height, 100_000) * scale))
        .background(LinearGradient(colors: [Theme.surface, tint.opacity(0.08)], startPoint: .topLeading, endPoint: .bottomTrailing))
        .clipShape(RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(selected ? tint : Theme.border, lineWidth: selected ? 2 : 1))
        .shadow(color: tint.opacity(selected ? 0.12 : 0.03), radius: 12, y: 5)
    }
    private var drag: some Gesture {
        DragGesture(minimumDistance: 3)
            .updating($translation) { value, state, _ in state = value.translation }
            .onChanged { _ in onSelect() }
            .onEnded { value in
                onMove((screen.x + value.translation.width / scale).rounded(),
                       (screen.y + value.translation.height / scale).rounded())
            }
    }
}

extension Int {
    init(clamping value: Double) {
        if !value.isFinite { self = 0 }
        else if value >= Double(Int.max) { self = Int.max }
        else if value <= Double(Int.min) { self = Int.min }
        else { self = Int(value) }
    }
}
