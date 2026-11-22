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
            WorkspaceView(model: model, peer: appDelegate.peer)
                .onDisappear { Task { _ = await appDelegate.peer.shutdown() } }
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
