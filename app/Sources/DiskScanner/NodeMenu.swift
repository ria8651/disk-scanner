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

            Button("Reveal in Finder", systemImage: "magnifyingglass") {
                revealInFinder(scan.path(node))
            }
            if let row, row.kind == DS_KIND_DIR, row.child_count > 0 {
                Button("Show in Map", systemImage: "square.grid.3x3") {
                    model.focus = node
                    model.descend(node)
                }
            }
            Divider()
            Button(
                picked ? "Remove from Selection" : "Add to Selection",
                systemImage: picked ? "minus.circle" : "checkmark.circle"
            ) {
                model.toggle(node)
            }
            Divider()
            Button("Copy Path", systemImage: "document.on.document") {
                let pb = NSPasteboard.general
                pb.clearContents()
                pb.setString(scan.path(node), forType: .string)
            }
        }
    }
}
