import AppKit
import DiskScannerFFI
import SwiftUI

struct ContentView: View {
    /// Owned by the app, so the menu bar and the window act on one model.
    let model: ScanModel

    var body: some View {
        // One NavigationStack for every phase. The window's chrome is then
        // identical throughout: measured, the *only* difference between the
        // start screen and the browser was that the browser had an NSToolbar
        // and the start screen did not — which is what made macOS round the
        // titlebar differently once a scan finished.
        NavigationStack { content }
            .task {
                Diag.window("idle")
                Diag.menus()
                if let p = Diag.autoScan {
                    // Must outlast the idle sample, or both samples catch the
                    // browser and the comparison is meaningless.
                    try? await Task.sleep(for: .seconds(3))
                    model.start(path: p)
                }
            }
            .onChange(of: model.phase) { _, new in
                if case .ready = new {
                    Diag.window("ready", delay: 1.5)
                    Diag.separators(delay: 2.0)
                }
            }
    }

    @ViewBuilder
    private var content: some View {
        switch model.phase {
        case .ready:
            if let scan = model.scan {
                BrowserView(model: model, scan: scan)
            }
        case .idle:
            SetupScreen(model: model) { StartPanel(model: model) }
        case .scanning(let d, let f, let p):
            SetupScreen(model: model) {
                ScanningPanel(model: model, dirs: d, files: f, physical: p)
            }
        case .failed(let why):
            SetupScreen(model: model) { FailurePanel(model: model, message: why) }
        }
    }
}

// MARK: - Setup chrome
//
// One frame shared by idle / scanning / failed so the window does not jump
// between states. It fills the window; an earlier version let a VStack size
// itself and painted a flat colour behind it, which left a seam down each
// edge where the flat colour met the window's own material.

private struct SetupScreen<Content: View>: View {
    let model: ScanModel
    @ViewBuilder var content: Content

    var body: some View {
        VStack(spacing: 26) {
            Spacer(minLength: 0)
            content
            Spacer(minLength: 0)
            LegendRow()
                .padding(.bottom, 4)
        }
        .padding(40)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .navigationTitle("Disk Scanner")
        .toolbar { toolbarItem }
    }

    /// Not decoration: the window needs a toolbar in every phase or its
    /// titlebar rounding changes when one appears. Each phase gets the action
    /// that is actually useful in it.
    @ToolbarContentBuilder
    private var toolbarItem: some ToolbarContent {
        ToolbarItem {
            switch model.phase {
            case .scanning:
                Button("Cancel", systemImage: "stop.fill") { model.cancel() }
            case .failed:
                Button("Start over", systemImage: "chevron.backward") { model.phase = .idle }
            default:
                Button("Choose Folder…", systemImage: "folder") {
                    if let p = chooseFolder() {
                        model.pendingPath = p
                        model.start(path: p)
                    }
                }
            }
        }
    }
}

/// Shared by the toolbar and the start panel.
///
/// `@MainActor` because `NSOpenPanel` is: without it Swift 6 flags every
/// property set on the panel as a cross-actor mutation.
@MainActor
func chooseFolder() -> String? {
    let panel = NSOpenPanel()
    panel.canChooseDirectories = true
    panel.canChooseFiles = false
    panel.allowsMultipleSelection = false
    return panel.runModal() == .OK ? panel.url?.path : nil
}

private struct Wordmark: View {
    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "internaldrive")
                .font(.system(size: 34, weight: .light))
                .foregroundStyle(Palette.yours.gradient)
                .frame(width: 76, height: 76)
                .background(.quaternary.opacity(0.5), in: .rect(cornerRadius: 20, style: .continuous))

            Text("Disk Scanner")
                .font(.system(size: 28, weight: .semibold, design: .rounded))
            Text("What your disk is really doing — including the parts other scanners average away.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 440)
        }
    }
}

// MARK: - Idle

private struct StartPanel: View {
    @Bindable var model: ScanModel

    private var path: Binding<String> {
        Binding(get: { model.pendingPath }, set: { model.pendingPath = $0 })
    }

    var body: some View {
        VStack(spacing: 24) {
            Wordmark()
            FullDiskAccessCard(model: model)

            GlassEffectContainer(spacing: 10) {
                HStack(spacing: 10) {
                    TextField("Path to scan", text: path)
                        .textFieldStyle(.plain)
                        .font(.system(.body, design: .monospaced))
                        .padding(.horizontal, 14)
                        .padding(.vertical, 8)
                        .frame(width: 330)
                        .glassEffect(.regular, in: .capsule)

                    Button("Choose…", systemImage: "folder") { if let p = chooseFolder() { model.pendingPath = p } }
                        .labelStyle(.titleOnly)
                        .buttonStyle(.glass)
                        .buttonBorderShape(.capsule)

                    Button("Scan", systemImage: "arrow.right") { model.start(path: model.pendingPath) }
                        .labelStyle(.titleOnly)
                        .buttonStyle(.glassProminent)
                        .buttonBorderShape(.capsule)
                        .keyboardShortcut(.defaultAction)
                }
            }

            HStack(spacing: 8) {
                ForEach(shortcuts, id: \.path) { s in
                    Button {
                        model.pendingPath = s.path
                        model.start(path: s.path)
                    } label: {
                        Label(s.name, systemImage: s.icon)
                            .font(.caption)
                    }
                    .buttonStyle(.glass)
                    .buttonBorderShape(.capsule)
                    .controlSize(.small)
                }
            }
        }
    }

