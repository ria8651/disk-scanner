import AppKit
import DiskScannerFFI
import SwiftUI

/// Memo for the treemap layout, keyed by (node, size).
///
/// A reference type, and derived inside the `Canvas` renderer rather than
/// pushed through `@State`, for a specific reason: the previous version
/// computed rects in `onAppear`/`onChange(of: size)` and relied on SwiftUI
/// repainting the Canvas once that state landed. It often did not. The rects
/// were ready, the body had re-evaluated, and the map still sat blank until a
/// mouse event forced a redraw — measured at 11.7s on one run.
///
/// Layout is a pure function of (node, size), so computing it where the size
/// is known and the paint is about to happen removes the gap entirely: any
/// draw, at any size, already has the right geometry.
final class TreemapLayout {
    private var node: UInt32 = .max
    private var size: CGSize = .zero
    private(set) var rects: [DsRect] = []

    func rects(node: UInt32, size: CGSize, scan: Scan) -> [DsRect] {
        guard size.width > 1, size.height > 1 else { return [] }
        if node == self.node, abs(size.width - self.size.width) < 0.5,
            abs(size.height - self.size.height) < 0.5
        {
            return rects
        }
        let t = Date()
        self.node = node
        self.size = size
        rects = scan.treemap(of: node, size: size)
        Diag.slow("treemap layout \(rects.count) rects", t)
        return rects
    }
}

/// A nested treemap of the whole subtree, not one layer of it.
///
/// Directories become frames holding their children; only blocks too small or
/// too deep to subdivide are drawn solid. On a solid block, area is how big a
/// thing is, the core filled from the bottom is what deleting it would
/// actually return, and the rest is shaded by *why* it would not — cloned or
/// snapshot-pinned. That is the entire thesis, drawn rather than stated.
struct TreemapView: View {
    let model: ScanModel
    let scan: Scan
    let node: UInt32

    @State private var layout = TreemapLayout()
    @State private var hoverIndex: Int?

    private var hovered: DsRect? {
        guard let i = hoverIndex, layout.rects.indices.contains(i) else { return nil }
        return layout.rects[i]
    }

    var body: some View {
        // No GeometryReader: `Canvas` hands the renderer its own size, which
        // is the only size that matters and is never one of the speculative
        // intermediate sizes a GeometryReader reports during layout passes.
        Canvas(opaque: false) { ctx, size in
            let rects = layout.rects(node: node, size: size, scan: scan)
            let t = Date()
            // Paint order is the array order: a container is always drawn
            // before the children that sit on top of it.
            for (i, r) in rects.enumerated() {
                draw(r, hovered: i == hoverIndex, in: ctx)
            }
            Diag.slow("canvas draw \(rects.count) rects", t)
        }
        .contentShape(.rect)
        .onContinuousHover { phase in
            switch phase {
            case .active(let p): hoverIndex = hit(p)
            case .ended: hoverIndex = nil
            }
        }
        .onTapGesture(count: 2) { activate() }
        .onTapGesture(count: 1) { click() }
        .contextMenu {
            // `hovered` is current: the menu opens where the pointer already is.
            if let h = actionable {
                nodeMenuItems(for: h.node)
            }
        }
        .overlay(alignment: .bottomLeading) { caption }
    }

    // MARK: interaction

    /// Aggregate blocks stand for many tail items and carry their *parent's*
    /// id, so they must never be focused or selected as if they were a node.
    private var actionable: DsRect? {
        guard let h = hovered, h.aggregated == 0 else { return nil }
        return h
    }

    private func click() {
        guard let h = actionable else { return }
        if NSEvent.modifierFlags.contains(.command) {
            model.toggle(h.node)
        } else {
            model.focus = h.node
        }
    }

    private func activate() {
        guard let h = actionable else { return }
        model.descend(h.node)
    }

    /// The deepest rectangle under the cursor. Because layout is pre-order,
    /// that is simply the last one that contains the point.
    private func hit(_ p: CGPoint) -> Int? {
        let rects = layout.rects
        return rects.indices.reversed().first { i in
            let r = rects[i]
            return p.x >= CGFloat(r.x) && p.x < CGFloat(r.x + r.w) && p.y >= CGFloat(r.y)
                && p.y < CGFloat(r.y + r.h)
        }
    }

