// Run from the repository root after compiling with swiftc -O.
// Writes only runs/macos-query-latency-20261030.json; no app or input is started.
import Foundation
import CoreGraphics
import ApplicationServices

// Read-only timing: no CGEvent construction/posting, event taps, permission
// requests, Keychain calls, or changes to displays/system settings.
let sampleCount = 300
let warmups = 10
var ids = [CGDirectDisplayID](repeating: 0, count: 33)
var count: UInt32 = 0
let initialResult = CGGetActiveDisplayList(33, &ids, &count)
guard initialResult == .success, count > 0, count <= 32 else { fatalError("No bounded display set") }
let knownIDs = Array(ids.prefix(Int(count)))

@inline(never) func activeList() -> Double {
    var ids = [CGDirectDisplayID](repeating: 0, count: 33)
    var count: UInt32 = 0
    _ = CGGetActiveDisplayList(33, &ids, &count)
    return Double(count)
}
@inline(never) func perDisplayQueries() -> Double {
    var value = 0.0
    for id in knownIDs {
        value += Double(CGDisplayIsActive(id)) + Double(CGDisplayIsAsleep(id))
        let bounds = CGDisplayBounds(id)
        value += bounds.width + bounds.height
    }
    return value
}
@inline(never) func snapshotQueries() -> Double {
    var ids = [CGDirectDisplayID](repeating: 0, count: 33)
    var count: UInt32 = 0
    guard CGGetActiveDisplayList(33, &ids, &count) == .success, count <= 32 else { return -1 }
    var value = 0.0
    for id in ids.prefix(Int(count)) {
        guard CGDisplayIsActive(id) != 0 else { return -1 }
        if CGDisplayIsAsleep(id) != 0 { continue }
        let bounds = CGDisplayBounds(id)
        value += bounds.width + bounds.height
    }
    return value
}
@inline(never) func preflightTriplet() -> Double {
    // Evaluate all three independently, even if this CLI host lacks access.
    let ax = AXIsProcessTrusted()
    let post = CGPreflightPostEventAccess()
    let listen = CGPreflightListenEventAccess()
    return (ax ? 1 : 0) + (post ? 1 : 0) + (listen ? 1 : 0)
}
let operations: [(String, () -> Double)] = [
    ("AXIsProcessTrusted", { AXIsProcessTrusted() ? 1 : 0 }),
    ("CGPreflightPostEventAccess", { CGPreflightPostEventAccess() ? 1 : 0 }),
    ("CGPreflightListenEventAccess", { CGPreflightListenEventAccess() ? 1 : 0 }),
    ("CGGetActiveDisplayList", activeList),
    ("active_asleep_bounds_known_displays", perDisplayQueries),
    ("display_snapshot_query_subset", snapshotQueries),
    ("permission_triplet", preflightTriplet),
    ("permission_plus_snapshot_queries", { preflightTriplet() + snapshotQueries() })
]
var samples = operations.map { _ in [Double]() }
var firstCalls = [Double]()
var checksum = 0.0
func elapsed(_ work: () -> Double) -> Double {
    let start = DispatchTime.now().uptimeNanoseconds
    checksum += work()
    return Double(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000
}
for (_, operation) in operations { firstCalls.append(elapsed(operation)) }
for _ in 0..<warmups { for (_, operation) in operations { _ = elapsed(operation) } }
let began = DispatchTime.now().uptimeNanoseconds
for round in 0..<sampleCount {
    // Rotate start position to reduce systematic temporal order bias.
    for step in 0..<operations.count {
        let index = (round + step) % operations.count
        samples[index].append(elapsed(operations[index].1))
    }
}
func stats(_ values: [Double]) -> [String: Any] {
    let sorted = values.sorted()
    func percentile(_ p: Double) -> Double { sorted[max(0, Int(ceil(p * Double(sorted.count))) - 1)] }
    return ["count": sorted.count, "p50_ms": percentile(0.5), "p95_ms": percentile(0.95),
            "p99_ms": percentile(0.99), "max_ms": sorted.last!, "mean_ms": sorted.reduce(0,+) / Double(sorted.count)]
}
var measurements = [String: Any]()
for (index, pair) in operations.enumerated() {
    var result = stats(samples[index]); result["first_call_ms"] = firstCalls[index]
    measurements[pair.0] = result
}
let report: [String: Any] = [
    "schema": 1, "scope": "CLI-host read-only system-query microbenchmark, not app input-worker context",
    "samples_per_operation": sampleCount, "warmups_per_operation": warmups,
    "order": "round-robin with rotating first operation", "display_count": knownIDs.count,
    "system_input_injected": false, "permission_requests": false, "settings_changed": false,
    "elapsed_sampling_ms": Double(DispatchTime.now().uptimeNanoseconds - began) / 1_000_000,
    "host_permissions": ["accessibility": AXIsProcessTrusted(), "post_event": CGPreflightPostEventAccess(), "listen_event": CGPreflightListenEventAccess()],
    "measurements": measurements, "checksum_finite": checksum.isFinite,
    "excludes": ["CGEventPost", "event allocation", "IPC", "worker scheduling", "display reconfiguration callback", "real input"]
]
let data = try JSONSerialization.data(withJSONObject: report, options: [.prettyPrinted, .sortedKeys])
try data.write(to: URL(fileURLWithPath: "runs/macos-query-latency-20261030.json"))
for (name, result) in measurements.sorted(by: { $0.key < $1.key }) {
    let values = result as! [String: Any]
    print("\(name): p50=\(values["p50_ms"]!)ms p99=\(values["p99_ms"]!)ms max=\(values["max_ms"]!)ms")
}
