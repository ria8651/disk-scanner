import AppKit
import SwiftUI

/// Window-state probe for headless inspection.
///
/// `CGWindowListCopyWindowInfo` is permission-gated and `screencapture` needs
/// Screen Recording, so from a terminal there is no way to see what the window
/// actually looks like. Asking AppKit directly costs nothing and needs no
/// entitlement, and beats guessing at which modifier changed the chrome.
enum Diag {
    static var enabled: Bool { ProcessInfo.processInfo.environment["DSCAN_DIAG"] != nil }

    /// A path to scan automatically, so both phases can be sampled in one run.
    static var autoScan: String? { ProcessInfo.processInfo.environment["DSCAN_DIAG_SCAN"] }

    private static let t0 = Date()

    /// Timestamped milestone, for finding where wall-clock actually goes.
    static func mark(_ label: String) {
        guard enabled else { return }
        emit(String(format: "%7.3fs  %@", Date().timeIntervalSince(t0), label))
    }

    /// Report a duration only when it is worth reporting.
    static func slow(_ label: String, _ start: Date, over ms: Double = 3) {
        guard enabled else { return }
        let dt = Date().timeIntervalSince(start) * 1000
        if dt >= ms { emit(String(format: "  SLOW %@: %.1f ms", label, dt)) }
    }

    static func window(_ tag: String, delay: TimeInterval = 1.0) {
        guard enabled else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
            guard let w = NSApplication.shared.windows.first(where: \.isVisible) else {
                emit("[\(tag)] no visible window")
                return
            }
            let cv = w.contentView
            emit(
                """
                [\(tag)]
                  frame            \(w.frame)
                  styleMask        \(w.styleMask.rawValue)
                  fullSizeContent  \(w.styleMask.contains(.fullSizeContentView))
                  unifiedTitleBar  \(w.styleMask.contains(.unifiedTitleAndToolbar))
                  toolbar          \(w.toolbar != nil) style=\(w.toolbarStyle.rawValue)
                  titlebarAppearsTransparent \(w.titlebarAppearsTransparent)
                  titlebarSeparator \(w.titlebarSeparatorStyle.rawValue)
                  contentView      \(type(of: cv as Any)) layer=\(cv?.layer != nil)
                  cornerRadius     \(cv?.layer?.cornerRadius ?? -1)
                  masksToBounds    \(cv?.layer?.masksToBounds ?? false)
                  subviewLayers    \(cv?.subviews.map { "\(type(of: $0)):r=\($0.layer?.cornerRadius ?? -1)" } ?? [])
                """)
        }
    }

    /// Dump the menu bar. Menus are built by SwiftUI from `Commands`, and
    /// there is no other way to confirm from a terminal that they came out the
    /// way they were written.
    static func menus(delay: TimeInterval = 1.0) {
        guard enabled else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
            guard let main = NSApplication.shared.mainMenu else {
                emit("no main menu")
                return
            }
            var out = "[menus]\n"
            for top in main.items {
                guard let sub = top.submenu else { continue }
                out += "  \(top.title)\n"
                for it in sub.items {
                    if it.isSeparatorItem {
                        out += "    ---\n"
                        continue
                    }
                    let key = it.keyEquivalent.isEmpty
                        ? ""
                        : "  [\(modifierString(it.keyEquivalentModifierMask))\(it.keyEquivalent.uppercased())]"
                    out += "    \(it.title)\(key)\(it.isEnabled ? "" : "  (disabled)")\n"
                }
            }
            emit(out)
        }
    }

    /// Hunt for whatever is drawing a hairline: any view in the window that is
    /// separator-shaped, or named like one. Guessing which modifier owns a
    /// 1pt line is slower than asking the view tree.
    static func separators(delay: TimeInterval = 1.5) {
        guard enabled else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
            // asyncAfter on the main queue IS the main actor; saying so keeps
            // the AppKit walk below from being flagged as cross-actor.
            MainActor.assumeIsolated {
            guard let w = NSApplication.shared.windows.first(where: \.isVisible),
                let root = w.contentView?.superview
            else { return }
            var out = "[separators]\n"
            // A nested func is its own isolation scope, so the enclosing
            // `assumeIsolated` does not cover it.
            @MainActor func walk(_ v: NSView, _ depth: Int) {
                let cls = String(describing: type(of: v))
                let f = v.convert(v.bounds, to: root)
                let thin = f.height > 0 && f.height <= 2.5 && f.width > 100
                let named = cls.lowercased().contains("separator")
                    || cls.lowercased().contains("divider")
                    || cls.lowercased().contains("border")
                if thin || named {
                    out += String(
                        format: "  %@%@  y=%.1f h=%.1f w=%.1f hidden=%@ alpha=%.2f\n",
                        String(repeating: " ", count: depth), cls, f.origin.y, f.height, f.width,
                        v.isHidden ? "Y" : "N", v.alphaValue)
                    // Who owns it: the chain up to the window is what names the
                    // modifier responsible.
                    var chain: [String] = []
                    var cur: NSView? = v.superview
                    while let c = cur, chain.count < 12 {
                        chain.append(String(describing: type(of: c)))
                        cur = c.superview
                    }
                    out += "      owned by: " + chain.joined(separator: " < ") + "\n"
                }
                for sub in v.subviews { walk(sub, depth + 1) }
            }
            walk(root, 0)
            emit(out)
            }
        }
    }

    private static func modifierString(_ m: NSEvent.ModifierFlags) -> String {
        var s = ""
        if m.contains(.control) { s += "^" }
        if m.contains(.option) { s += "⌥" }
        if m.contains(.shift) { s += "⇧" }
        if m.contains(.command) { s += "⌘" }
        return s
    }

    private static func emit(_ s: String) {
        FileHandle.standardError.write(Data((s + "\n").utf8))
    }
}

extension Diag {
    private nonisolated(unsafe) static var seen: Set<String> = []
    /// Milestones that happen inside a repeatedly-evaluated body.
    static func markOnce(_ label: String) {
        guard enabled, !seen.contains(label) else { return }
        seen.insert(label)
        mark(label)
    }
}
