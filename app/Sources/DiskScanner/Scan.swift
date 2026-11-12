import AppKit
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

/// A prepared refresh crossing from its worker back to the main actor, owned
/// by exactly one side at a time — the same shape of promise as `ScanHandle`.
struct PatchHandle: @unchecked Sendable {
    let p: OpaquePointer
    func discard() { ds_patch_free(p) }
}

/// Lets a refresh worker keep the scan alive while it reads. Unchecked: the
/// worker only calls `prepare`, which only reads, and `ScanModel` never
/// applies while a worker is running.
private struct ScanRef: @unchecked Sendable {
    let scan: Scan
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
//
// The tree changes only through `apply`, on the main actor. Node ids do not
// survive it; `ScanModel` re-resolves the ones it holds by path.

final class Scan {
    fileprivate let handle: OpaquePointer
    private(set) var summary = DsSummary()
    private(set) var snapshotNote = ""
    private(set) var reconcileNote = ""
    /// Folders re-listed since the walk. Non-zero means clone figures
    /// outside them may be stale.
    private(set) var refreshes: UInt32 = 0
    let skippedMounts: [(path: String, reason: String)]
    private var reclaimCache: [UInt32: DsReclaim] = [:]

    init(_ handle: OpaquePointer) {
        let t = Date()
        defer { Diag.slow("Scan.init", t) }
        self.handle = handle
        skippedMounts = (0..<ds_skipped_count(handle)).map { i in
            var p = [CChar](repeating: 0, count: 1024)
            var r = [CChar](repeating: 0, count: 256)
            ds_skipped(handle, i, &p, 1024, &r, 256)
            return (decodeCString(p), decodeCString(r))
        }
        reloadDerived()
    }

    /// Everything read once and cached, re-read after the tree changes.
    private func reloadDerived() {
        var s = DsSummary()
        ds_summary(handle, &s)
        summary = s
        snapshotNote = cString(2048) { ds_snapshot_hint(handle, $0, $1) }
        reconcileNote = cString(2048) { ds_reconcile_text(handle, $0, $1) }
        refreshes = ds_refresh_count(handle)
        reclaimCache = [:]
    }

    // MARK: refresh

    /// The node at `path`; with `nearest`, the deepest one on the way there.
    func lookup(_ path: String, nearest: Bool = false) -> UInt32? {
        let n = path.withCString { ds_lookup(handle, $0, nearest) }
        return n == DS_NO_NODE ? nil : n
    }

    /// Re-list one folder. Blocking I/O that only reads the tree: call it off
    /// the main actor, and never while an `apply` could run. nil if the scan
    /// does not hold the folder.
    func prepare(_ dir: String) -> PatchHandle? {
        dir.withCString { ds_refresh_prepare(handle, $0) }.map(PatchHandle.init)
    }

    /// Graft a prepared patch. Invalidates node ids when it returns true.
    func apply(_ patch: PatchHandle) -> Bool {
        let ok = ds_refresh_apply(handle, patch.p)
        if ok { reloadDerived() }
        return ok
    }

    /// Whether `prepare` would re-list `dir`. Cheap.
    func holds(_ dir: String) -> Bool {
        dir.withCString { ds_refresh_holds(handle, $0) }
    }

