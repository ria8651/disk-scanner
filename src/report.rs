//! Turning a scan into numbers a person can act on.

use crate::model::ROOT;
use crate::scan::ScanResult;

/// Human-readable bytes, binary units.
pub fn human(b: u64) -> String {
    const U: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", b, U[0])
    } else {
        format!("{:.2} {}", v, U[i])
    }
}

/// How the walked total compares with what the kernel says the volume holds.
///
/// These will not match, and the difference is informative rather than a bug:
/// snapshots hold blocks no live file references (pushing the walk *below*
/// `statfs`), while clones are counted once per file by `ALLOCSIZE` (pushing
/// the walk *above* it).
#[derive(Debug, Clone, Copy)]
pub struct Reconciliation {
    pub walked_physical: u64,
    pub volume_used: u64,
    pub volume_total: u64,
    /// `walked - volume_used`. Positive means we over-counted (clones).
    pub delta: i64,
}

impl Reconciliation {
    pub fn explain(&self) -> String {
        let d = self.delta;
        if d > 0 {
            format!(
                "walk exceeds volume by {} — clone blocks counted once per file, \
                 plus system-volume blocks shared with the seal",
                human(d as u64)
            )
        } else {
            format!(
                "volume exceeds walk by {} — snapshots, purgeable caches and \
                 filesystem metadata that no live file accounts for",
                human((-d) as u64)
            )
        }
    }
}

/// `None` when the scan root is not a volume root: comparing a subdirectory's
/// bytes against whole-volume usage would only ever produce a scary,
/// meaningless number.
pub fn reconcile(r: &ScanResult) -> Option<Reconciliation> {
    if !r.root_is_volume_root || r.volume_used == 0 {
        return None;
    }
    let walked = r.tree.node(ROOT).size.physical;
    Some(Reconciliation {
        walked_physical: walked,
        volume_used: r.volume_used,
        volume_total: r.volume_total,
        delta: walked as i64 - r.volume_used as i64,
    })
}

/// Evidence that snapshots are suppressing the reclaimable figures.
///
/// Inferred from scan data rather than by shelling out to `tmutil`: a file
/// pinned by a snapshot reports `exclusive == 0` with no clone flags, so a
/// large `shared_snapshot` total with few clone families is the signature.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotHint {
    pub likely: bool,
    pub pinned: u64,
    pub physical: u64,
}

impl SnapshotHint {
    pub fn fraction(&self) -> f64 {
        if self.physical == 0 {
            0.0
        } else {
            self.pinned as f64 / self.physical as f64
        }
    }
    pub fn message(&self) -> Option<String> {
        if !self.likely {
            return None;
        }
        Some(format!(
            "{} ({:.0}%) of allocated bytes are pinned by a snapshot or the \
             sealed system volume. Deleting those files frees nothing until the \
             snapshots expire — macOS keeps automatic local Time Machine \
             snapshots by default.",
            human(self.pinned),
            self.fraction() * 100.0
        ))
    }
}

pub fn snapshot_hint(r: &ScanResult) -> SnapshotHint {
    let root = r.tree.node(ROOT);
    let pinned = root.size.shared_snapshot;
    let physical = root.size.physical;
    SnapshotHint {
        likely: physical > 0 && pinned * 4 > physical, // >25% pinned
        pinned,
        physical,
    }
}

/// Clone families wholly contained in this scan: deleting every member really
/// would free the shared blocks. Families only partly present are excluded,
/// since the absent members keep the blocks alive.
pub fn reclaimable_from_clones(r: &ScanResult) -> (u64, usize, usize) {
    let mut bytes = 0u64;
    let mut whole = 0usize;
    for f in r.families.values() {
        if f.seen >= f.refcnt && f.refcnt > 1 {
            bytes += f.shared_bytes;
            whole += 1;
        }
    }
    (bytes, whole, r.families.len())
}
