import AppKit
import SwiftUI

/// A colour that resolves per appearance, so the palette stays legible against
/// both light and dark glass rather than being tuned for one of them.
private func dynamic(light: NSColor, dark: NSColor) -> Color {
    Color(
        nsColor: NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? dark : light
        })
}

private func hex(_ v: UInt32) -> NSColor {
    NSColor(
        srgbRed: Double((v >> 16) & 0xff) / 255, green: Double((v >> 8) & 0xff) / 255,
        blue: Double(v & 0xff) / 255, alpha: 1)
}

/// The palette *is* the argument.
///
/// Three categories, not six numbers: what you would get back, what a clone is
/// holding, and what a snapshot has pinned. Everything else in the model is a
/// diagnostic and belongs in the inspector, not in the geometry.
enum Palette {
    /// Bytes this object alone owns. Deleting it returns these.
    static let yours = dynamic(light: hex(0x0E9B84), dark: hex(0x3ED9BC))
    /// Shared with APFS clones — recoverable, but only if every clone goes.
    static let cloned = dynamic(light: hex(0x2F6FD0), dark: hex(0x6FA8FF))
    /// Pinned by a snapshot or the sealed system volume. Not yours to free.
    static let pinned = dynamic(light: hex(0x7A5CC4), dark: hex(0xB49BFF))
    /// Unreadable: the one thing other scanners silently omit.
    static let unknown = dynamic(light: hex(0xC07818), dark: hex(0xF0B450))

    static let hairline = Color.black.opacity(0.22)

    /// Which category owns the non-exclusive remainder of a block.
    static func remainder(snapshotFrac: Float, exclusiveFrac: Float) -> Color {
        let rest = max(0, 1 - exclusiveFrac)
        return snapshotFrac > rest * 0.5 ? pinned : cloned
    }
}

extension Color {
    /// Deterministic slight lightness jitter so sibling blocks of the same
    /// category stay distinguishable without introducing a second meaning.
    func jittered(_ seed: UInt32) -> Color {
        let d = (Double(seed &* 2_654_435_761 % 100) / 100.0 - 0.5) * 0.14
        let n = NSColor(self).usingColorSpace(.sRGB) ?? .gray
        return Color(
            red: min(1, max(0, n.redComponent + d)),
            green: min(1, max(0, n.greenComponent + d)),
            blue: min(1, max(0, n.blueComponent + d)))
    }
}

/// A small colour chip used wherever a number needs its category named.
struct Swatch: View {
    let color: Color
    var size: CGFloat = 9
    var body: some View {
        RoundedRectangle(cornerRadius: size / 3, style: .continuous)
            .fill(color)
            .frame(width: size, height: size)
    }
}

/// Window-level spacing, in one place. The map, the banner and the selection
/// bar are siblings floating on the window's own background, so they share an
/// inset and a corner radius rather than each carrying their own.
enum Metrics {
    static let inset: CGFloat = 10
    static let gap: CGFloat = 8
    static let radius: CGFloat = 14
}