    /// Folders an apply found stale but could not re-list itself.
    func takeFollowups() -> [String] {
        var out: [String] = []
        while true {
            var buf = [CChar](repeating: 0, count: 1024)
            var n = ds_refresh_followup(handle, &buf, buf.count)
            if n >= buf.count {
                buf = [CChar](repeating: 0, count: n + 1)
                n = ds_refresh_followup(handle, &buf, buf.count)
            }
            if n == 0 { return out }
            out.append(decodeCString(buf))
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
        // Discarding the Bool: it only reports a null handle, which cannot
        // happen here, and `out` is already zeroed.
        _ = ids.withUnsafeBufferPointer { ds_reclaim(handle, $0.baseAddress, ids.count, &out) }
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
    /// Pointed at from somewhere else in the UI — an inspector row being
    /// hovered — so the map can show which block it means. The map's own
    /// hover stays local to the map, where it repaints nothing else.
    var highlighted: UInt32?
    /// The selection basket. Reclaim is a property of this set, never a sum.
    var selection: Set<UInt32> = []
    var sort: SortKey = .physical
    var showInspector = true
    /// Bumped whenever the tree changes under the views. `Scan` is not
    /// observable, so anything that draws from it reads this to redraw.
    private(set) var revision = 0
    /// Set when change events were lost, so the live figures can no longer be
    /// trusted to be complete. Cleared by a full scan.
    private(set) var missedChanges = false

    private var job: OpaquePointer?
    private var generation = 0
    @ObservationIgnored private var watcher: FolderWatcher?
    /// Folders waiting to be re-listed, bucketed by depth. Collected while a
    /// scan or a refresh is running and drained one at a time after,
    /// shallowest first. Buckets make "shallowest" cheap: a deleted tree can
    /// queue tens of thousands of folders, and scanning them all on every
    /// pick was quadratic.
    @ObservationIgnored private var dirty = DirtyQueue()
    @ObservationIgnored private var refreshing = false
    /// A refresh that finished while a menu was open, applied when it closes.
    /// Applying under an open context menu rebuilds it against renumbered
    /// nodes and nothing hovered, which empties or dismisses it.
    @ObservationIgnored private var held: (patch: PatchHandle, scan: Scan, gen: Int)?
    @ObservationIgnored private var menusOpen = 0
    /// Counted so a frozen-looking scan can be told from a silent one.
    private var progressTicks = 0

    init() {
        // Every menu, not just ours: the menu bar renumbering under itself is
        // harmless, but telling ours apart is not worth the plumbing.
        let nc = NotificationCenter.default
        nc.addObserver(forName: NSMenu.didBeginTrackingNotification, object: nil, queue: .main) {
            [weak self] _ in MainActor.assumeIsolated { self?.menusOpen += 1 }
        }
        nc.addObserver(forName: NSMenu.didEndTrackingNotification, object: nil, queue: .main) {
            [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.menusOpen = max(0, self.menusOpen - 1)
                if self.menusOpen == 0, let h = self.held {
                    self.held = nil
                    self.finishRefresh(h.patch, h.scan, h.gen)
                }
            }
        }
    }

    var fullDiskAccess: Int32 { ds_full_disk_access() }
    var responsibleApp: String { cString(1024) { ds_responsible_app_hint($0, $1) } }

    /// Reclaim over the current selection.
    ///
    /// One-entry memo: SwiftUI may evaluate the selection bar several times
    /// per update, and this walks every selected subtree. `@ObservationIgnored`
    /// so caching inside a getter does not register as a change.
    @ObservationIgnored private var reclaimMemo: (sel: Set<UInt32>, value: DsReclaim)?

    var reclaim: DsReclaim {
        // Read so the footer redraws when a refresh changes sizes under an
        // unchanged selection.
        _ = revision
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

        // Before the walk, so nothing that changes during it slips by.
        stopWatching()
        missedChanges = false
        watcher = FolderWatcher(root: path) { [weak self] dirs, lost in
            self?.noteChanges(dirs, lost: lost)
        }

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
            stopWatching()
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
            // Nothing to keep current, and `pumpRefresh` would never drain.
            stopWatching()
            phase = .failed(err ?? "scan failed")
            return
        }
        let s = Scan(handle.p)
        scan = s
        // A stopped scan has folders it never opened; re-listing one would
        // fill it in while the rest stay empty. Nothing sound to keep current.
        if s.summary.cancelled != 0 { stopWatching() }
        Diag.mark("phase -> ready")
        cursor = s.root
        focus = nil
        phase = .ready
        pumpRefresh()
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
        stopWatching()
        generation &+= 1
        scan = nil
        selection = []
        focus = nil
        cursor = 0
        phase = .idle
    }

    // MARK: live updates

    /// The node at `path` now. For anything that captured a node earlier and
    /// acts later — an open context menu — across a refresh that may have
    /// renumbered everything in between.
    func node(at path: String) -> UInt32? {
        scan?.lookup(path)
    }

    private func stopWatching() {
        watcher?.stop()
        watcher = nil
        dirty = DirtyQueue()
        if let h = held {
            held = nil
            h.patch.discard()
            refreshing = false
        }
    }

    private func noteChanges(_ dirs: Set<String>, lost: Bool) {
        if lost { missedChanges = true }
        dirty.insert(dirs)
        pumpRefresh()
    }

    /// Re-list whatever changed, one folder in flight at a time.
    ///
    /// One at a time is what makes this safe without locks: `prepare` reads
    /// the tree on a worker while views read it here, and `apply` — the only
    /// writer — runs here only after that worker is done. It is also what
    /// makes it correct: each folder is prepared against the tree the last
    /// apply left (see `refresh::Patch`).
    ///
    /// Shallowest first, so a parent that walks a new subfolder in full goes
    /// before that subfolder's own event, which then costs only a re-list.
    private func pumpRefresh() {
        guard phase == .ready, let scan, !refreshing else { return }
        // Folders already gone are dropped here, without a trip to a worker:
        // their parent's re-listing is what removes them.
        var picked: String?
        while let d = dirty.popShallowest() {
            if scan.holds(d) {
                picked = d
                break
            }
        }
        guard let next = picked else { return }
        refreshing = true
        let gen = generation
        let ref = ScanRef(scan: scan)
        Diag.mark("refresh \(next)")
        Task.detached(priority: .utility) { [weak self] in
            let patch = ref.scan.prepare(next)
            await MainActor.run {
                guard let self else {
                    patch?.discard()
                    return
                }
                self.finishRefresh(patch, ref.scan, gen)
            }
        }
    }

    private func finishRefresh(_ patch: PatchHandle?, _ target: Scan, _ gen: Int) {
        guard let patch else {
            refreshing = false
            pumpRefresh()
            return
        }
        guard gen == generation, let s = scan, s === target else {
            refreshing = false
            patch.discard()
            pumpRefresh()
            return
        }
        // Still `refreshing` while held, so nothing else is prepared against
        // the tree this patch was built from.
        if menusOpen > 0 {
            held = (patch, target, gen)
            return
        }
        refreshing = false
        defer { pumpRefresh() }
        // Ids are about to be invalidated: remember what they pointed at.
        let cursorPath = s.path(cursor)
        let focusPath = focus.map(s.path)
        let highlightPath = highlighted.map(s.path)
        let picked = selection.map(s.path)

        let t = Date()
        let changed = s.apply(patch)
        Diag.slow("refresh apply", t)
        Diag.mark("refreshed: \(changed); root now \(s.summary.root.physical.formattedBytes)")
        guard changed else { return }
        dirty.insert(s.takeFollowups())

        cursor = s.lookup(cursorPath, nearest: true) ?? s.root
        focus = focusPath.flatMap { s.lookup($0) }
        highlighted = highlightPath.flatMap { s.lookup($0) }
        selection = Set(picked.compactMap { s.lookup($0) })
        reclaimMemo = nil
        revision &+= 1
    }

    var canRescan: Bool {
        if case .scanning = phase { return false }
        return scan != nil
    }

    // MARK: navigation

    /// Navigate into `node`. Returns false when there is nowhere to go —
    /// a file, or an empty or unreadable folder.
    @discardableResult
    func descend(_ node: UInt32) -> Bool {
        guard let s = scan, let r = s.row(node), r.kind == DS_KIND_DIR, r.child_count > 0
        else { return false }
        cursor = node
        focus = nil
        return true
    }

    /// What a click means, wherever it happens: go there if it is somewhere
    /// you can go, otherwise show it. Shared by the map and the inspector list
    /// so a click means the same thing in both.
    func open(_ node: UInt32) {
        if !descend(node) { focus = node }
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

/// Folders to re-list, deduplicated and bucketed by depth.
struct DirtyQueue {
    private var byDepth: [Int: Set<String>] = [:]

    mutating func insert(_ dirs: some Sequence<String>) {
        for d in dirs {
            byDepth[d.utf8.reduce(0) { $1 == UInt8(ascii: "/") ? $0 + 1 : $0 }, default: []]
                .insert(d)
        }
    }

    mutating func popShallowest() -> String? {
        guard let depth = byDepth.keys.min() else { return nil }
        let d = byDepth[depth]!.removeFirst()
        if byDepth[depth]!.isEmpty { byDepth[depth] = nil }
        return d
    }
}
