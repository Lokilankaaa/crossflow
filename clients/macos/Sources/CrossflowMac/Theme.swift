import AppKit
import SwiftUI

// SDK 27 also exports a State macro whose plugin is not shipped with CLT.
// Select the longstanding property-wrapper type explicitly, retaining macOS 14 support.
typealias ViewState<Value> = SwiftUI.State<Value>

/// The shared JSON is copied into the app by the build script, never forked here.
enum Theme {
    static let tokens: JSONValue? = {
        guard let url = Bundle.main.url(forResource: "design-tokens", withExtension: "json"),
              let data = try? Data(contentsOf: url) else { return nil }
        return try? JSONDecoder().decode(JSONValue.self, from: data)
    }()
    static func color(_ name: String, fallback: NSColor) -> Color {
        guard let hex = tokens?["colors"]?[name]?.string,
              let rgb = UInt32(hex.trimmingCharacters(in: CharacterSet(charactersIn: "#")), radix: 16) else {
            return Color(nsColor: fallback)
        }
        return Color(.sRGB, red: Double((rgb >> 16) & 255) / 255,
                     green: Double((rgb >> 8) & 255) / 255, blue: Double(rgb & 255) / 255, opacity: 1)
    }
    static func number(_ group: String, _ key: String, fallback: Double) -> Double {
        tokens?[group]?[key]?.number ?? fallback
    }
    static func copy(_ key: String, fallback: String) -> String {
        tokens?["copy"]?[key]?.string ?? fallback
    }
    static var background: Color { color("background", fallback: .windowBackgroundColor) }
    static var surface: Color { color("surface", fallback: .controlBackgroundColor) }
    static var text: Color { color("text", fallback: .labelColor) }
    static var secondary: Color { color("text_secondary", fallback: .secondaryLabelColor) }
    static var border: Color { color("border", fallback: .separatorColor) }
    static var accent: Color { color("accent", fallback: .controlAccentColor) }
    static var subtle: Color { color("accent_subtle", fallback: .quaternaryLabelColor) }
    static var deviceAccent: Color { color("device_secondary", fallback: .systemTeal) }
    static var titleSize: Double { number("typography", "title", fallback: 26) }
    static var bodySize: Double { number("typography", "body", fallback: 14) }
    static var captionSize: Double { number("typography", "caption", fallback: 12) }
    static var sidebarWidth: Double { number("window", "sidebar_width", fallback: 236) }
    static var inspectorWidth: Double { number("window", "inspector_width", fallback: 260) }
}

struct StatusBadge: View {
    let title: String
    var active = false
    var body: some View {
        HStack(spacing: 6) {
            Circle().fill(active ? Theme.accent : Theme.secondary).frame(width: 6, height: 6)
            Text(title).font(.system(size: Theme.captionSize, weight: .medium))
        }
        .foregroundStyle(active ? Theme.accent : Theme.secondary)
        .padding(.horizontal, 10).padding(.vertical, 6)
        .background(active ? Theme.subtle : Theme.background, in: Capsule())
    }
}
