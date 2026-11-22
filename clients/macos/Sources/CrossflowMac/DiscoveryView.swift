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

@MainActor
final class DiscoveryAppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    let discovery = DiscoveryModel()
    let devices = DeviceModel()
    let peer = PeerMeshModel()
    private var awaitingShutdown = false
    private var statusItem: NSStatusItem?
    private var statusMenuItem: NSMenuItem?
    private var showHideMenuItem: NSMenuItem?
    private var toggleShareMenuItem: NSMenuItem?
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
            refreshMenuBar()
        }
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        if awaitingShutdown {
            return true
        }
        sender.orderOut(nil)
        refreshMenuBar()
        return false
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
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        if let button = item.button {
            if let img = NSImage(systemSymbolName: "display.2", accessibilityDescription: "Crossflow") {
                img.isTemplate = true
                button.image = img
            } else {
                button.title = "CF"
            }
            button.toolTip = "Crossflow"
        }

        let menu = NSMenu()
        let statusEntry = NSMenuItem(title: "状态：正在发现设备", action: nil, keyEquivalent: "")
        statusEntry.isEnabled = false
        menu.addItem(statusEntry)
        menu.addItem(.separator())

        let showHideEntry = NSMenuItem(title: "隐藏到菜单栏", action: #selector(toggleMainWindow), keyEquivalent: "")
        showHideEntry.target = self
        menu.addItem(showHideEntry)

        let toggleShareEntry = NSMenuItem(title: "暂停共享", action: #selector(toggleSharing), keyEquivalent: "")
        toggleShareEntry.target = self
        toggleShareEntry.isHidden = true
        menu.addItem(toggleShareEntry)

        menu.addItem(.separator())
        let quitEntry = NSMenuItem(title: "退出 Crossflow", action: #selector(quitApp), keyEquivalent: "q")
        quitEntry.target = self
        menu.addItem(quitEntry)

        item.menu = menu
        statusItem = item
        statusMenuItem = statusEntry
        showHideMenuItem = showHideEntry
        toggleShareMenuItem = toggleShareEntry
    }

    func refreshMenuBar() {
        statusMenuItem?.title = "状态：\(peer.statusTitle)"
        if let button = statusItem?.button {
            if peer.isSharing, let name = peer.peerName {
                button.toolTip = "Crossflow · 已连接 \(name)"
            } else {
                button.toolTip = "Crossflow · \(peer.statusTitle)"
            }
        }
        let visible = resolvedMainWindow()?.isVisible ?? false
        showHideMenuItem?.title = visible ? "隐藏到菜单栏" : "显示主界面"
        if let toggleItem = toggleShareMenuItem {
            toggleItem.isHidden = !peer.isConnected
            toggleItem.title = peer.isSharing ? "暂停共享" : "恢复共享"
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
            win.orderOut(nil)
            refreshMenuBar()
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

    @objc private func quitApp() {
        NSApp.terminate(nil)
    }
}

