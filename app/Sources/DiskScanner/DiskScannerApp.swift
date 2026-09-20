import AppKit
import DiskScannerFFI
import SwiftUI

@main
struct DiskScannerApp: App {
    /// Owned here, not in `ContentView`, because the menu bar acts on it.
    @State private var model = ScanModel()

    var body: some Scene {
        WindowGroup("Disk Scanner") {
            ContentView(model: model)
                .frame(minWidth: 940, minHeight: 620)
        }
        // Deliberately NOT .hiddenTitleBar: the toolbar's back button lives at
        // the top-left, exactly where the traffic lights would sit.
        .commands { MenuCommands(model: model) }
    }
}

// MARK: - Menu bar

private struct MenuCommands: Commands {
    let model: ScanModel

    private var sortBinding: Binding<SortKey> {
        Binding(get: { model.sort }, set: { model.sort = $0 })
    }

    var body: some Commands {
        // File. Replacing `.newItem` also removes "New Window", which would
        // otherwise open a second window sharing this one model.
        CommandGroup(replacing: .newItem) {
            Button("New Scan") { model.newScan() }
                .keyboardShortcut("n")
            Button("Scan Folder…") {
                if let p = chooseFolder() {
                    model.pendingPath = p
                    model.start(path: p)
                }
            }
            .keyboardShortcut("o")
            Divider()
            Button("Rescan") { model.start(path: model.rootPath) }
                .keyboardShortcut("r")
                .disabled(!model.canRescan)
            Button("Stop Scan") { model.cancel() }
                .keyboardShortcut(".", modifiers: .command)
                .disabled(!isScanning)
        }

        CommandGroup(after: .pasteboard) {
            Divider()
            Button("Deselect All") { model.clearSelection() }
                .keyboardShortcut("a", modifiers: [.shift, .command])
                .disabled(model.selection.isEmpty)
        }

        CommandGroup(after: .toolbar) {
            Button(model.showInspector ? "Hide Inspector" : "Show Inspector") {
                model.showInspector.toggle()
            }
            .keyboardShortcut("i", modifiers: [.option, .command])
            .disabled(model.scan == nil)
            Divider()
            Picker("Sort Contents By", selection: sortBinding) {
                ForEach(SortKey.allCases) { Text($0.label).tag($0) }
            }
            .disabled(model.scan == nil)
        }

        CommandMenu("Go") {
            Button("Enclosing Folder") { model.ascend() }
                .keyboardShortcut(.upArrow, modifiers: .command)
                .disabled(!canAscend)
            Button("Top of Scan") {
                if let s = model.scan {
                    model.cursor = s.root
                    model.focus = nil
                }
            }
            .keyboardShortcut(.upArrow, modifiers: [.shift, .command])
            .disabled(!canAscend)
            Divider()
            Button("Reveal in Finder") { revealFocus() }
                .keyboardShortcut("f", modifiers: [.shift, .command])
                .disabled(model.scan == nil)
        }

        CommandGroup(replacing: .help) {
            Button("Full Disk Access Settings…") {
                if let u = URL(string: String(cString: ds_fda_settings_url())) {
                    NSWorkspace.shared.open(u)
                }
            }
        }
    }

    private var isScanning: Bool {
        if case .scanning = model.phase { return true }
        return false
    }

    private var canAscend: Bool {
        guard let s = model.scan else { return false }
        return s.parent(model.cursor) != nil
    }

    private func revealFocus() {
        guard let s = model.scan else { return }
        revealInFinder(s.path(model.focus ?? model.cursor))
    }
}
