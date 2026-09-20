//! C ABI for the Swift front end.
//!
//! # Shape of the boundary
//!
//! The tree is 4.2M nodes and ~450 MB; it never crosses. Swift holds an
//! opaque `DsScan *` and asks narrow questions — "children of node N sorted
//! by allocated size, rows 0..50", "treemap rectangles for node N at 900x600"
//! — which are answered by filling a caller-allocated buffer of POD structs.
//! No serialisation, no allocation on the Swift side, no per-node bridging.
//!
//! # Lifecycle
//!
//! A scan is mutable while it runs and immutable forever after. `ds_scan_begin`
//! spawns a thread and returns a `DsJob *` that can only be cancelled or
//! freed; the finished `DsScan *` arrives through the completion callback.
//! Nothing reads the tree while it is being built, which is why no lock
//! appears anywhere below.
//!
//! # Panics
//!
//! Release builds set `panic = "abort"`, so no unwind can cross into Swift.
//! Every entry point below additionally bounds-checks its arguments and
//! returns a neutral value rather than indexing blindly, because a stale
//! `NodeId` from the UI is an expected input, not a bug.

use crate::model::{Kind, NodeId, State, ROOT};
use crate::report;
use crate::scan::{self, Options, ScanResult};
use crate::sys::tcc;

