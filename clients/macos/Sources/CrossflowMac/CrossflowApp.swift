import AppKit
import SwiftUI

@main
enum CrossflowLauncher {
    @MainActor static func main() async {
        let args = CommandLine.arguments
        if args.count >= 2 && args[1] == "--self-test" {
            guard args.count == 3 else {
                FileHandle.standardError.write(Data("Usage: CrossflowMac --self-test output.json\n".utf8))
                exit(2)
            }
            exit(await SelfTest.run(output: URL(fileURLWithPath: args[2])))
        }
        CrossflowApp.main()
    }
}

struct CrossflowApp: App {
    @NSApplicationDelegateAdaptor(DiscoveryAppDelegate.self) private var appDelegate
    @ViewState private var model = WorkspaceModel()
    var body: some Scene {
        Window("Crossflow", id: "workspace") {
            WorkspaceView(model: model, peer: appDelegate.peer, updater: appDelegate.updater)
                .background(WindowCloseHider { window in
                    appDelegate.attachMainWindow(window)
                })
                .preferredColorScheme(.light)
                .frame(minWidth: Theme.number("window", "minimum_width", fallback: 880),
                       minHeight: Theme.number("window", "minimum_height", fallback: 600))
        }
        .defaultSize(width: Theme.number("window", "width", fallback: 1180),
                     height: Theme.number("window", "height", fallback: 760))
        .windowResizability(.contentMinSize)
        .commands {
            CommandGroup(replacing: .newItem) { }
        }
    }
}

private struct WindowCloseHider: NSViewRepresentable {
    let onWindow: @MainActor (NSWindow) -> Void

    func makeNSView(context: Context) -> NSView {
        let view = WindowAttachView()
        view.onWindow = onWindow
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        if let v = nsView as? WindowAttachView {
            v.onWindow = onWindow
            if let win = v.window {
                onWindow(win)
            }
        }
    }

    private final class WindowAttachView: NSView {
        var onWindow: (@MainActor (NSWindow) -> Void)?
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if let win = window {
                onWindow?(win)
            }
        }
    }
}
