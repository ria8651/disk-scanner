import DiskScannerFFI
import SwiftUI

/// Grouped `Form` sections, so the inspector reads as a native macOS
/// inspector rather than a hand-drawn panel: system card shapes, system
/// spacing, and `LabeledContent` rows that align their values automatically.
struct InspectorView: View {
    let model: ScanModel
    let scan: Scan

    private var target: UInt32 { model.focus ?? model.cursor }

    var body: some View {
        Form {
            if let row = scan.row(target) {
                Section {
                    Identity(scan: scan, node: target, row: row)
                        .nodeMenu(target, model: model, scan: scan)
                }
                Section("Size") { SizeSection(size: row.size) }
                Section("If you deleted this") { ReclaimSection(scan: scan, node: target) }
                if row.child_count > 0 {
                    Section("Contents") { ContentsSection(model: model, scan: scan, node: target) }
                }
            }
            Section("Scan") { ScanSection(scan: scan) }
        }
        .formStyle(.grouped)
        .scrollEdgeEffectStyle(.soft, for: .top)
    }
}

// MARK: - Identity

private struct Identity: View {
    let scan: Scan
    let node: UInt32
    let row: DsRow

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Label {
                Text(scan.displayName(node)).font(.headline).lineLimit(2)
            } icon: {
                Image(systemName: row.kind == DS_KIND_DIR ? "folder.fill" : "doc.fill")
                    .foregroundStyle(.secondary)
            }

            Text(scan.path(node))
                .font(.system(size: 10, design: .monospaced))
                .foregroundStyle(.tertiary)
                .lineLimit(3)
                .textSelection(.enabled)

            if row.is_clone != 0 || row.hardlink_alias != 0 || row.unknown_below > 0 {
                HStack(spacing: 6) {
                    if row.is_clone != 0 { Chip("clone", Palette.cloned) }
                    if row.hardlink_alias != 0 { Chip("hardlink alias", .secondary) }
                    if row.unknown_below > 0 {
                        Chip("\(row.unknown_below.grouped) unreadable below", Palette.unknown)
                    }
                }
            }
            if row.hardlink_alias != 0 {
                Text("Another link to an inode already counted; its bytes are recorded there.")
                    .font(.caption2).foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 2)
    }
}

private struct Chip: View {
    let text: String
    let color: Color
    init(_ t: String, _ c: Color) {
        text = t
        color = c
    }
    var body: some View {
        Text(text)
            .font(.system(size: 9, weight: .medium))
            .padding(.horizontal, 7).padding(.vertical, 3)
            .background(color.opacity(0.18), in: .capsule)
            .overlay(Capsule().strokeBorder(color.opacity(0.4)))
            .foregroundStyle(color)
    }
}

// MARK: - Size
//
// Six numbers exist in the model; three of them are what a person decides on.
// The rest are diagnostics and stay folded away.

private struct SizeSection: View {
    let size: DsSizes

    var body: some View {
        LabeledContent("Allocated") {
            Text(size.physical.formattedBytes)
                .font(.system(.body, design: .rounded, weight: .semibold))
                .monospacedDigit()
        }

        StackedBar(
            parts: [
                (Palette.yours, size.exclusive),
                (Palette.cloned, size.shared_clones),
                (Palette.pinned, size.shared_snapshot),
            ], total: size.physical
        )
        .listRowInsets(.init(top: 2, leading: 16, bottom: 8, trailing: 16))

        row("Yours", size.exclusive, Palette.yours, "freed by deleting this")
        row("Cloned", size.shared_clones, Palette.cloned, "needs every clone deleted too")
        row("Pinned", size.shared_snapshot, Palette.pinned, "held by a snapshot or the seal")

        DisclosureGroup("Diagnostics") {
            LabeledContent {
                Text(size.logical.formattedBytes).font(.caption).monospacedDigit()
            } label: {
                Text("Apparent").font(.caption)
                Text("what Finder shows; overstates sparse and iCloud files")
            }
            LabeledContent {
                Text(size.sparse_saving.formattedBytes).font(.caption).monospacedDigit()
            } label: {
                Text("Sparse holes").font(.caption)
                Text("never written to disk")
            }
        }
    }

    private func row(_ name: String, _ v: UInt64, _ c: Color, _ note: String) -> some View {
        LabeledContent {
            Text(v.formattedBytes).monospacedDigit().font(.system(size: 11, design: .monospaced))
        } label: {
            Label { Text(name) } icon: { Swatch(color: c) }
            Text(note)
        }
    }
}

private struct StackedBar: View {
    let parts: [(Color, UInt64)]
    let total: UInt64

    var body: some View {
        GeometryReader { g in
            HStack(spacing: 1.5) {
                ForEach(Array(parts.enumerated()), id: \.offset) { _, p in
                    Capsule().fill(p.0)
                        .frame(width: total == 0 ? 0 : g.size.width * CGFloat(p.1) / CGFloat(total))
                }
                Spacer(minLength: 0)
            }
        }
        .frame(height: 7)
        .background(.quaternary, in: .capsule)
    }
}

// MARK: - Reclaim
//
// The number every other scanner gets wrong: not a sum of `exclusive`, but
// what this subtree would actually give back if it went away.

private struct ReclaimSection: View {
    let scan: Scan
    let node: UInt32

    private func timedReclaim() -> DsReclaim {
        let t = Date()
        defer { Diag.slow("inspector reclaim", t) }
        return scan.subtreeReclaim(node)
    }

