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
    @ViewState private var model = WorkspaceModel()
    var body: some Scene {
        Window("Crossflow", id: "workspace") {
            WorkspaceView(model: model)
                .frame(minWidth: Theme.number("window", "min_width", fallback: 880),
                       minHeight: Theme.number("window", "min_height", fallback: 600))
        }
        .defaultSize(width: Theme.number("window", "width", fallback: 1180),
                     height: Theme.number("window", "height", fallback: 760))
        .windowResizability(.contentMinSize)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("导入布局…") { model.importDocument() }.keyboardShortcut("o")
                Button("导出布局草稿…") { Task { await model.exportDocument() } }.keyboardShortcut("s")
            }
            CommandGroup(replacing: .undoRedo) {
                Button("撤销") { model.undo() }.keyboardShortcut("z").disabled(!model.canUndo)
                Button("重做") { model.redo() }.keyboardShortcut("z", modifiers: [.command, .shift]).disabled(!model.canRedo)
            }
        }
    }
}
