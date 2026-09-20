import AppKit
import DiskScannerFFI
import SwiftUI

struct BrowserView: View {
    let model: ScanModel
    let scan: Scan

    @State private var bannerDismissed = false

    var body: some View {
        // No NavigationStack here: ContentView owns the only one, so the
        // window keeps the same chrome in every phase. Nesting a second one
        // inside it gave the browser its own toolbar and its own title.
        //
        // Banner and basket sit in normal layout flow rather than in
        // `safeAreaBar`. Applied after `.inspector` those bars wrap the whole
        // split and float over the toolbar and the inspector, which made the
        // banner unreadable. Both carry text that must be legible, so they
        // take real space instead.
        //
        // The window inset is defined once, here, so the banner, the map and
        // the selection bar cannot drift out of alignment. Each owns only its
        // own internal padding. The map is inset but NOT clipped: square
        // corners, its own surface flush to its own edges.
        VStack(spacing: Metrics.gap) {
            banner
            TreemapView(model: model, scan: scan, node: model.cursor)
            footer
        }
        .padding(Metrics.inset)
        // macOS 26 hangs a "scroll pocket" off the titlebar background. Over a
        // scroll view it blurs the content passing beneath; over a pane that
        // never scrolls — a treemap — it degrades to NSHardPocketView, a bare
        // 1pt rule across the top of the map.
        //
        // `scrollEdgeEffectHidden` does NOT reach it: the pocket belongs to
        // NSTitlebarBackgroundView, not to the pane the modifier applies to
        // (verified by walking the view tree). Hiding the toolbar background
        // takes the pocket with it, and costs nothing here because the content
        // is inset and never passes under the toolbar anyway.
        .toolbarBackgroundVisibility(.hidden, for: .windowToolbar)
        .navigationTitle(scan.displayName(model.cursor))
        .navigationSubtitle(scan.path(model.cursor))
        .toolbarTitleMenu { ancestryMenu }
        .toolbar { toolbarContent }
        .inspector(
            isPresented: Binding(
                get: { model.showInspector }, set: { model.showInspector = $0 })
        ) {
            InspectorView(model: model, scan: scan)
                .inspectorColumnWidth(min: 300, ideal: 352, max: 520)
        }
    }

    // MARK: toolbar

    @ToolbarContentBuilder
    private var toolbarContent: some ToolbarContent {
        ToolbarItem(placement: .navigation) {
            Button("Up", systemImage: "chevron.up") { model.ascend() }
                .disabled(scan.parent(model.cursor) == nil)
                .help("Go up one level")
        }
        ToolbarSpacer(.flexible)
        ToolbarItem {
            Menu("Sort", systemImage: "arrow.up.arrow.down") {
                Picker("Sort", selection: Binding(get: { model.sort }, set: { model.sort = $0 })) {
                    ForEach(SortKey.allCases) { Text($0.label).tag($0) }
                }
                .pickerStyle(.inline)
            }
            .help("Order contents in the inspector")
        }
        ToolbarItem {
            Button("Rescan", systemImage: "arrow.clockwise") { model.start(path: model.rootPath) }
                .help("Scan \(model.rootPath) again")
        }
        ToolbarItem {
            Button("New Scan", systemImage: "house") { model.newScan() }
                .help("Back to the start screen")
        }
        ToolbarSpacer(.fixed)
        ToolbarItem {
            Button("Inspector", systemImage: "sidebar.trailing") { model.showInspector.toggle() }
                .help("Show or hide the inspector")
        }
    }

    /// Clicking the window title gives the path hierarchy, the way a document
    /// app gives you its folder chain.
    @ViewBuilder
    private var ancestryMenu: some View {
        ForEach(scan.ancestry(model.cursor).reversed(), id: \.self) { id in
            Button {
                model.cursor = id
                model.focus = nil
            } label: {
                Label(
                    scan.displayName(id),
                    systemImage: id == model.cursor ? "checkmark" : "folder")
            }
        }
    }

    // MARK: bars

    @ViewBuilder
    private var banner: some View {
        if !scan.snapshotNote.isEmpty && !bannerDismissed {
            SnapshotBanner(text: scan.snapshotNote) { bannerDismissed = true }
        }
    }

    @ViewBuilder
    private var footer: some View {
        FooterBar(model: model)
    }
}

// MARK: - Snapshot banner
//
// The single most valuable sentence the scanner produces. Without it, a mostly
// hollow treemap just looks like the tool is broken.

