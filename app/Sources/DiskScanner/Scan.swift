import DiskScannerFFI
import Foundation
import Observation
import SwiftUI

// MARK: - C callback plumbing
//
// `@convention(c)` functions cannot capture, so the Swift closures travel
// through the `ctx` pointer as a retained box. The box is released exactly
// once, in the completion thunk.

/// Ownership of a finished `DsScan *` moving from the scan thread to the
/// main actor. Unchecked because the compiler cannot see the invariant the
/// Rust side guarantees: the pointer is created on the scan thread, handed
/// over exactly once in the completion callback, and never touched there
/// again. From that moment it is owned solely by `ScanModel`.
struct ScanHandle: @unchecked Sendable {
    let p: OpaquePointer
}

private final class Callbacks: @unchecked Sendable {
    let progress: @Sendable (UInt64, UInt64, UInt64) -> Void
    let done: @Sendable (ScanHandle?, String?) -> Void
    init(
        progress: @escaping @Sendable (UInt64, UInt64, UInt64) -> Void,
        done: @escaping @Sendable (ScanHandle?, String?) -> Void
    ) {
        self.progress = progress
        self.done = done
    }
}

private let progressThunk: @convention(c) (UnsafeMutableRawPointer?, UInt64, UInt64, UInt64) ->
    Void = { ctx, dirs, files, physical in
        guard let ctx else { return }
        Unmanaged<Callbacks>.fromOpaque(ctx).takeUnretainedValue()
            .progress(dirs, files, physical)
    }

private let doneThunk: @convention(c) (UnsafeMutableRawPointer?, OpaquePointer?, UnsafePointer<CChar>?) ->
    Void = { ctx, scan, err in
        guard let ctx else { return }
        // takeRetainedValue balances the passRetained in `Scanner.start`.
        let cb = Unmanaged<Callbacks>.fromOpaque(ctx).takeRetainedValue()
        cb.done(scan.map(ScanHandle.init), err.map { String(cString: $0) })
    }

// MARK: - Sort keys

enum SortKey: Int32, CaseIterable, Identifiable {
    case physical = 0, exclusive = 1, logical = 2, name = 3, mtime = 4
    var id: Int32 { rawValue }
    var label: String {
        switch self {
        case .physical: "Allocated"
        case .exclusive: "Yours"
        case .logical: "Apparent"
        case .name: "Name"
        case .mtime: "Modified"
        }
    }
}

// MARK: - A finished scan
//
// Wraps the opaque `DsScan *`. Every accessor is a narrow question answered
// into a caller-allocated buffer; the 4.2M-node arena never crosses.

final class Scan {
    fileprivate let handle: OpaquePointer
    let summary: DsSummary
    let snapshotNote: String
    let reconcileNote: String
    let skippedMounts: [(path: String, reason: String)]
    private var reclaimCache: [UInt32: DsReclaim] = [:]

    init(_ handle: OpaquePointer) {
        let t = Date()
        defer { Diag.slow("Scan.init", t) }
        self.handle = handle
        var s = DsSummary()
        ds_summary(handle, &s)
        summary = s
        snapshotNote = cString(2048) { ds_snapshot_hint(handle, $0, $1) }
        reconcileNote = cString(2048) { ds_reconcile_text(handle, $0, $1) }
        skippedMounts = (0..<ds_skipped_count(handle)).map { i in
            var p = [CChar](repeating: 0, count: 1024)
            var r = [CChar](repeating: 0, count: 256)
            ds_skipped(handle, i, &p, 1024, &r, 256)
            return (String(cString: p), String(cString: r))
        }
    }

    deinit { ds_scan_free(handle) }

    var root: UInt32 { ds_root() }

    func row(_ node: UInt32) -> DsRow? {
        var r = DsRow()
        return ds_row(handle, node, &r) ? r : nil
    }

    func name(_ node: UInt32) -> String {
        cString(512) { ds_name(handle, node, $0, $1) }
    }

    /// The name to show. The scan root's "name" is its whole path, which is
    /// the right thing in a breadcrumb and far too long in a list row.
    func displayName(_ node: UInt32) -> String {
        let n = name(node)
        if node == root, let last = n.split(separator: "/").last { return String(last) }
        return n.isEmpty ? "/" : n
    }

