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

// ---------------------------------------------------------------------------
// True reclaim: what deleting a *set* of things would actually free.
//
// This cannot be a sum. Three clones of a 64 MiB file each report
// `exclusive == 0` and `shared_clones == 64 MiB`; summing the subtree says
// "0 exclusive, 192 MiB shared" when the real answer for deleting all three
// is 64 MiB, and for deleting any two of them is zero. Reclaim is therefore
// a property of the selection as a whole, evaluated against clone families.

use crate::model::{NodeId, Tree};
use std::collections::{HashMap, HashSet};

/// What deleting a selection would free, and what would survive it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reclaim {
    /// Bytes actually freed, right now, by deleting everything selected.
    pub bytes: u64,
    /// Of `bytes`: blocks no one else referenced to begin with.
    pub from_exclusive: u64,
    /// Of `bytes`: clone-shared blocks released because *every* member of
    /// those families is inside the selection.
    pub from_completed_families: u64,
    /// Clone families the selection covers completely.
    pub families_completed: u32,
    /// Clone-shared bytes NOT freed, because members live outside the
    /// selection. Adding those members would convert this into `bytes`.
    pub held_by_clones_outside: u64,
    /// Bytes pinned by a snapshot or the sealed system volume. Deleting the
    /// files does not free these at all, whatever else is selected.
    pub pinned_by_snapshot: u64,
    /// Allocated bytes covered by the selection (`du`'s answer for it).
    pub physical: u64,
    pub files: u64,
    pub dirs: u64,
    /// Unreadable nodes inside the selection: `bytes` is a lower bound.
    pub unknown: u32,
}

impl Reclaim {
    /// Fraction of covered allocation that deleting this selection frees.
    pub fn efficiency(&self) -> f64 {
        if self.physical == 0 {
            0.0
        } else {
            self.bytes as f64 / self.physical as f64
        }
    }
    /// The one-line honest summary for a UI footer.
    pub fn summary(&self) -> String {
        let mut s = format!("frees {}", human(self.bytes));
        if self.held_by_clones_outside > 0 {
            s.push_str(&format!(
                "; {} more would need every clone selected too",
                human(self.held_by_clones_outside)
            ));
        }
        if self.pinned_by_snapshot > 0 {
            s.push_str(&format!(
                "; {} stays pinned by snapshots regardless",
                human(self.pinned_by_snapshot)
            ));
        }
        if self.unknown > 0 {
            s.push_str(&format!("; {} unreadable — lower bound", self.unknown));
        }
        s
    }
}

/// Bytes freed by deleting every node in `sel`, and their whole subtrees.
///
/// Overlapping selections are safe: a node reached twice (because both it and
/// an ancestor were selected) is counted once.
///
/// **Hardlink caveat.** Additional links to an already-counted inode are
/// folded to zero by the scanner, so selecting only an alias correctly frees
/// nothing. The converse is not modelled: selecting the *first*-seen link of
/// a multiply-linked file credits its full `exclusive` even though the other
/// links keep the blocks alive. `Node` does not retain `linkcount`, so this
/// cannot currently be detected after the fact.
pub fn reclaim(r: &ScanResult, sel: &[NodeId]) -> Reclaim {
    let t: &Tree = &r.tree;
    let mut out = Reclaim::default();

    // Drop any selection that already lies inside another selection. Once the
    // remaining roots are disjoint their subtrees cannot overlap, so the walk
    // needs no visited set at all — which is the whole cost at this scale:
    // a per-node hash insert over millions of files, to guard against an
    // overlap that a cheap ancestor check has already ruled out.
    let set: HashSet<NodeId> = sel
        .iter()
        .copied()
        .filter(|&n| (n as usize) < t.len())
        .collect();
    let mut roots: Vec<NodeId> = Vec::with_capacity(set.len());
    for &n in &set {
        let mut cur = n;
        let mut covered = false;
        // Depth, not breadth: a handful of steps up to the scan root.
        loop {
            let p = t.node(cur).parent;
            if p == crate::model::NO_NODE || p == cur {
                break;
            }
            if set.contains(&p) {
                covered = true;
                break;
            }
            cur = p;
        }
        if !covered {
            roots.push(n);
        }
    }

    // cloneid -> how many of that family the selection covers.
    let mut fam_hits: HashMap<u64, u32> = HashMap::new();
    let mut stack: Vec<NodeId> = roots;

    while let Some(id) = stack.pop() {
        let n = t.node(id);
        for c in n.children() {
            stack.push(c);
        }
        if !matches!(n.state(), crate::model::State::Ok) {
            out.unknown += 1;
        }
        if n.is_dir() {
            out.dirs += 1;
            continue; // directory sizes are subtree rollups; the files carry them
        }
        if n.hardlink_alias {
            continue; // already accounted for at the first link
        }
        out.files += 1;
        out.physical += n.size.physical;
        out.from_exclusive += n.size.exclusive;
        out.pinned_by_snapshot += n.size.shared_snapshot;
        if let Some(&cid) = r.clone_of.get(&id) {
            *fam_hits.entry(cid).or_insert(0) += 1;
        }
    }

    for (cid, hits) in fam_hits {
        let Some(f) = r.families.get(&cid) else {
            continue;
        };
        if hits >= f.refcnt {
            out.from_completed_families += f.shared_bytes;
            out.families_completed += 1;
        } else {
            out.held_by_clones_outside += f.shared_bytes;
        }
    }

    out.bytes = out.from_exclusive + out.from_completed_families;
    out
}

/// Reclaim for a single subtree — the number a directory row wants to show.
pub fn subtree_reclaim(r: &ScanResult, node: NodeId) -> Reclaim {
    reclaim(r, &[node])
}