use std::ffi::{c_char, c_void, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Plain-old-data types shared with `include/disk_scanner.h`.
// Any change here must be mirrored there.

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DsSizes {
    pub logical: u64,
    pub physical: u64,
    pub exclusive: u64,
    pub shared_clones: u64,
    pub shared_snapshot: u64,
    pub sparse_saving: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DsRow {
    pub node: u32,
    pub child_count: u32,
    pub unknown_below: u32,
    pub kind: u8,
    pub state: u8,
    pub hardlink_alias: u8,
    pub is_clone: u8,
    pub mtime: i64,
    pub size: DsSizes,
}

/// One rectangle of a laid-out treemap.
///
/// `exclusive_frac` and `snapshot_frac` are what let the view draw the whole
/// argument without numbers: area is how big a thing is, the solid core is
/// what deleting it would actually return, and the rest is shaded by *why*
/// it would not.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DsRect {
    pub node: u32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub exclusive_frac: f32,
    pub snapshot_frac: f32,
    pub physical: u64,
    /// 0 for a real node; otherwise this block stands for N tail items too
    /// small to draw, and `node` is their parent.
    pub aggregated: u32,
    pub kind: u8,
    pub state: u8,
    /// `unknown_below > 0`: this block's total is a lower bound.
    pub incomplete: u8,
    /// 1 when this block's children are drawn nested inside it, so the view
    /// frames it and labels it rather than filling it.
    pub container: u8,
    /// Nesting level below the node being shown; 0 for its direct children.
    pub depth: u8,
    pub _pad: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DsReclaim {
    pub bytes: u64,
    pub from_exclusive: u64,
    pub from_completed_families: u64,
    pub held_by_clones_outside: u64,
    pub pinned_by_snapshot: u64,
    pub physical: u64,
    pub files: u64,
    pub dirs: u64,
    pub families_completed: u32,
    pub unknown: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DsSummary {
    pub root: DsSizes,
    pub dirs: u64,
    pub files: u64,
    pub symlinks: u64,
    pub hardlink_aliases: u64,
    pub dataless: u64,
    pub compressed: u64,
    pub sparse: u64,
    pub clone_members: u64,
    pub denied_tcc: u64,
    pub denied_perm: u64,
    pub errors: u64,
    pub volume_used: u64,
    pub volume_total: u64,
    pub clone_reclaimable: u64,
    pub node_count: u64,
    pub arena_bytes: u64,
    pub elapsed_secs: f64,
    pub root_unknown_below: u32,
    pub clone_families_whole: u32,
    pub clone_families_total: u32,
    pub skipped_mounts: u32,
    pub cancelled: u8,
    pub root_is_volume_root: u8,
    pub _pad: [u8; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DsReconcile {
    pub walked_physical: u64,
    pub volume_used: u64,
    pub volume_total: u64,
    pub delta: i64,
}

pub const DS_SORT_PHYSICAL: i32 = 0;
pub const DS_SORT_EXCLUSIVE: i32 = 1;
pub const DS_SORT_LOGICAL: i32 = 2;
pub const DS_SORT_NAME: i32 = 3;
pub const DS_SORT_MTIME: i32 = 4;

// ---------------------------------------------------------------------------
// Handles

/// A finished, immutable scan.
pub struct DsScan {
    r: ScanResult,
    /// One-entry memo for `ds_children`. Scrolling a directory re-asks for the
    /// same ordering every frame; re-sorting 100k children each time is the
    /// obvious way to make a fast scanner feel slow.
    sort_cache: Mutex<Option<(NodeId, i32, bool, Vec<NodeId>)>>,
}

/// A scan in flight. Cancellable; yields a `DsScan` through the callback.
pub struct DsJob {
    cancel: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The Swift-side context pointer, carried onto the scan thread.
///
/// Safety contract, upheld by the caller: the pointer must remain valid until
/// the completion callback has run, and both callbacks must tolerate being
/// invoked from a non-main thread (Swift hops to the main actor itself).
struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}

impl Ctx {
    /// Taking the pointer through a method matters: edition-2021 closures
    /// capture disjoint *fields*, so `move || f(c.0)` would capture the raw
    /// `*mut c_void` — which is not `Send` — instead of this wrapper.
    #[inline]
    fn ptr(&self) -> *mut c_void {
        self.0
    }
}

pub type DsProgressFn = extern "C" fn(ctx: *mut c_void, dirs: u64, files: u64, physical: u64);
pub type DsDoneFn = extern "C" fn(ctx: *mut c_void, scan: *mut DsScan, err: *const c_char);

// ---------------------------------------------------------------------------
// Small helpers

/// Copy `s` into a caller buffer as NUL-terminated UTF-8.
/// Returns the length the string needs, so `len >= cap` means "call again".
unsafe fn out_str(s: &[u8], buf: *mut c_char, cap: usize) -> usize {
    if !buf.is_null() && cap > 0 {
        let n = s.len().min(cap - 1);
        std::ptr::copy_nonoverlapping(s.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    s.len()
}

impl DsScan {
    #[inline]
    fn valid(&self, node: u32) -> bool {
        (node as usize) < self.r.tree.len()
    }
}

fn sizes(s: &crate::model::Sizes) -> DsSizes {
    DsSizes {
        logical: s.logical,
        physical: s.physical,
        exclusive: s.exclusive,
        shared_clones: s.shared_clones,
        shared_snapshot: s.shared_snapshot,
        sparse_saving: s.sparse_saving,
    }
}

fn row_of(sc: &DsScan, id: NodeId) -> DsRow {
    let n = sc.r.tree.node(id);
    DsRow {
        node: id,
        child_count: n.child_count,
        unknown_below: n.unknown_below,
        kind: n.kind() as u8,
        state: match n.state() {
            State::Ok => 0,
            State::DeniedTcc => 1,
            State::DeniedPerm => 2,
            State::Error => 3,
            State::Skipped => 4,
        },
        hardlink_alias: n.hardlink_alias as u8,
        is_clone: sc.r.clone_of.contains_key(&id) as u8,
        mtime: n.mtime,
        size: sizes(&n.size),
    }
}

// ---------------------------------------------------------------------------
// Full Disk Access

#[no_mangle]
pub extern "C" fn ds_full_disk_access() -> i32 {
    match tcc::full_disk_access() {
        tcc::Access::Full => 0,
        tcc::Access::Denied => 1,
        tcc::Access::Unknown => 2,
    }
}

#[no_mangle]
pub extern "C" fn ds_fda_settings_url() -> *const c_char {
    // Static, NUL-terminated, never freed.
    static URL: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    URL.get_or_init(|| CString::new(tcc::SETTINGS_URL).unwrap())
        .as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn ds_responsible_app_hint(buf: *mut c_char, cap: usize) -> usize {
    match tcc::responsible_app_hint() {
        Some(s) => out_str(s.as_bytes(), buf, cap),
        None => out_str(b"", buf, cap),
    }
}

// ---------------------------------------------------------------------------
// Running a scan

/// Begin a scan. Returns NULL only if `path` is not valid UTF-8 or NUL-free.
#[no_mangle]
pub unsafe extern "C" fn ds_scan_begin(
    path: *const c_char,
    threads: u32,
    on_progress: Option<DsProgressFn>,
    on_done: DsDoneFn,
    ctx: *mut c_void,
) -> *mut DsJob {
    if path.is_null() {
        return std::ptr::null_mut();
    }
    let root = PathBuf::from(std::ffi::OsStr::from_bytes(CStr::from_ptr(path).to_bytes()));

    let mut opts = Options::default();
    if threads > 0 {
        opts.threads = threads as usize;
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel2 = Arc::clone(&cancel);
    let c = Ctx(ctx);

    let thread = std::thread::Builder::new()
        .name("dscan".into())
        .stack_size(2 << 20)
        .spawn(move || {
            let c = c; // move the whole wrapper, not the raw pointer
            let progress: Option<Box<dyn Fn(scan::Progress) + Send>> = match on_progress {
                Some(f) => {
                    let p = Ctx(c.ptr());
                    Some(Box::new(move |g: scan::Progress| {
                        f(p.ptr(), g.dirs, g.files, g.physical);
                    }))
                }
                None => None,
            };
            match scan::scan(&root, opts, cancel2, progress) {
                Ok(r) => {
                    let boxed = Box::into_raw(Box::new(DsScan {
                        r,
                        sort_cache: Mutex::new(None),
                    }));
                    on_done(c.ptr(), boxed, std::ptr::null());
                }
                Err(e) => {
                    let msg = CString::new(e.to_string()).unwrap_or_default();
                    on_done(c.ptr(), std::ptr::null_mut(), msg.as_ptr());
                }
            }
        });

    match thread {
        Ok(t) => Box::into_raw(Box::new(DsJob {
            cancel,
            thread: Some(t),
        })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Ask the scan to stop. It still delivers a (partial) result via the callback.
#[no_mangle]
pub unsafe extern "C" fn ds_scan_cancel(job: *mut DsJob) {
    if let Some(j) = job.as_ref() {
        j.cancel.store(true, Ordering::Relaxed);
    }
}

/// Free the job handle. Blocks until the scan thread has exited, so cancel
/// first if the scan may still be running.
#[no_mangle]
pub unsafe extern "C" fn ds_job_free(job: *mut DsJob) {
    if job.is_null() {
        return;
    }
    let mut j = Box::from_raw(job);
    if let Some(t) = j.thread.take() {
        let _ = t.join();
    }
}

#[no_mangle]
pub unsafe extern "C" fn ds_scan_free(scan: *mut DsScan) {
    if !scan.is_null() {
        drop(Box::from_raw(scan));
    }
}

// ---------------------------------------------------------------------------
// Reading a finished scan

#[no_mangle]
pub extern "C" fn ds_root() -> u32 {
    ROOT
}

#[no_mangle]
pub unsafe extern "C" fn ds_node_count(scan: *const DsScan) -> u32 {
    scan.as_ref().map(|s| s.r.tree.len() as u32).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn ds_parent(scan: *const DsScan, node: u32) -> u32 {
    match scan.as_ref() {
        Some(s) if s.valid(node) => s.r.tree.node(node).parent,
        _ => crate::model::NO_NODE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn ds_row(scan: *const DsScan, node: u32, out: *mut DsRow) -> bool {
    match (scan.as_ref(), out.as_mut()) {
        (Some(s), Some(o)) if s.valid(node) => {
            *o = row_of(s, node);
            true
        }
        _ => false,
    }
}

#[no_mangle]
pub unsafe extern "C" fn ds_name(
    scan: *const DsScan,
    node: u32,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    match scan.as_ref() {
        Some(s) if s.valid(node) => out_str(s.r.tree.name(node), buf, cap),
        _ => out_str(b"", buf, cap),
    }
}

#[no_mangle]
pub unsafe extern "C" fn ds_path(
    scan: *const DsScan,
    node: u32,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    match scan.as_ref() {
        Some(s) if s.valid(node) => out_str(s.r.tree.path(node).as_os_str().as_bytes(), buf, cap),
        _ => out_str(b"", buf, cap),
    }
}

/// Children of `node` in the requested order, windowed by `offset`.
/// Returns how many rows were written.
#[no_mangle]
pub unsafe extern "C" fn ds_children(
    scan: *const DsScan,
    node: u32,
    sort: i32,
    descending: bool,
    offset: usize,
    out: *mut DsRow,
    cap: usize,
) -> usize {
    let Some(s) = scan.as_ref() else { return 0 };
    if !s.valid(node) || out.is_null() || cap == 0 {
        return 0;
    }

    let mut guard = s.sort_cache.lock().unwrap();
    let hit = matches!(&*guard, Some((n, k, d, _)) if *n == node && *k == sort && *d == descending);
    if !hit {
        let mut ids: Vec<NodeId> = s.r.tree.node(node).children().collect();
        let t = &s.r.tree;
        match sort {
            DS_SORT_NAME => ids.sort_unstable_by(|&a, &b| t.name(a).cmp(t.name(b))),
            DS_SORT_MTIME => ids.sort_unstable_by_key(|&i| t.node(i).mtime),
            DS_SORT_EXCLUSIVE => ids.sort_unstable_by_key(|&i| t.node(i).size.exclusive),
            DS_SORT_LOGICAL => ids.sort_unstable_by_key(|&i| t.node(i).size.logical),
            _ => ids.sort_unstable_by_key(|&i| t.node(i).size.physical),
        }
        if descending {
            ids.reverse();
        }
        *guard = Some((node, sort, descending, ids));
    }
    let ids = &guard.as_ref().unwrap().3;

    let n = ids.len().saturating_sub(offset).min(cap);
    for k in 0..n {
        *out.add(k) = row_of(s, ids[offset + k]);
    }
    n
}

// ---------------------------------------------------------------------------
// Treemap layout
//
// Laid out in Rust rather than Swift: the geometry depends on child ordering
// and on the aggregation cutoff, both of which need the arena. Swift receives
// only the rectangles it is about to draw.
//
// The layout is *recursive*. A one-level treemap answers "what is big in this
// folder"; a nested one answers "where does the weight actually sit", which is
// the question someone opens a disk scanner with. Directories become frames
// containing their children; only blocks too small or too deep to subdivide
// are drawn as solid leaves.

struct LRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Worst aspect ratio in a candidate row (Bruls et al.).
fn worst(sum: f64, max: f64, min: f64, side: f64) -> f64 {
    if sum <= 0.0 || side <= 0.0 || min <= 0.0 {
        return f64::INFINITY;
    }
    let s2 = sum * sum;
    let w2 = side * side;
    ((w2 * max) / s2).max(s2 / (w2 * min))
}

/// Squarified treemap. `vals` must be sorted descending and sum to `r`'s area.
fn squarify(vals: &[f64], mut r: LRect, out: &mut Vec<(usize, LRect)>) {
    let mut i = 0usize;
    while i < vals.len() {
        let side = r.w.min(r.h) as f64;
        if side <= 0.0 || !side.is_finite() {
            break;
        }
        // Grow a row while it keeps getting squarer.
        let mut sum = 0.0f64;
        let mut best = f64::INFINITY;
        let mut j = i;
        while j < vals.len() {
            let ns = sum + vals[j];
            let w = worst(ns, vals[i], vals[j], side);
            if j > i && w > best {
                break;
            }
            best = w;
            sum = ns;
            j += 1;
        }
        if sum <= 0.0 {
            break;
        }

        // Place vals[i..j] as a strip along the short side.
        if r.w <= r.h {
            let hh = (sum / r.w as f64) as f32;
            let mut x = r.x;
            for k in i..j {
                let ww = (vals[k] / sum) as f32 * r.w;
                out.push((k, LRect { x, y: r.y, w: ww, h: hh }));
                x += ww;
            }
            r.y += hh;
            r.h -= hh;
        } else {
            let ww = (sum / r.h as f64) as f32;
            let mut y = r.y;
            for k in i..j {
                let hh = (vals[k] / sum) as f32 * r.h;
                out.push((k, LRect { x: r.x, y, w: ww, h: hh }));
                y += hh;
            }
            r.x += ww;
            r.w -= ww;
        }
        i = j;
    }
}

struct Tm<'a> {
    t: &'a crate::model::Tree,
    out: &'a mut Vec<DsRect>,
    cap: usize,
    min_px: f32,
    max_depth: u32,
    gap: f32,
}

/// Extra inset inside a container, on top of the sibling gap its children
/// already carry. Kept small: separation is the gap's job, at every level.
const TM_PAD: f32 = 1.0;
/// A group's gap as a fraction of its typical tile. "Excessive padding" is
/// never about absolute size — 3pt is invisible between two 200pt tiles and
/// eats a seventh of a 20pt one — so the gap is derived from what it is
/// separating and the `gap` argument becomes a ceiling rather than a constant.
const TM_GAP_RATIO: f32 = 0.10;
/// Below this a gap stops reading as a gap and starts reading as a seam.
const TM_GAP_MIN: f32 = 0.5;
/// Height of a container's title strip, when there is room for one.
const TM_HEADER: f32 = 13.0;

fn leaf_rect(t: &crate::model::Tree, id: NodeId, r: &LRect, depth: u32, container: bool) -> DsRect {
    let n = t.node(id);
    let p = n.size.physical.max(1) as f32;
    DsRect {
        node: id,
        x: r.x,
        y: r.y,
        w: r.w,
        h: r.h,
        exclusive_frac: (n.size.exclusive as f32 / p).clamp(0.0, 1.0),
        snapshot_frac: (n.size.shared_snapshot as f32 / p).clamp(0.0, 1.0),
        physical: n.size.physical,
        aggregated: 0,
        kind: n.kind() as u8,
        state: match n.state() {
            State::Ok => 0,
            State::DeniedTcc => 1,
            State::DeniedPerm => 2,
            State::Error => 3,
            State::Skipped => 4,
        },
        incomplete: (n.unknown_below > 0) as u8,
        container: container as u8,
        depth: depth.min(255) as u8,
        _pad: [0; 3],
    }
}

/// Place one node, deciding whether it can hold its children or must be drawn
/// solid. Emitted pre-order, so a parent always precedes the children painted
/// on top of it — which is also what makes "deepest rectangle wins" the
/// correct hit test on the Swift side.
fn lay_node(c: &mut Tm, id: NodeId, r: LRect, depth: u32) {
    if c.out.len() >= c.cap {
        return;
    }
    let n = c.t.node(id);
    let nestable = n.is_dir()
        && n.child_count > 0
        && depth < c.max_depth
        && r.w > 4.0 * c.min_px
        && r.h > 4.0 * c.min_px;

    if !nestable {
        c.out.push(leaf_rect(c.t, id, &r, depth, false));
        return;
    }

    // Reserve a title strip only when the block is tall enough that losing it
    // does not squash the children into nothing.
    let header = if r.h > 46.0 && r.w > 50.0 { TM_HEADER } else { 0.0 };
    let inner = LRect {
        x: r.x + TM_PAD,
        y: r.y + header + TM_PAD,
        w: r.w - 2.0 * TM_PAD,
        h: r.h - header - 2.0 * TM_PAD,
    };
    if inner.w < 2.0 * c.min_px || inner.h < 2.0 * c.min_px {
        c.out.push(leaf_rect(c.t, id, &r, depth, false));
        return;
    }

    c.out.push(leaf_rect(c.t, id, &r, depth, true));
    lay_children(c, id, inner, depth + 1);
}

/// Squarify `parent`'s children into `r`, then recurse into each.
fn lay_children(c: &mut Tm, parent: NodeId, r: LRect, depth: u32) {
    if c.out.len() >= c.cap {
        return;
    }
    let t = c.t;
    let mut ids: Vec<NodeId> = t
        .node(parent)
        .children()
        .filter(|&i| t.node(i).size.physical > 0)
        .collect();
    if ids.is_empty() {
        return;
    }
    ids.sort_unstable_by_key(|&i| std::cmp::Reverse(t.node(i).size.physical));

    let total: u64 = ids.iter().map(|&i| t.node(i).size.physical).sum();
    if total == 0 {
        return;
    }
    let area = (r.w as f64) * (r.h as f64);
    let scale = area / total as f64;
    let min_area = (c.min_px as f64) * (c.min_px as f64);

    // Everything below the drawable cutoff collapses into one trailing block,
    // so a directory with 100k entries yields a few hundred rectangles rather
    // than 100k invisible ones.
    let mut head = ids.len();
    for (k, &i) in ids.iter().enumerate() {
        if (t.node(i).size.physical as f64) * scale < min_area {
            head = k;
            break;
        }
    }
    let budget = c.cap.saturating_sub(c.out.len());
    if head + 1 > budget {
        head = budget.saturating_sub(1);
    }
    let tail: u64 = ids[head..].iter().map(|&i| t.node(i).size.physical).sum();
    let tail_count = ids.len() - head;

    let mut vals: Vec<f64> = ids[..head]
        .iter()
        .map(|&i| t.node(i).size.physical as f64 * scale)
        .collect();
    if tail > 0 {
        vals.push(tail as f64 * scale);
    }
    if vals.is_empty() {
        return;
    }

    // One gap for the whole group, taken from the MEDIAN tile rather than the
    // mean: treemap areas are heavy-tailed, and a single huge first child
    // would otherwise talk a crowd of small ones into wide gaps.
    let gap = if head > 0 {
        let median_area = vals[head / 2].max(1.0);
        ((median_area.sqrt() as f32) * TM_GAP_RATIO).clamp(TM_GAP_MIN, c.gap)
    } else {
        TM_GAP_MIN
    };

    let mut laid: Vec<(usize, LRect)> = Vec::with_capacity(vals.len());
    squarify(&vals, r, &mut laid);

    for (k, rc) in laid {
        if c.out.len() >= c.cap {
            return;
        }
        // Half the gap comes off each side, so neighbours end up a full gap
        // apart. Still clamped per tile: within a group a single sliver must
        // stay visible rather than have the group's gap eat it.
        let g = gap * 0.5;
        let rc = LRect {
            x: rc.x + g.min(rc.w * 0.2),
            y: rc.y + g.min(rc.h * 0.2),
            w: (rc.w - 2.0 * g.min(rc.w * 0.2)).max(0.0),
            h: (rc.h - 2.0 * g.min(rc.h * 0.2)).max(0.0),
        };
        if k >= head {
            // The aggregate block stands for the tail and carries the parent's
            // id, so the view can name it without pretending it is a node.
            c.out.push(DsRect {
                node: parent,
                x: rc.x,
                y: rc.y,
                w: rc.w,
                h: rc.h,
                exclusive_frac: 0.0,
                snapshot_frac: 0.0,
                physical: tail,
                aggregated: tail_count as u32,
                kind: 3,
                state: 0,
                incomplete: 0,
                container: 0,
                depth: depth.min(255) as u8,
                _pad: [0; 3],
            });
        } else {
            lay_node(c, ids[k], rc, depth);
        }
    }
}

/// Lay out the subtree under `node` in a `w` x `h` rectangle.
///
/// `max_depth` bounds the nesting (0 = direct children only, drawn solid).
/// Blocks thinner than `min_px` are folded into an aggregate block at their
/// own level. `gap` is the space left between sibling tiles, at every level.
/// Returns the number of rectangles written, in paint order.
#[no_mangle]
pub unsafe extern "C" fn ds_treemap(
    scan: *const DsScan,
    node: u32,
    w: f32,
    h: f32,
    min_px: f32,
    max_depth: u32,
    gap: f32,
    out: *mut DsRect,
    cap: usize,
) -> usize {
    let Some(s) = scan.as_ref() else { return 0 };
    if !s.valid(node) || out.is_null() || cap == 0 || w <= 0.0 || h <= 0.0 {
        return 0;
    }
    let mut buf: Vec<DsRect> = Vec::with_capacity(cap.min(4096));
    {
        let mut c = Tm {
            t: &s.r.tree,
            out: &mut buf,
            cap,
            min_px: min_px.max(0.5),
            max_depth,
            gap: gap.max(0.0),
        };
        lay_children(&mut c, node, LRect { x: 0.0, y: 0.0, w, h }, 0);
    }
    let n = buf.len().min(cap);
    std::ptr::copy_nonoverlapping(buf.as_ptr(), out, n);
    n
}


// ---------------------------------------------------------------------------
// Reclaim, summary, explanations

/// What deleting `nodes` would actually free. See `report::reclaim`.
#[no_mangle]
pub unsafe extern "C" fn ds_reclaim(
    scan: *const DsScan,
    nodes: *const u32,
    count: usize,
    out: *mut DsReclaim,
) -> bool {
    let (Some(s), Some(o)) = (scan.as_ref(), out.as_mut()) else {
        return false;
    };
    let sel: &[u32] = if nodes.is_null() || count == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(nodes, count)
    };
    let r = report::reclaim(&s.r, sel);
    *o = DsReclaim {
        bytes: r.bytes,
        from_exclusive: r.from_exclusive,
        from_completed_families: r.from_completed_families,
        held_by_clones_outside: r.held_by_clones_outside,
        pinned_by_snapshot: r.pinned_by_snapshot,
        physical: r.physical,
        files: r.files,
        dirs: r.dirs,
        families_completed: r.families_completed,
        unknown: r.unknown,
    };
    true
}

#[no_mangle]
pub unsafe extern "C" fn ds_summary(scan: *const DsScan, out: *mut DsSummary) -> bool {
    let (Some(s), Some(o)) = (scan.as_ref(), out.as_mut()) else {
        return false;
    };
    let r = &s.r;
    let root = r.tree.node(ROOT);
    let st = &r.stats;
    let (clone_bytes, whole, total) = report::reclaimable_from_clones(r);
    *o = DsSummary {
        root: sizes(&root.size),
        dirs: st.dirs,
        files: st.files,
        symlinks: st.symlinks,
        hardlink_aliases: st.hardlink_aliases,
        dataless: st.dataless,
        compressed: st.compressed,
        sparse: st.sparse,
        clone_members: st.clone_members,
        denied_tcc: st.denied_tcc,
        denied_perm: st.denied_perm,
        errors: st.errors,
        volume_used: r.volume_used,
        volume_total: r.volume_total,
        clone_reclaimable: clone_bytes,
        node_count: r.tree.len() as u64,
        arena_bytes: r.tree.memory_bytes() as u64,
        elapsed_secs: st.elapsed.as_secs_f64(),
        root_unknown_below: root.unknown_below,
        clone_families_whole: whole as u32,
        clone_families_total: total as u32,
        skipped_mounts: r.skipped.len() as u32,
        cancelled: r.cancelled as u8,
        root_is_volume_root: r.root_is_volume_root as u8,
        _pad: [0; 6],
    };
    true
}

#[no_mangle]
pub unsafe extern "C" fn ds_reconcile(scan: *const DsScan, out: *mut DsReconcile) -> bool {
    let (Some(s), Some(o)) = (scan.as_ref(), out.as_mut()) else {
        return false;
    };
    match report::reconcile(&s.r) {
        Some(rc) => {
            *o = DsReconcile {
                walked_physical: rc.walked_physical,
                volume_used: rc.volume_used,
                volume_total: rc.volume_total,
                delta: rc.delta,
            };
            true
        }
        None => false,
    }
}

/// The reconciliation sentence, or empty when the root is not a volume root.
#[no_mangle]
pub unsafe extern "C" fn ds_reconcile_text(
    scan: *const DsScan,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    match scan.as_ref().and_then(|s| report::reconcile(&s.r)) {
        Some(rc) => out_str(rc.explain().as_bytes(), buf, cap),
        None => out_str(b"", buf, cap),
    }
}

/// The "why is reclaimable so low" sentence, or empty when it does not apply.
#[no_mangle]
pub unsafe extern "C" fn ds_snapshot_hint(
    scan: *const DsScan,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    match scan
        .as_ref()
        .and_then(|s| report::snapshot_hint(&s.r).message())
    {
        Some(m) => out_str(m.as_bytes(), buf, cap),
        None => out_str(b"", buf, cap),
    }
}

/// Mounts deliberately not entered. Index `< ds_skipped_count`.
#[no_mangle]
pub unsafe extern "C" fn ds_skipped_count(scan: *const DsScan) -> usize {
    scan.as_ref().map(|s| s.r.skipped.len()).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn ds_skipped(
    scan: *const DsScan,
    index: usize,
    path: *mut c_char,
    path_cap: usize,
    reason: *mut c_char,
    reason_cap: usize,
) -> bool {
    let Some(s) = scan.as_ref() else { return false };
    let Some((p, why)) = s.r.skipped.get(index) else {
        return false;
    };
    out_str(p.as_os_str().as_bytes(), path, path_cap);
    out_str(why.as_bytes(), reason, reason_cap);
    true
}

/// Formatted byte count, matching the CLI's units.
#[no_mangle]
pub unsafe extern "C" fn ds_human(bytes: u64, buf: *mut c_char, cap: usize) -> usize {
    out_str(report::human(bytes).as_bytes(), buf, cap)
}

const _: () = {
    // Kind is mirrored as raw u8 in DsRow/DsRect; keep the mapping honest.
    assert!(Kind::Dir as u8 == 0);
    assert!(Kind::File as u8 == 1);
};

#[cfg(test)]
mod abi {
    use super::*;
    /// Print the layout the header must mirror. `cargo test -- --nocapture abi`
    #[test]
    fn layout() {
        macro_rules! p {
            ($t:ty) => {
                println!(
                    "{:16} size={:3} align={}",
                    stringify!($t),
                    std::mem::size_of::<$t>(),
                    std::mem::align_of::<$t>()
                )
            };
        }
        p!(DsSizes);
        p!(DsRow);
        p!(DsRect);
        p!(DsReclaim);
        p!(DsSummary);
        p!(DsReconcile);
    }
}
