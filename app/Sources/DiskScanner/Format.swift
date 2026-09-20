import DiskScannerFFI
import Foundation

/// Call a C function that fills a caller buffer and returns the length it
/// needs. Retries once when the first buffer was too small, so callers never
/// have to guess a capacity.
func cString(_ initial: Int = 1024, _ body: (UnsafeMutablePointer<CChar>, Int) -> Int) -> String {
    var buf = [CChar](repeating: 0, count: initial)
    let needed = buf.withUnsafeMutableBufferPointer { body($0.baseAddress!, initial) }
    if needed >= initial {
        var big = [CChar](repeating: 0, count: needed + 1)
        _ = big.withUnsafeMutableBufferPointer { body($0.baseAddress!, needed + 1) }
        return decodeCString(big)
    }
    return decodeCString(buf)
}

/// Decode a NUL-terminated buffer the C side filled.
///
/// Not `String(cString:)`: that is deprecated on arrays precisely because it
/// has no length bound and will read past the end of one that is somehow not
/// terminated. Stopping at the NUL ourselves is the same operation, bounded.
func decodeCString(_ buf: [CChar]) -> String {
    String(decoding: buf.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
}

/// Byte counts formatted exactly as the Rust side formats them, so the app
/// and the CLI never disagree about a number by a rounding step.
func bytes(_ b: UInt64) -> String {
    cString(64) { ds_human(b, $0, $1) }
}

extension UInt64 {
    var formattedBytes: String { bytes(self) }
    var grouped: String {
        let f = NumberFormatter()
        f.numberStyle = .decimal
        return f.string(from: NSNumber(value: self)) ?? String(self)
    }
}

extension UInt32 {
    var grouped: String { UInt64(self).grouped }
}