    var body: some View {
        let r = timedReclaim()
        HStack(alignment: .firstTextBaseline, spacing: 7) {
            Text(r.bytes.formattedBytes)
                .font(.system(.title2, design: .rounded, weight: .bold))
                .monospacedDigit()
                .foregroundStyle(Palette.yours)
            VStack(alignment: .leading, spacing: 0) {
                Text("actually freed").font(.caption)
                if r.physical > r.bytes {
                    Text("of \(r.physical.formattedBytes) allocated")
                        .font(.caption2).foregroundStyle(.tertiary)
                }
            }
        }

        if r.families_completed > 0 {
            note(
                Palette.yours,
                "\(r.families_completed.grouped) clone families are wholly inside — their shared blocks do come back."
            )
        }
        if r.held_by_clones_outside > 0 {
            note(
                Palette.cloned,
                "\(r.held_by_clones_outside.formattedBytes) is held by clones living outside this folder."
            )
        }
        if r.pinned_by_snapshot > 0 {
            note(
                Palette.pinned,
                "\(r.pinned_by_snapshot.formattedBytes) stays pinned by a snapshot no matter what.")
        }
        if r.unknown > 0 {
            note(Palette.unknown, "\(r.unknown.grouped) unreadable entries: this is a lower bound.")
        }
    }

    private func note(_ c: Color, _ s: String) -> some View {
        Label {
            Text(s).font(.caption).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        } icon: {
            Circle().fill(c).frame(width: 6, height: 6)
        }
    }
}

// MARK: - Contents

private struct ContentsSection: View {
    let model: ScanModel
    let scan: Scan
    let node: UInt32

    var body: some View {
        let rows = scan.children(of: node, sort: model.sort, limit: 60)
        // One Form row holding the whole list, laid out here with zero spacing.
        //
        // A grouped `Form` on macOS is not a `List`, so `listRowInsets` and
        // `listRowSeparator` are ignored and the Form's own inter-row spacing
        // stays outside any hover region a row can define. Owning the stack is
        // the only way to make adjacent rows actually touch, which is what
        // stops the highlight flickering off between them.
        VStack(spacing: 0) {
            ForEach(Array(rows.enumerated()), id: \.offset) { _, r in
                row(r)
            }
            if rows.count == 60 {
                Text("showing the 60 largest")
                    .font(.caption2).foregroundStyle(.tertiary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 4)
            }
        }
    }

    @ViewBuilder
    private func row(_ r: DsRow) -> some View {
        let picked = model.selection.contains(r.node)
        let lit = model.highlighted == r.node
        HStack(spacing: 8) {
            Button {
                // No withAnimation: the footer owns the animation for
                // selection changes, so every entry point eases alike.
                model.toggle(r.node)
            } label: {
                Image(systemName: picked ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(picked ? Palette.yours : Color.secondary)
            }
            .buttonStyle(.plain)
            .help(picked ? "Remove from selection" : "Add to selection")

            Image(systemName: r.kind == DS_KIND_DIR ? "folder" : "doc")
                .font(.caption2).foregroundStyle(.tertiary)

            Text(scan.displayName(r.node))
                .font(.system(size: 11)).lineLimit(1).truncationMode(.middle)

            Spacer(minLength: 6)

            Text(r.size.physical.formattedBytes)
                .font(.system(size: 10, design: .monospaced))
                .monospacedDigit()
                .foregroundStyle(.secondary)
        }
        // Padding is INSIDE the hover region, and rows are flush, so moving
        // down the list never crosses a gap that is part of no row.
        .padding(.horizontal, 6)
        .padding(.vertical, 5)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 6, style: .continuous)
                .fill(Color.primary.opacity(lit ? 0.09 : 0))
        )
        .contentShape(.rect)
        .onTapGesture { model.open(r.node) }
        .onHover { model.highlighted = $0 ? r.node : nil }
        .nodeMenu(r.node, model: model, scan: scan)
    }
}

// MARK: - Scan facts

private struct ScanSection: View {
    let scan: Scan

    var body: some View {
        let s = scan.summary
        LabeledContent("Files", value: s.files.grouped)
        LabeledContent("Directories", value: s.dirs.grouped)
        LabeledContent("Elapsed", value: String(format: "%.2fs", s.elapsed_secs))
        if s.clone_members > 0 {
            LabeledContent("Clone members", value: s.clone_members.grouped)
        }
        if s.dataless > 0 {
            LabeledContent {
                Text(s.dataless.grouped)
            } label: {
                Text("iCloud-evicted")
                Text("never downloaded by this scan")
            }
        }
        if s.hardlink_aliases > 0 {
            LabeledContent("Hardlink aliases", value: s.hardlink_aliases.grouped)
        }
        if s.denied_tcc + s.denied_perm + s.errors > 0 {
            LabeledContent {
                Text("\(s.denied_tcc.grouped) / \(s.denied_perm.grouped) / \(s.errors.grouped)")
                    .foregroundStyle(Palette.unknown)
            } label: {
                Text("Unreadable")
                Text("TCC / permissions / errors")
            }
        }
        if !scan.reconcileNote.isEmpty {
            Text(scan.reconcileNote)
                .font(.caption2).foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
        if !scan.skippedMounts.isEmpty {
            DisclosureGroup("\(scan.skippedMounts.count) mounts not entered") {
                ForEach(Array(scan.skippedMounts.enumerated()), id: \.offset) { _, m in
                    LabeledContent {
                        Text(m.reason).font(.caption2).foregroundStyle(.tertiary)
                    } label: {
                        Text(m.path).font(.system(size: 10, design: .monospaced)).lineLimit(1)
                    }
                }
            }
        }
        Text("⌘-click blocks in the map, or tick items above, to see what deleting them together would really free.")
            .font(.caption2).foregroundStyle(.tertiary)
            .fixedSize(horizontal: false, vertical: true)
    }
}