    func path(_ node: UInt32) -> String {
        cString(1024) { ds_path(handle, node, $0, $1) }
    }

    func parent(_ node: UInt32) -> UInt32? {
        let p = ds_parent(handle, node)
        return p == DS_NO_NODE || p == node ? nil : p
    }

    /// Ancestor chain from the root down to `node`, for the breadcrumb.
    func ancestry(_ node: UInt32) -> [UInt32] {
        var chain = [node]
        var cur = node
        while let p = parent(cur), chain.count < 128 {
            chain.append(p)
            cur = p
        }
        return chain.reversed()
    }

    func children(of node: UInt32, sort: SortKey = .physical, limit: Int = 500) -> [DsRow] {
        var rows = [DsRow](repeating: DsRow(), count: limit)
        let n = rows.withUnsafeMutableBufferPointer {
            ds_children(handle, node, sort.rawValue, true, 0, $0.baseAddress, limit)
        }
        return Array(rows.prefix(n))
    }

    /// Recursive squarified layout of the subtree under `node`, computed in
    /// Rust and returned in paint order (containers before their children).
    func treemap(
        of node: UInt32, size: CGSize, minPixels: CGFloat = 4, maxDepth: UInt32 = 6,
        maxGap: CGFloat = 3, cap: Int = 24000
    ) -> [DsRect] {
        guard size.width > 1, size.height > 1 else { return [] }
        var rects = [DsRect](repeating: DsRect(), count: cap)
        let n = rects.withUnsafeMutableBufferPointer {
            ds_treemap(
                handle, node, Float(size.width), Float(size.height), Float(minPixels),
                maxDepth, Float(maxGap), $0.baseAddress, cap)
        }
        return Array(rects.prefix(n))
    }

    /// What deleting one subtree would free.
    ///
    /// Memoised, because the tree is immutable and this is read from a view
    /// body: the inspector re-evaluates on every selection change, and its
    /// target is usually the scan root, so an uncached call re-walked millions
    /// of nodes each time something was ticked.
    func subtreeReclaim(_ node: UInt32) -> DsReclaim {
        if let hit = reclaimCache[node] { return hit }
        let r = reclaim([node])
        reclaimCache[node] = r
        return r
    }

    /// What deleting this whole selection would actually free.
    func reclaim(_ nodes: some Collection<UInt32>) -> DsReclaim {
        var out = DsReclaim()
        let ids = Array(nodes)
        if ids.isEmpty {
            return out
        }
        ids.withUnsafeBufferPointer { ds_reclaim(handle, $0.baseAddress, ids.count, &out) }
        return out
    }
}

// MARK: - Driving a scan

@MainActor
@Observable
final class ScanModel {
    enum Phase: Equatable {
        case idle
        case scanning(dirs: UInt64, files: UInt64, physical: UInt64)
        case ready
        case failed(String)
    }

    var phase: Phase = .idle
    var scan: Scan?
    var rootPath = "/"
    /// The path shown in the start field, held here so the window toolbar and
    /// the start panel operate on the same value.
    var pendingPath = NSHomeDirectory()

    /// The directory the treemap is showing.
    var cursor: UInt32 = 0
    /// The block the pointer is over, or the last one clicked.
    var focus: UInt32?
    /// The selection basket. Reclaim is a property of this set, never a sum.
    var selection: Set<UInt32> = []
    var sort: SortKey = .physical
    var showInspector = true

    private var job: OpaquePointer?
    private var generation = 0
    /// Counted so a frozen-looking scan can be told from a silent one.
    private var progressTicks = 0

    var fullDiskAccess: Int32 { ds_full_disk_access() }
    var responsibleApp: String { cString(1024) { ds_responsible_app_hint($0, $1) } }

    /// Reclaim over the current selection.
    ///
    /// One-entry memo: SwiftUI may evaluate the selection bar several times
    /// per update, and this walks every selected subtree. `@ObservationIgnored`
    /// so caching inside a getter does not register as a change.
    @ObservationIgnored private var reclaimMemo: (sel: Set<UInt32>, value: DsReclaim)?