    /// Mirrors `View.nodeMenu`, but built inline because the Canvas has one
    /// context menu for the whole surface and must target the hovered block.
    @ViewBuilder
    private func nodeMenuItems(for node: UInt32) -> some View {
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
        ) { model.toggle(node) }
        Divider()
        Button("Copy Path", systemImage: "document.on.document") {
            let pb = NSPasteboard.general
            pb.clearContents()
            pb.setString(scan.path(node), forType: .string)
        }
    }

    // MARK: drawing

    private func draw(_ r: DsRect, hovered isHover: Bool, in ctx: GraphicsContext) {
        let rect = CGRect(x: CGFloat(r.x), y: CGFloat(r.y), width: CGFloat(r.w), height: CGFloat(r.h))
            .insetBy(dx: 0.5, dy: 0.5)
        guard rect.width > 0.5, rect.height > 0.5 else { return }
        // A rounded corner of radius r needs 2r of BOTH dimensions, so a
        // 3pt-tall sliver can never show more than ~1.5pt of rounding. Half
        // the short side is the geometric ceiling (beyond it the ends turn
        // into a capsule), so take as much of that as looks right.
        let radius = min(5, min(rect.width, rect.height) / 2.5)
        let path = Path(roundedRect: rect, cornerRadius: radius, style: .continuous)
        let isSelected = r.aggregated == 0 && model.selection.contains(r.node)

        if r.container != 0 {
            drawContainer(r, rect, path, isHover: isHover, isSelected: isSelected, in: ctx)
            return
        }

        if r.aggregated > 0 {
            ctx.fill(path, with: .color(.gray.opacity(0.28)))
            ctx.stroke(path, with: .color(Palette.hairline), lineWidth: 0.5)
            label(rect, ctx, title: "+\(r.aggregated.grouped)", subtitle: nil, depth: r.depth)
            return
        }

        if r.state != DS_STATE_OK {
            // Unreadable: drawn, labelled, and impossible to mistake for empty.
            ctx.fill(path, with: .color(Palette.unknown.opacity(0.32)))
            ctx.stroke(path, with: .color(Palette.unknown), lineWidth: 1)
            label(rect, ctx, title: "unreadable", subtitle: nil, depth: r.depth)
            return
        }

        // The remainder: everything deleting this would NOT return.
        let rest = Palette.remainder(snapshotFrac: r.snapshot_frac, exclusiveFrac: r.exclusive_frac)
        ctx.fill(path, with: .color(rest.jittered(r.node).opacity(0.5)))

        // The solid core, filled from the bottom: what you actually get back.
        let frac = CGFloat(max(0, min(1, r.exclusive_frac)))
        if frac > 0.004 {
            // A *copy* of the context: GraphicsContext.clip intersects and is
            // not scoped, so clipping the shared context here would silently
            // clip every block drawn after this one.
            var core = ctx
            core.clip(to: path)
            let h = rect.height * frac
            core.fill(
                Path(CGRect(x: rect.minX, y: rect.maxY - h, width: rect.width, height: h)),
                with: .color(Palette.yours.jittered(r.node)))
        }

        outline(path, ctx, isHover: isHover, isSelected: isSelected, incomplete: r.incomplete != 0)
        label(
            rect, ctx, title: scan.displayName(r.node),
            subtitle: r.physical.formattedBytes, depth: r.depth)
    }

    /// A directory that holds its children: a frame and a title strip, not a
    /// fill. Filling it would double-count — its children already cover the
    /// same area and carry the colour encoding.
    private func drawContainer(
        _ r: DsRect, _ rect: CGRect, _ path: Path, isHover: Bool, isSelected: Bool,
        in ctx: GraphicsContext
    ) {
        // Depth shading, so nesting is legible without any extra ink.
        let tint = Double(r.depth) * 0.035
        ctx.fill(path, with: .color(.primary.opacity(0.05 + tint)))
        ctx.stroke(path, with: .color(.primary.opacity(0.12)), lineWidth: 0.5)

        if rect.height > 46, rect.width > 50 {
            // No fill behind the title: the container's own tint already
            // separates it from its children, and a second shade on top of it
            // reads as another level of nesting that is not there.
            let strip = CGRect(x: rect.minX, y: rect.minY, width: rect.width, height: 13)
            labelStrip(
                strip, ctx, title: scan.displayName(r.node), trailing: r.physical.formattedBytes)
        }

        outline(path, ctx, isHover: isHover, isSelected: isSelected, incomplete: r.incomplete != 0)
    }

    private func outline(
        _ path: Path, _ ctx: GraphicsContext, isHover: Bool, isSelected: Bool, incomplete: Bool
    ) {
        if isSelected {
            ctx.stroke(path, with: .color(Palette.yours), lineWidth: 2.5)
        } else if isHover {
            ctx.stroke(path, with: .color(.white.opacity(0.85)), lineWidth: 1.6)
        }
        if incomplete {
            // A block whose total is a lower bound says so on its face.
            ctx.stroke(
                path, with: .color(Palette.unknown.opacity(0.8)),
                style: .init(lineWidth: 1.2, dash: [3, 3]))
        }
    }

    // MARK: labels

    private func label(
        _ rect: CGRect, _ ctx: GraphicsContext, title: @autoclosure () -> String,
        subtitle: @autoclosure () -> String?, depth: UInt8
    ) {
        // The guard comes first for a reason: building these strings costs an
        // FFI call and a heap allocation each, and most rects are too small to
        // ever show one. @autoclosure keeps the call sites readable.
        guard rect.width > 46, rect.height > 18 else { return }
        let text = ctx.resolve(
            Text(title()).font(.system(size: 10.5, weight: .medium)).foregroundStyle(.white))
        guard text.measure(in: rect.size).width <= rect.width - 8 else { return }
        ctx.draw(text, at: CGPoint(x: rect.minX + 4, y: rect.minY + 3), anchor: .topLeading)

        if rect.height > 32, let subtitle = subtitle() {
            let sub = ctx.resolve(
                Text(subtitle).font(.system(size: 9.5, design: .rounded))
                    .foregroundStyle(.white.opacity(0.8)))
            ctx.draw(sub, at: CGPoint(x: rect.minX + 4, y: rect.minY + 17), anchor: .topLeading)
        }
    }

    private func labelStrip(
        _ strip: CGRect, _ ctx: GraphicsContext, title: @autoclosure () -> String,
        trailing: @autoclosure () -> String
    ) {
        let name = ctx.resolve(
            Text(title()).font(.system(size: 10, weight: .semibold)).foregroundStyle(.primary))
        let size = ctx.resolve(
            Text(trailing()).font(.system(size: 9, design: .rounded)).foregroundStyle(.secondary))
        let sw = size.measure(in: strip.size).width
        let nw = name.measure(in: strip.size).width

        guard nw <= strip.width - 10 else { return }
        ctx.draw(name, at: CGPoint(x: strip.minX + 5, y: strip.midY), anchor: .leading)
        if nw + sw < strip.width - 16 {
            ctx.draw(size, at: CGPoint(x: strip.maxX - 5, y: strip.midY), anchor: .trailing)
        }
    }

    // MARK: hover caption
    //
    // Local state, so moving the pointer repaints this view and nothing else.

    @ViewBuilder
    private var caption: some View {
        if let h = hovered {
            HStack(spacing: 8) {
                if h.aggregated > 0 {
                    Text("\(h.aggregated.grouped) items too small to draw")
                    Text(h.physical.formattedBytes).foregroundStyle(.secondary)
                } else {
                    Image(systemName: h.container != 0 ? "folder" : "doc")
                        .foregroundStyle(.secondary)
                    Text(scan.displayName(h.node)).bold().lineLimit(1)
                    Text(h.physical.formattedBytes).foregroundStyle(.secondary)
                    if h.exclusive_frac > 0.004 {
                        Text("· \(Int(h.exclusive_frac * 100))% yours")
                            .foregroundStyle(Palette.yours)
                    }
                    if h.snapshot_frac > 0.01 {
                        Text("· \(Int(h.snapshot_frac * 100))% pinned")
                            .foregroundStyle(Palette.pinned)
                    }
                }
            }
            .font(.caption)
            .monospacedDigit()
            .padding(.horizontal, 13).padding(.vertical, 7)
            .glassEffect(.regular, in: .capsule)
            .padding(14)
            .allowsHitTesting(false)
            .transition(.opacity)
        }
    }
}
