import CoreServices
import Foundation

/// Hears about changes under the scan root, as the folders that need
/// re-listing.
///
/// Per-file events, because a folder-level stream names the folder whose
/// listing changed only for creations; a rename into the Trash would surface
/// as "something under ~ changed". The folder to re-list is always the
/// parent of the path an event names — a removed, added, renamed or rewritten
/// item all change exactly that listing.
///
/// Started when a scan begins, not when it ends: anything that changes while
/// the walk is running may or may not have been seen, and re-listing it once
/// the scan lands is right either way.
@MainActor
final class FolderWatcher {
    /// Folders to re-list, and whether events were lost (the kernel or the
    /// daemon dropped some, so a refresh cannot be trusted to be complete).
    typealias Handler = @MainActor (_ dirs: Set<String>, _ lost: Bool) -> Void

    private var stream: FSEventStreamRef?
    private let root: String
    private let handler: Handler

    init?(root: String, handler: @escaping Handler) {
        self.root = (root as NSString).standardizingPath
        self.handler = handler

        var ctx = FSEventStreamContext(
            version: 0, info: Unmanaged.passUnretained(self).toOpaque(),
            retain: nil, release: nil, copyDescription: nil)
        let flags = UInt32(
            kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagUseCFTypes
                | kFSEventStreamCreateFlagWatchRoot)
        guard
            let s = FSEventStreamCreate(
                nil, eventThunk, &ctx, [self.root] as CFArray,
                FSEventStreamEventId(kFSEventStreamEventIdSinceNow),
                // Long enough to coalesce a Finder copy into one refresh,
                // short enough that a trashed folder vanishes promptly.
                0.4, flags)
        else { return nil }
        stream = s
        FSEventStreamSetDispatchQueue(s, .main)
        FSEventStreamStart(s)
    }

    /// Must be called before release: the stream holds an unretained pointer
    /// back to this object.
    func stop() {
        guard let s = stream else { return }
        FSEventStreamStop(s)
        FSEventStreamInvalidate(s)
        FSEventStreamRelease(s)
        stream = nil
    }

    fileprivate func receive(_ paths: [String], _ flags: [FSEventStreamEventFlags]) {
        var dirs = Set<String>()
        var lost = false
        for (p, f) in zip(paths, flags) {
            if f & UInt32(kFSEventStreamEventFlagRootChanged) != 0 {
                // The root itself moved or vanished; nothing inside it can be
                // resolved any more.
                lost = true
                continue
            }
            let dropped = UInt32(
                kFSEventStreamEventFlagMustScanSubDirs | kFSEventStreamEventFlagUserDropped
                    | kFSEventStreamEventFlagKernelDropped)
            if f & dropped != 0 {
                lost = true
                // Still worth re-listing what we were told about.
                dirs.insert(p)
                continue
            }
            if f & UInt32(kFSEventStreamEventFlagHistoryDone) != 0 { continue }
            dirs.insert((p as NSString).deletingLastPathComponent)
        }
        if !dirs.isEmpty || lost { handler(dirs, lost) }
    }
}

private let eventThunk: FSEventStreamCallback = { _, info, count, paths, flags, _ in
    guard let info else { return }
    let list = Unmanaged<CFArray>.fromOpaque(paths).takeUnretainedValue() as? [String] ?? []
    let fl = Array(UnsafeBufferPointer(start: flags, count: count))
    // The stream is scheduled on the main queue. The pointer crosses as an
    // integer: a raw pointer is not Sendable, and it is only ever turned back
    // into the watcher on the queue it came from.
    let addr = UInt(bitPattern: info)
    MainActor.assumeIsolated {
        guard let p = UnsafeMutableRawPointer(bitPattern: addr) else { return }
        Unmanaged<FolderWatcher>.fromOpaque(p).takeUnretainedValue().receive(list, fl)
    }
}
