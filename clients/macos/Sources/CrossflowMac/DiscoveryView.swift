import AppKit
import Observation
import SwiftUI

@MainActor @Observable
final class DiscoveryModel {
    var devices: [NearbyDevice] = []
    var scanning = false
    var message = "准备查找附近设备…"
    var cleanupFailed = false
    @ObservationIgnored private let client: DiscoveryClient
    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var task: Task<Void, Never>?
    @ObservationIgnored private var restartRequested = false

    init(client: DiscoveryClient = DiscoveryClient()) { self.client = client }

    func start() {
        if let task {
            // Re-entering the page while the previous cancellation is cleaning
            // up schedules one scan after it finishes, never a concurrent scan.
            if task.isCancelled { restartRequested = true }
            return
        }
        task = Task {
            await refresh()
            task = nil
            if restartRequested { restartRequested = false; start() }
        }
    }
    private func refresh() async {
        guard !scanning, !Task.isCancelled else { return }
        scanning = true; devices = []; message = "正在查找附近设备…"
        generation += 1; let current = generation
        defer { scanning = false }
        do {
            guard await client.stop() else { throw DiscoveryError.cleanupPending }
            try Task.checkCancellation()
            guard current == generation else { throw CancellationError() }
            let result = try await client.scan()
            guard current == generation, !Task.isCancelled else { return }
            cleanupFailed = false; devices = result.devices
            message = devices.isEmpty ? "暂未发现设备" : "发现 \(devices.count) 台附近设备"
            if result.capacityLimited { message += " · 部分设备未显示" }
        } catch DiscoveryError.cleanupPending {
            cleanupFailed = true; message = DiscoveryError.cleanupPending.localizedDescription
        } catch is CancellationError {
            if current == generation { message = "扫描已取消" }
        } catch {
            guard current == generation else { return }
            message = (error as? DiscoveryError)?.localizedDescription ?? DiscoveryError.failed.localizedDescription
        }
    }
    func cancel() {
        generation += 1; restartRequested = false; task?.cancel(); devices = []; client.cancel(); message = "扫描已取消"
    }
    func shutdown() async -> Bool {
        cancel()
        let stopped = await client.stop()
        cleanupFailed = !stopped
        if !stopped { message = DiscoveryError.cleanupPending.localizedDescription }
        return stopped
    }
}

private enum MenuBarVisualState: Equatable {
    case searching
    case connected
    case activeRemote
    case paused
}