private struct SnapshotBanner: View {
    let text: String
    let dismiss: () -> Void
    @State private var expanded = false

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "clock.arrow.circlepath")
                .foregroundStyle(Palette.pinned)
                .padding(.top, 1)

            // No Spacer here, and the width is claimed explicitly. With a
            // Spacer the text was squeezed to almost nothing, and
            // `fixedSize(vertical:)` then faithfully grew to fit the resulting
            // wrap — which `lineLimit(2)` hid until "More" removed the cap and
            // the banner shoved the rest of the window off screen.
            Text(text)
                .font(.callout)
                // Expanded is still bounded. The message is a fixed template
                // that needs three or four lines; a cap costs nothing and
                // means a longer one can never break the layout again.
                .lineLimit(expanded ? 8 : 2)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)

            Button(expanded ? "Less" : "More") { withAnimation { expanded.toggle() } }
                .buttonStyle(.plain).font(.caption.weight(.medium))
                .foregroundStyle(Palette.pinned)
                .fixedSize()
            Button("Dismiss", systemImage: "xmark") { withAnimation { dismiss() } }
                .labelStyle(.iconOnly)
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 16).padding(.vertical, 11)
        .frame(maxWidth: .infinity, alignment: .leading)
        .glassEffect(
            .regular.tint(Palette.pinned.opacity(0.18)),
            in: .rect(cornerRadius: Metrics.radius, style: .continuous))
    }
}

// MARK: - Footer bar
//
// One bar, two contents. The legend and the selection summary are the same
// object: a single glass container whose contents change. An earlier version
// swapped two differently-sized views inside a fixed-height slot, which left
// the legend floating in space reserved for a bar that was not there yet.
//
// The bar grows for the selection state, so the change reads as the same bar
// expanding rather than one control being replaced by another.

private struct FooterBar: View {
    let model: ScanModel

    var body: some View {
        HStack(spacing: 12) {
            if model.selection.isEmpty {
                LegendItems()
                Spacer(minLength: 0)
                Text("⌘-click a block to see what deleting it would really free")
                    .font(.caption2).foregroundStyle(.tertiary).lineLimit(1)
            } else {
                selection
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 16)
        .padding(.vertical, model.selection.isEmpty ? 9 : 11)
        .glassEffect(.regular, in: .rect(cornerRadius: Metrics.radius, style: .continuous))

    }

    /// The feature the data model makes possible and a per-row number cannot:
    /// reclaim evaluated over the whole selection, so completing a clone
    /// family makes the figure jump.
    @ViewBuilder
    private var selection: some View {
        let r = model.reclaim
        let n = model.selection.count

        Label("\(n)", systemImage: "checkmark.circle.fill")
            .font(.system(.body, design: .rounded, weight: .semibold))
            .monospacedDigit()
            .foregroundStyle(Palette.yours)
            .help("\(n) selected")

        Divider().frame(height: 26)

        VStack(alignment: .leading, spacing: 1) {
            HStack(alignment: .firstTextBaseline, spacing: 5) {
                Text(r.bytes.formattedBytes)
                    .font(.system(.title3, design: .rounded, weight: .bold))
                    .monospacedDigit()
                    .contentTransition(.numericText())
                    .foregroundStyle(Palette.yours)
                Text("actually freed").font(.caption).foregroundStyle(.secondary)
            }
            Text("of \(r.physical.formattedBytes) allocated")
                .font(.caption2).foregroundStyle(.tertiary)
        }
        .lineLimit(1)

        if r.held_by_clones_outside > 0 {
            caveat(
                "doc.on.doc", Palette.cloned,
                "\(r.held_by_clones_outside.formattedBytes) needs every clone selected")
        }
        if r.pinned_by_snapshot > 0 {
            caveat(
                "clock.arrow.circlepath", Palette.pinned,
                "\(r.pinned_by_snapshot.formattedBytes) snapshot-pinned")
        }

        Spacer(minLength: 4)

        Button("Clear", systemImage: "xmark") { model.clearSelection() }
        .labelStyle(.titleOnly)
        .buttonStyle(.glass)
        .buttonBorderShape(.capsule)
        .controlSize(.small)
    }

    private func caveat(_ icon: String, _ color: Color, _ text: String) -> some View {
        Label(text, systemImage: icon)
            .font(.caption)
            .foregroundStyle(color)
            .lineLimit(1)
    }
}
