import AppKit
import DiskScannerFFI
import SwiftUI

/// Read-only, and the natural next step after finding something big.
func revealInFinder(_ path: String) {
    NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
}

extension View {
    /// The same actions wherever a node appears — a map block, an inspector
    /// row, the inspector header. Defined once so the three cannot drift.
    func nodeMenu(_ node: UInt32, model: ScanModel, scan: Scan) -> some View {
        contextMenu {
            let picked = model.selection.contains(node)
            let row = scan.row(node)
            // Taken when the menu opens. A refresh can land while it is open
            // and renumber nodes, so the actions go by path, not by `node`.
            let path = scan.path(node)

            Button("Reveal in Finder", systemImage: "magnifyingglass") {
                revealInFinder(path)
            }
            if let row, row.kind == DS_KIND_DIR, row.child_count > 0 {
                Button("Show in Map", systemImage: "square.grid.3x3") {
                    if let n = model.node(at: path) { model.open(n) }
                }
            }
            Divider()
            Button(
                picked ? "Remove from Selection" : "Add to Selection",
                systemImage: picked ? "minus.circle" : "checkmark.circle"
            ) {
                if let n = model.node(at: path) { model.toggle(n) }
            }
            Divider()
            Button("Copy Path", systemImage: "document.on.document") {
                let pb = NSPasteboard.general
                pb.clearContents()
                pb.setString(path, forType: .string)
            }
        }
    }
}