@MainActor
final class DiscoveryAppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    let discovery = DiscoveryModel()
    let devices = DeviceModel()
    let peer = PeerMeshModel()
    private var awaitingShutdown = false
    private var statusItem: NSStatusItem?
    private var statusMenuItem: NSMenuItem?
    private var focusMenuItem: NSMenuItem?
    private var showHideMenuItem: NSMenuItem?
    private var toggleShareMenuItem: NSMenuItem?
    private var quickJumpParentItem: NSMenuItem?
    private var quickJumpSubmenu: NSMenu?
    private var screenLockMenuItem: NSMenuItem?
    private var clipboardMenuItem: NSMenuItem?
    private var inputStateMenuItem: NSMenuItem?
    private var channelParentItem: NSMenuItem?
    private var channelSubmenu: NSMenu?
    private var sendFilesParentItem: NSMenuItem?
    private var sendFilesSubmenu: NSMenu?
    private var autoStartMenuItem: NSMenuItem?
    private var lastVisualState: MenuBarVisualState?
    private var lastChannelMenuSig: String = ""
    private var lastPeersMenuSig: String = ""
    private var startedMinimized: Bool = CommandLine.arguments.contains("--minimized")
    private var didApplyInitialMinimize = false
    private weak var mainWindow: NSWindow?

    func applicationDidFinishLaunching(_ notification: Notification) {
        setupStatusBarItem()
        peer.onStatusChange = { [weak self] in
            self?.refreshMenuBar()
        }
        refreshMenuBar()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        showMainWindow()
        return true
    }

    func attachMainWindow(_ window: NSWindow) {
        if mainWindow !== window {
            mainWindow = window
            window.delegate = self
            window.styleMask.insert([.resizable, .miniaturizable])
            window.collectionBehavior.insert(.fullScreenPrimary)
            if startedMinimized && !didApplyInitialMinimize {
                didApplyInitialMinimize = true
                hideMainWindow(window)
            }
            refreshMenuBar()
        }
    }

    func windowWillUseStandardFrame(_ window: NSWindow, defaultFrame newFrame: NSRect) -> NSRect {
        window.screen?.visibleFrame ?? newFrame
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        if awaitingShutdown {
            return true
        }
        hideMainWindow(sender)
        return false
    }

    private func hideMainWindow(_ window: NSWindow) {
        window.orderOut(nil)
        refreshMenuBar()
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard !awaitingShutdown else { return .terminateLater }
        awaitingShutdown = true
        if let item = statusItem {
            NSStatusBar.system.removeStatusItem(item)
            statusItem = nil
        }
        Task {
            let peerStopped = await peer.shutdown()
            let devicesStopped = await devices.shutdown()
            let discoveryStopped = await discovery.shutdown()
            awaitingShutdown = false
            sender.reply(toApplicationShouldTerminate: peerStopped && devicesStopped && discoveryStopped)
        }
        return .terminateLater
    }

    private func setupStatusBarItem() {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        if let button = item.button {
            button.image = renderMenuBarIcon(state: .searching)
            button.imagePosition = .imageOnly
            button.toolTip = "Crossflow"
        }

        let menu = NSMenu()
        menu.autoenablesItems = false

        let statusEntry = NSMenuItem(title: "状态：正在发现设备", action: nil, keyEquivalent: "")
        statusEntry.isEnabled = false
        menu.addItem(statusEntry)

        let focusEntry = NSMenuItem(title: "", action: nil, keyEquivalent: "")
        focusEntry.isEnabled = false
        focusEntry.isHidden = true
        menu.addItem(focusEntry)

        menu.addItem(.separator())

        let showHideEntry = NSMenuItem(title: "隐藏到菜单栏", action: #selector(toggleMainWindow), keyEquivalent: "")
        showHideEntry.target = self
        menu.addItem(showHideEntry)

        let toggleShareEntry = NSMenuItem(title: "暂停共享", action: #selector(toggleSharing), keyEquivalent: "")
        toggleShareEntry.target = self
        toggleShareEntry.isHidden = true
        menu.addItem(toggleShareEntry)

        let qjParent = NSMenuItem(title: "快速切换 (QuickJump)", action: nil, keyEquivalent: "")
        let qjSub = NSMenu()
        qjParent.submenu = qjSub
        qjParent.isHidden = true
        menu.addItem(qjParent)

        menu.addItem(.separator())

        let lockEntry = NSMenuItem(title: "锁定当前屏幕（防误触）", action: #selector(toggleScreenLock), keyEquivalent: "")
        lockEntry.target = self
        lockEntry.state = .off
        menu.addItem(lockEntry)

        let clipEntry = NSMenuItem(title: "剪贴板共享", action: #selector(toggleClipboardSync), keyEquivalent: "")
        clipEntry.target = self
        clipEntry.state = .on
        menu.addItem(clipEntry)

        let imeEntry = NSMenuItem(title: "中英文与大小写同步", action: #selector(toggleInputStateSync), keyEquivalent: "")
        imeEntry.target = self
        imeEntry.state = .on
        menu.addItem(imeEntry)

        let chParent = NSMenuItem(title: "网络信道", action: nil, keyEquivalent: "")
        let chSub = NSMenu()
        chParent.submenu = chSub
        chParent.isHidden = true
        menu.addItem(chParent)

        menu.addItem(.separator())

        let sendParent = NSMenuItem(title: "发送文件到", action: nil, keyEquivalent: "")
        let sendSub = NSMenu()
        sendParent.submenu = sendSub
        sendParent.isEnabled = false
        menu.addItem(sendParent)

        let folderEntry = NSMenuItem(title: "打开接收文件夹", action: #selector(openReceiveFolder), keyEquivalent: "")
        folderEntry.target = self
        menu.addItem(folderEntry)

        menu.addItem(.separator())

        let autoStartEntry = NSMenuItem(title: "开机自启动", action: #selector(toggleAutoStart), keyEquivalent: "")
        autoStartEntry.target = self
        autoStartEntry.state = isLaunchAgentEnabled() ? .on : .off
        menu.addItem(autoStartEntry)

        let quitEntry = NSMenuItem(title: "退出 Crossflow", action: #selector(quitApp), keyEquivalent: "q")
        quitEntry.target = self
        menu.addItem(quitEntry)

        item.menu = menu
        statusItem = item
        statusMenuItem = statusEntry
        focusMenuItem = focusEntry
        showHideMenuItem = showHideEntry
        toggleShareMenuItem = toggleShareEntry
        quickJumpParentItem = qjParent
        quickJumpSubmenu = qjSub
        screenLockMenuItem = lockEntry
        clipboardMenuItem = clipEntry
        inputStateMenuItem = imeEntry
        channelParentItem = chParent
        channelSubmenu = chSub
        sendFilesParentItem = sendParent
        sendFilesSubmenu = sendSub
        autoStartMenuItem = autoStartEntry
    }

    func refreshMenuBar() {
        let isRemoteFocus = peer.isSharing
            && peer.focusDeviceID != nil
            && peer.focusDeviceID != peer.localDeviceID
        let visual: MenuBarVisualState = {
            if peer.state == "paused" || peer.state == "error" || peer.state == "stopped" || peer.state == "startup_rejected" {
                return .paused
            }
            if isRemoteFocus { return .activeRemote }
            if peer.isSharing { return .connected }
            return .searching
        }()
        if visual != lastVisualState {
            lastVisualState = visual
            statusItem?.button?.image = renderMenuBarIcon(state: visual)
        }

        let peerTag = peer.peerName ?? "远端设备"
        let chSuffix = peer.activeChannelLabel.map { " · \($0)" } ?? ""
        if peer.isSharing {
            statusMenuItem?.title = "已连接：\(peerTag)\(chSuffix)"
            statusItem?.button?.toolTip = "Crossflow · 已连接 \(peerTag)\(chSuffix)"
        } else if peer.state == "paused" {
            statusMenuItem?.title = "已暂停：\(peerTag)"
            statusItem?.button?.toolTip = "Crossflow · 已暂停"
        } else {
            statusMenuItem?.title = "状态：\(peer.statusTitle)"
            statusItem?.button?.toolTip = "Crossflow · \(peer.statusTitle)"
        }

        if let focusItem = focusMenuItem {
            if peer.isSharing, peer.focusDeviceID != nil {
                let lockTag = peer.screenLocked ? "（已锁定）" : ""
                focusItem.title = "当前焦点：\(isRemoteFocus ? peerTag : "这台 Mac")\(lockTag)"
                focusItem.isHidden = false
            } else {
                focusItem.isHidden = true
            }
        }

        let visible = resolvedMainWindow()?.isVisible ?? false
        showHideMenuItem?.title = visible ? "隐藏到菜单栏" : "打开主界面"

        if let toggleItem = toggleShareMenuItem {
            toggleItem.isHidden = !peer.isConnected
            toggleItem.title = peer.isSharing ? "暂停共享" : "恢复共享"
        }

        var activePeers = peer.peers.filter(\.connected)
        if activePeers.isEmpty, peer.isConnected, let pid = peer.peerDeviceID {
            activePeers = [PeerInfo(
                id: pid,
                name: peerTag,
                platform: "windows",
                connected: true,
                focused: isRemoteFocus,
                slot: 1,
                address: peer.peerAddress,
                activeChannelLabel: peer.activeChannelLabel,
                preferredChannel: "auto"
            )]
        }

        quickJumpParentItem?.isHidden = !(peer.isSharing && !activePeers.isEmpty)
        screenLockMenuItem?.state = peer.screenLocked ? .on : .off
        clipboardMenuItem?.state = peer.clipboardSync ? .on : .off
        inputStateMenuItem?.state = peer.inputStateSync ? .on : .off
        sendFilesParentItem?.isEnabled = peer.isConnected && !activePeers.isEmpty
        autoStartMenuItem?.state = isLaunchAgentEnabled() ? .on : .off

        let peersSig = activePeers.map { "\($0.id):\($0.name):\($0.focused):\($0.slot ?? 0)" }.joined(separator: "|")
        if peersSig != lastPeersMenuSig {
            lastPeersMenuSig = peersSig
            if let qjSub = quickJumpSubmenu {
                qjSub.removeAllItems()
                let nextItem = NSMenuItem(title: "切到下一台设备", action: #selector(quickJumpNext), keyEquivalent: "")
                nextItem.target = self
                qjSub.addItem(nextItem)
                let homeItem = NSMenuItem(title: "回到本机", action: #selector(quickJumpHome), keyEquivalent: "")
                homeItem.target = self
                qjSub.addItem(homeItem)
                if !activePeers.isEmpty {
                    qjSub.addItem(.separator())
                    for (idx, p) in activePeers.enumerated() {
                        let slot = p.slot ?? (idx + 1)
                        let item = NSMenuItem(title: "#\(slot) \(p.name)", action: #selector(quickJumpToPeer(_:)), keyEquivalent: "")
                        item.target = self
                        item.representedObject = p.id
                        item.state = p.focused ? .on : .off
                        qjSub.addItem(item)
                    }
                }
            }
            if let sendSub = sendFilesSubmenu {
                sendSub.removeAllItems()
                for p in activePeers {
                    let item = NSMenuItem(title: "发送给 \(p.name)…", action: #selector(pickAndSendFilesToPeer(_:)), keyEquivalent: "")
                    item.target = self
                    item.representedObject = p.id
                    sendSub.addItem(item)
                }
                if activePeers.count > 1 {
                    sendSub.addItem(.separator())
                    let allItem = NSMenuItem(title: "发送给全部已连接设备…", action: #selector(pickAndSendFilesToPeer(_:)), keyEquivalent: "")
                    allItem.target = self
                    allItem.representedObject = nil
                    sendSub.addItem(allItem)
                }
            }
        }

        let sig = peer.selectedChannel
            + "|" + peer.networkChannels.map { "\($0.id):\($0.label)" }.joined(separator: ",")
            + "|" + activePeers.map { "\($0.id):\($0.preferredChannel):\($0.activeChannelLabel ?? "")" }.joined(separator: ";")
        if sig != lastChannelMenuSig, let sub = channelSubmenu, let parent = channelParentItem {
            lastChannelMenuSig = sig
            sub.removeAllItems()
            let hdr = NSMenuItem(title: "全局默认信道", action: nil, keyEquivalent: "")
            hdr.isEnabled = false
            sub.addItem(hdr)
            for ch in peer.networkChannels {
                let item = NSMenuItem(title: ch.label, action: #selector(selectChannel(_:)), keyEquivalent: "")
                item.target = self
                item.representedObject = ch.id
                item.state = (ch.id == peer.selectedChannel) ? .on : .off
                sub.addItem(item)
            }
            if !activePeers.isEmpty {
                sub.addItem(.separator())
                for p in activePeers {
                    let activeTag = p.activeChannelLabel.map { " (\($0))" } ?? ""
                    let pItem = NSMenuItem(title: "\(p.name)\(activeTag)", action: nil, keyEquivalent: "")
                    let pSub = NSMenu()
                    for (idx, ch) in peer.networkChannels.enumerated() {
                        let lbl = idx == 0 ? "跟随全局默认" : ch.label
                        let chItem = NSMenuItem(title: lbl, action: #selector(selectPeerChannel(_:)), keyEquivalent: "")
                        chItem.target = self
                        chItem.representedObject = [p.id, ch.id]
                        chItem.state = (ch.id == p.preferredChannel) ? .on : .off
                        pSub.addItem(chItem)
                    }
                    pItem.submenu = pSub
                    sub.addItem(pItem)
                }
            }
            parent.isHidden = peer.networkChannels.isEmpty
        }
    }

    private func renderMenuBarIcon(state: MenuBarVisualState) -> NSImage {
        let size = NSSize(width: 22, height: 18)
        let img = NSImage(size: size, flipped: false) { _ in
            let isDarkBar = NSAppearance.currentDrawing().bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            let glyphColor: NSColor = switch state {
            case .connected: isDarkBar ? .white : NSColor(red: 30/255, green: 41/255, blue: 59/255, alpha: 1)
            case .activeRemote: isDarkBar
                ? NSColor(red: 56/255, green: 189/255, blue: 248/255, alpha: 1)
                : NSColor(red: 37/255, green: 99/255, blue: 235/255, alpha: 1)
            case .searching, .paused:
                isDarkBar
                    ? NSColor(white: 1.0, alpha: 0.55)
                    : NSColor(red: 100/255, green: 116/255, blue: 139/255, alpha: 0.85)
            }

            let leftRect = NSRect(x: 1.5, y: 2.5, width: 7.8, height: 13.0)
            let rightRect = NSRect(x: 12.2, y: 2.5, width: 7.8, height: 13.0)
            let leftPath = NSBezierPath(roundedRect: leftRect, xRadius: 2.2, yRadius: 2.2)
            let rightPath = NSBezierPath(roundedRect: rightRect, xRadius: 2.2, yRadius: 2.2)
            leftPath.lineWidth = 1.75
            rightPath.lineWidth = 1.75
            glyphColor.setStroke()
            leftPath.stroke()
            rightPath.stroke()

            // Clear portal gap across inner borders for the bridging arrow
            NSGraphicsContext.current?.cgContext.setBlendMode(.clear)
            NSBezierPath(rect: NSRect(x: 7.2, y: 5.5, width: 7.2, height: 7.0)).fill()
            NSGraphicsContext.current?.cgContext.setBlendMode(.normal)

            // Crisp bridging arrow left -> right
            let arrow = NSBezierPath()
            arrow.lineWidth = 1.85
            arrow.lineCapStyle = .round
            arrow.lineJoinStyle = .round
            arrow.move(to: NSPoint(x: 5.2, y: 9.0))
            arrow.line(to: NSPoint(x: 15.6, y: 9.0))
            arrow.move(to: NSPoint(x: 12.7, y: 11.7))
            arrow.line(to: NSPoint(x: 15.8, y: 9.0))
            arrow.line(to: NSPoint(x: 12.7, y: 6.3))
            glyphColor.setStroke()
            arrow.stroke()

            if state == .connected {
                let cutout = NSBezierPath(ovalIn: NSRect(x: 15.0, y: 0.2, width: 6.6, height: 6.6))
                NSGraphicsContext.current?.cgContext.setBlendMode(.clear)
                cutout.fill()
                NSGraphicsContext.current?.cgContext.setBlendMode(.normal)
                NSColor(red: 22/255, green: 163/255, blue: 74/255, alpha: 1).setFill()
                NSBezierPath(ovalIn: NSRect(x: 15.9, y: 1.1, width: 4.8, height: 4.8)).fill()
            } else if state == .paused {
                let cutout = NSBezierPath(ovalIn: NSRect(x: 14.3, y: 0.0, width: 7.4, height: 7.4))
                NSGraphicsContext.current?.cgContext.setBlendMode(.clear)
                cutout.fill()
                NSGraphicsContext.current?.cgContext.setBlendMode(.normal)
                NSColor(red: 217/255, green: 119/255, blue: 6/255, alpha: 1).setFill()
                NSBezierPath(ovalIn: NSRect(x: 15.1, y: 0.8, width: 5.8, height: 5.8)).fill()
                NSColor.white.setFill()
                NSBezierPath(rect: NSRect(x: 16.8, y: 2.3, width: 0.9, height: 2.8)).fill()
                NSBezierPath(rect: NSRect(x: 18.3, y: 2.3, width: 0.9, height: 2.8)).fill()
            }
            return true
        }
        img.isTemplate = false
        return img
    }

    private func launchAgentURL() -> URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents/local.crossflow.mac.plist")
    }

    private func isLaunchAgentEnabled() -> Bool {
        FileManager.default.fileExists(atPath: launchAgentURL().path)
    }

    private func setLaunchAgentEnabled(_ enable: Bool) {
        let url = launchAgentURL()
        if !enable {
            try? FileManager.default.removeItem(at: url)
            return
        }
        guard let exe = Bundle.main.executableURL?.path else { return }
        try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let plist: [String: Any] = [
            "Label": "local.crossflow.mac",
            "ProgramArguments": [exe, "--minimized"],
            "RunAtLoad": true,
            "KeepAlive": false
        ]
        if let data = try? PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0) {
            try? data.write(to: url, options: .atomic)
        }
    }

    private func resolvedMainWindow() -> NSWindow? {
        if let win = mainWindow { return win }
        return NSApp.windows.first { $0.title == "Crossflow" }
    }

    func showMainWindow() {
        NSApp.activate(ignoringOtherApps: true)
        if let win = resolvedMainWindow() {
            win.makeKeyAndOrderFront(nil)
        }
        refreshMenuBar()
    }

    @objc private func toggleMainWindow() {
        if let win = resolvedMainWindow(), win.isVisible {
            hideMainWindow(win)
        } else {
            showMainWindow()
        }
    }

    @objc private func toggleSharing() {
        if peer.isSharing {
            peer.pause()
        } else if peer.state == "paused" {
            peer.resume()
        }
    }

    @objc private func toggleScreenLock() {
        peer.setScreenLock(!peer.screenLocked)
        refreshMenuBar()
    }

    @objc private func quickJumpNext() {
        peer.quickJump(to: nil)
    }

    @objc private func quickJumpHome() {
        peer.quickJump(to: "local")
    }

    @objc private func quickJumpToPeer(_ sender: NSMenuItem) {
        if let id = sender.representedObject as? String {
            peer.quickJump(to: id)
        }
    }

    @objc private func toggleClipboardSync() {
        peer.setClipboard(!peer.clipboardSync)
        refreshMenuBar()
    }

    @objc private func toggleInputStateSync() {
        peer.setInputStateSync(!peer.inputStateSync)
        refreshMenuBar()
    }

    @objc private func selectChannel(_ sender: NSMenuItem) {
        guard let chId = sender.representedObject as? String else { return }
        peer.setChannel(chId, directPeer: nil)
        refreshMenuBar()
    }

    @objc private func selectPeerChannel(_ sender: NSMenuItem) {
        guard let pair = sender.representedObject as? [String], pair.count == 2 else { return }
        peer.setPeerChannel(deviceID: pair[0], channel: pair[1])
        refreshMenuBar()
    }

    @objc private func pickAndSendFilesToPeer(_ sender: NSMenuItem) {
        guard peer.isConnected else { return }
        let targetID = sender.representedObject as? String
        NSApp.activate(ignoringOtherApps: true)
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = true
        panel.prompt = "发送"
        panel.title = "选择要发送的文件或文件夹"
        if panel.runModal() == .OK {
            let paths = panel.urls.map { (($0 as NSURL).filePathURL ?? $0.standardizedFileURL).path }
            if !paths.isEmpty {
                peer.sendFiles(paths, targetDeviceID: targetID)
            }
        }
    }

    @objc private func openReceiveFolder() {
        let dir = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Downloads/Crossflow", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        NSWorkspace.shared.open(dir)
    }

    @objc private func toggleAutoStart() {
        setLaunchAgentEnabled(!isLaunchAgentEnabled())
        refreshMenuBar()
    }

    @objc private func quitApp() {
        NSApp.terminate(nil)
    }
}

