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
final class DiscoveryAppDelegate: NSObject, NSApplicationDelegate {
    let discovery = DiscoveryModel()
    let devices = DeviceModel()
    let peer = PeerMeshModel()
    private var awaitingShutdown = false
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard !awaitingShutdown else { return .terminateLater }
        awaitingShutdown = true
        Task {
            let peerStopped = await peer.shutdown()
            let devicesStopped = await devices.shutdown()
            let discoveryStopped = await discovery.shutdown()
            awaitingShutdown = false
            sender.reply(toApplicationShouldTerminate: peerStopped && devicesStopped && discoveryStopped)
        }
        return .terminateLater
    }
}

