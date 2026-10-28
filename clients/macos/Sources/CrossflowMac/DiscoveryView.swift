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
    private var awaitingShutdown = false
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard !awaitingShutdown else { return .terminateLater }
        awaitingShutdown = true
        Task {
            let devicesStopped = await devices.shutdown()
            let discoveryStopped = await discovery.shutdown()
            awaitingShutdown = false
            sender.reply(toApplicationShouldTerminate: devicesStopped && discoveryStopped)
        }
        return .terminateLater
    }
}

struct DiscoveryView: View {
    @Bindable var model: DiscoveryModel
    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            HStack {
                VStack(alignment: .leading, spacing: 7) {
                    Text("附近设备").font(.system(size: Theme.titleSize, weight: .semibold))
                    Text("在同一个局域网中查找 Crossflow 设备。")
                        .foregroundStyle(Theme.secondary)
                }
                Spacer()
                Button("刷新", systemImage: "arrow.clockwise", action: refresh)
                    .disabled(model.scanning).accessibilityIdentifier("discovery-refresh")
            }
            HStack(spacing: 10) {
                if model.scanning { ProgressView().controlSize(.small) }
                Text(model.message).foregroundStyle(Theme.secondary)
                Spacer()
                if model.scanning || model.cleanupFailed {
                    Button("停止扫描", action: stop).buttonStyle(.borderless)
                }
            }
            ScrollView {
                LazyVStack(spacing: 12) {
                    ForEach(model.devices) { device in NearbyDeviceRow(device: device) }
                    if !model.scanning && model.devices.isEmpty {
                        VStack(spacing: 12) {
                            Image(systemName: "desktopcomputer").font(.system(size: 36)).foregroundStyle(Theme.secondary)
                            Text("让另一台设备也加入局域网")
                                .font(.system(size: 16, weight: .semibold))
                            Text("对方需要启用 Crossflow 设备发现。刷新后，这里会显示真实发现结果。")
                                .foregroundStyle(Theme.secondary).multilineTextAlignment(.center)
                        }.frame(maxWidth: .infinity).padding(.vertical, 64)
                    }
                }
            }
            Text("发现不代表已配对。设备配对尚未接入，当前不能通过此页开始共享。")
                .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
        }
        .padding(28).frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.background)
        .task { model.start() }
        .onDisappear { model.cancel() }
    }
    private func refresh() { model.start() }
    private func stop() { Task { _ = await model.shutdown() } }
}

private struct NearbyDeviceRow: View {
    let device: NearbyDevice
    private var platform: String {
        switch device.platform {
        case "windows": "Windows"
        case "macos": "macOS"
        case "linux": "Linux"
        case "ios": "iOS"
        case "ipados": "iPadOS"
        default: "设备"
        }
    }
    var body: some View {
        HStack(spacing: 16) {
            Image(systemName: "desktopcomputer").font(.system(size: 26)).foregroundStyle(Theme.accent)
            VStack(alignment: .leading, spacing: 5) {
                Text(device.name).fontWeight(.semibold).lineLimit(1)
                Text("\(platform) · 未配对\(device.compatible ? "" : " · 版本不兼容")")
                    .font(.system(size: Theme.captionSize)).foregroundStyle(Theme.secondary)
            }
            Spacer()
            Button("配对（尚未支持）") {}.disabled(true)
                .help("配对流程尚未实现；发现不会授权远程输入。")
        }
        .padding(18).background(Theme.surface, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.border, lineWidth: 1))
        .accessibilityElement(children: .contain)
    }
}