    var reclaim: DsReclaim {
        guard let scan else { return DsReclaim() }
        if let m = reclaimMemo, m.sel == selection { return m.value }
        let r = scan.reclaim(selection)
        reclaimMemo = (selection, r)
        return r
    }

    func start(path: String) {
        guard case .scanning = phase else {
            beginScan(path)
            return
        }
    }

    private func beginScan(_ path: String) {
        cancel()
        scan = nil
        selection = []
        focus = nil
        rootPath = path
        phase = .scanning(dirs: 0, files: 0, physical: 0)

        generation &+= 1
        let gen = generation

        Diag.mark("scan begin \(path)")
        progressTicks = 0
        let cb = Callbacks(
            progress: { [weak self] d, f, p in
                Task { @MainActor in
                    guard let self, case .scanning = self.phase else { return }
                    self.progressTicks += 1
                    self.phase = .scanning(dirs: d, files: f, physical: p)
                }
            },
            done: { [weak self] handle, err in
                Task { @MainActor in
                    guard let self else {
                        // The window closed mid-scan; nobody will own this.
                        if let handle { ds_scan_free(handle.p) }
                        return
                    }
                    self.finish(handle, err, gen)
                }
            })

        let ctx = Unmanaged.passRetained(cb).toOpaque()
        job = path.withCString {
            ds_scan_begin($0, 0, progressThunk, doneThunk, ctx)
        }
        if job == nil {
            Unmanaged<Callbacks>.fromOpaque(ctx).release()
            phase = .failed("could not start a scan of \(path)")
        }
    }

    private func finish(_ handle: ScanHandle?, _ err: String?, _ gen: Int) {
        Diag.mark("walk done; \(progressTicks) progress updates reached the UI")
        guard gen == generation else {
            // Superseded by a newer scan, or abandoned via `newScan`.
            if let handle { ds_scan_free(handle.p) }
            return
        }
        if let j = job {
            ds_job_free(j)
            job = nil
        }
        guard let handle else {
            phase = .failed(err ?? "scan failed")
            return
        }
        let s = Scan(handle.p)
        scan = s
        Diag.mark("phase -> ready")
        cursor = s.root
        focus = nil
        phase = .ready
    }

    func cancel() {
        if let j = job { ds_scan_cancel(j) }
    }

    /// Back to the start screen, releasing the tree.
    ///
    /// Bumping the generation is what makes this safe mid-scan: the worker
    /// still finishes and still calls back, and `finish` then drops the result
    /// instead of dragging the user into a scan they walked away from.
    func newScan() {
        cancel()
        generation &+= 1
        scan = nil
        selection = []
        focus = nil
        cursor = 0
        phase = .idle
    }

    var canRescan: Bool {
        if case .scanning = phase { return false }
        return scan != nil
    }

    // MARK: navigation

    func descend(_ node: UInt32) {
        guard let s = scan, let r = s.row(node), r.kind == DS_KIND_DIR, r.child_count > 0
        else { return }
        cursor = node
        focus = nil
    }

    func ascend() {
        guard let s = scan, let p = s.parent(cursor) else { return }
        cursor = p
        focus = nil
    }

    /// The animation for a selection change, defined once and applied at the
    /// mutation rather than on a view.
    ///
    /// An implicit `.animation(value:)` only covers the subtree it is attached
    /// to, and the map and the footer are siblings — so whichever one carried
    /// the modifier eased while the other snapped. Wrapping the mutation puts
    /// the whole transaction in scope, and keeping it here is what stops the
    /// three call sites (map, inspector checkbox, Clear) from each inventing
    /// their own curve, which is what made the easing look inconsistent.
    static let selectionChange: Animation = .snappy(duration: 0.24)

    func toggle(_ node: UInt32) {
        withAnimation(Self.selectionChange) {
            if selection.contains(node) {
                selection.remove(node)
            } else {
                selection.insert(node)
            }
        }
    }

    func clearSelection() {
        withAnimation(Self.selectionChange) { selection = [] }
    }
}