    private var shortcuts: [(name: String, path: String, icon: String)] {
        [
            ("Whole disk", "/", "internaldrive"),
            ("Home", NSHomeDirectory(), "house"),
            ("Downloads", downloadsPath, "arrow.down.circle"),
            ("Applications", "/Applications", "square.grid.2x2"),
        ]
    }

    /// Asked for rather than assembled by hand: the folder is relocatable, so
    /// `~/Downloads` is a guess and this is the answer.
    private var downloadsPath: String {
        FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask).first?.path
            ?? NSHomeDirectory() + "/Downloads"
    }

}

/// FDA can be detected but never requested — there is no API, only System
/// Settings. Saying so before the scan is what stops a partial result from
/// being mistaken for a complete one.
private struct FullDiskAccessCard: View {
    let model: ScanModel

    var body: some View {
        if model.fullDiskAccess == DS_ACCESS_DENIED {
            let app = model.responsibleApp
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "lock.trianglebadge.exclamationmark")
                    .font(.title3)
                    .foregroundStyle(Palette.unknown)
                VStack(alignment: .leading, spacing: 5) {
                    Text("Full Disk Access is not granted").font(.headline)
                    Text(
                        app.isEmpty
                            ? "Results will be incomplete. Unreadable folders are shown as gaps, never as zero."
                            : "Results will be incomplete. Grant access to **\(app)** — macOS judges the responsible process, not this binary."
                    )
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    Button("Open System Settings") {
                        if let u = URL(string: String(cString: ds_fda_settings_url())) {
                            NSWorkspace.shared.open(u)
                        }
                    }
                    .buttonStyle(.glass)
                    .buttonBorderShape(.capsule)
                    .controlSize(.small)
                    .padding(.top, 2)
                }
            }
            .padding(16)
            .frame(maxWidth: 520, alignment: .leading)
            .background(
                Palette.unknown.opacity(0.10), in: .rect(cornerRadius: 16, style: .continuous)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .strokeBorder(Palette.unknown.opacity(0.35)))
        }
    }
}

// MARK: - Scanning

private struct ScanningPanel: View {
    let model: ScanModel
    let dirs: UInt64
    let files: UInt64
    let physical: UInt64

    var body: some View {
        VStack(spacing: 18) {
            Wordmark().opacity(0.9)

            ProgressView().controlSize(.small)

            Text(model.rootPath)
                .font(.system(.callout, design: .monospaced))
                .foregroundStyle(.secondary)
                .lineLimit(1).truncationMode(.head)

            HStack(spacing: 22) {
                counter(dirs.grouped, "directories")
                counter(files.grouped, "files")
                counter(physical.formattedBytes, "allocated")
            }
            .padding(.horizontal, 22).padding(.vertical, 14)
            .glassEffect(.regular, in: .rect(cornerRadius: 18, style: .continuous))

            Button("Cancel", systemImage: "stop.fill") { model.cancel() }
                .labelStyle(.titleOnly)
                .buttonStyle(.glass)
                .buttonBorderShape(.capsule)
        }
    }

    private func counter(_ value: String, _ label: String) -> some View {
        VStack(spacing: 2) {
            Text(value)
                .font(.system(.title3, design: .rounded, weight: .semibold))
                .monospacedDigit()
                .contentTransition(.numericText())
            Text(label).font(.caption).foregroundStyle(.secondary)
        }
        .frame(minWidth: 96)
    }
}

private struct FailurePanel: View {
    let model: ScanModel
    let message: String

    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 34, weight: .light))
                .foregroundStyle(Palette.unknown)
            Text("Scan failed").font(.title2.weight(.semibold))
            Text(message)
                .font(.callout).foregroundStyle(.secondary)
                .multilineTextAlignment(.center).frame(maxWidth: 420)
            Button("Back") { model.phase = .idle }
                .buttonStyle(.glass)
                .buttonBorderShape(.capsule)
        }
    }
}

// MARK: - Legend
//
// The encoding needs exactly one sentence of explanation, and then it reads on
// its own everywhere else in the app.

/// Just the swatches. Used bare inside the browser's footer bar, and wrapped
/// in its own capsule on the start screen.
struct LegendItems: View {
    var body: some View {
        HStack(spacing: 18) {
            item(Palette.yours, "Yours", "freed by deleting it")
            item(Palette.cloned, "Cloned", "needs every clone gone")
            item(Palette.pinned, "Pinned", "held by a snapshot")
            item(Palette.unknown, "Unreadable", "not counted")
        }
        .font(.caption2)
        .lineLimit(1)
    }

    private func item(_ c: Color, _ name: String, _ note: String) -> some View {
        HStack(spacing: 5) {
            Swatch(color: c, size: 8)
            Text(name).foregroundStyle(.primary)
            Text(note).foregroundStyle(.tertiary)
        }
    }
}

struct LegendRow: View {
    var body: some View {
        LegendItems()
            .padding(.horizontal, 16).padding(.vertical, 9)
            .glassEffect(.regular, in: .capsule)
    }
}
