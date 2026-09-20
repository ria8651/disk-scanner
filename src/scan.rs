//! Parallel directory walker.
//!
//! Speed here comes from threads, not from clever syscalls. Measured on macOS
//! 26: `getattrlistbulk` is only ~1.06x faster than `readdir + fstatat +
//! per-file getattrlist` for the same data. It is still the right call — one
//! code path, and it returns the CoW attributes `stat` cannot express — but
//! the win is parallelism, which measured 3.2x at 4 threads and flat beyond.

use crate::model::{Kind, NodeId, Sizes, State, Tree, NO_NODE, ROOT};
use crate::sys::attrlist::{BulkDir, Entry};
use crate::sys::{iopolicy, mounts};

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const DEDUPE_SHARDS: usize = 64;
const BULK_BUF: usize = 128 * 1024;

#[derive(Clone, Debug)]
pub struct Options {
    /// Worker threads. Measured plateau is 4 on a 6P+2E machine.
    pub threads: usize,
    /// Kernel buffer handed to `getattrlistbulk`.
    pub buf_size: usize,
}

impl Default for Options {
    fn default() -> Self {
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Options {
            threads: n.clamp(2, 4),
            buf_size: BULK_BUF,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    pub dirs: u64,
    pub files: u64,
    pub physical: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
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
    pub skipped_mounts: u64,
    pub elapsed: Duration,
}

/// One APFS clone family observed during the scan.
#[derive(Clone, Copy, Debug)]
pub struct Family {
    pub refcnt: u32,
    /// Members found inside this scan. When `seen == refcnt` the whole family
    /// is here, so deleting all of them would actually free `shared_bytes`.
    pub seen: u32,
    pub shared_bytes: u64,
}

pub struct ScanResult {
    pub tree: Tree,
    pub stats: Stats,
    pub root_path: PathBuf,
    /// Mounts deliberately not entered, with the reason.
    pub skipped: Vec<(PathBuf, &'static str)>,
    /// Clone families, keyed by `ATTR_CMNEXT_CLONEID`.
    pub families: HashMap<u64, Family>,
    /// Which clone family each clone member belongs to.
    ///
    /// A side table rather than a `Node` field on purpose: `Node` is already
    /// 96 bytes and size-guarded by a test, and clone members are a small
    /// minority of a whole-disk scan. Without this, `families` can be
    /// summarised globally but never attributed to a subtree or a selection,
    /// which is exactly what "what would deleting this actually free" needs.
    pub clone_of: HashMap<NodeId, u64>,
    pub policies: iopolicy::Policies,
    /// `statfs`-reported used bytes for the root volume, for reconciliation.
    pub volume_used: u64,
    pub volume_total: u64,
    /// True when the scan root is a mount point. Reconciling a walked total
    /// against volume usage is only meaningful for a whole volume; for a
    /// subdirectory the comparison is nonsense.
    pub root_is_volume_root: bool,
    pub cancelled: bool,
}

// ---------------------------------------------------------------------------

struct Dedupe {
    shards: Vec<Mutex<HashSet<u128>>>,
}

impl Dedupe {
    fn new() -> Self {
        Dedupe {
            shards: (0..DEDUPE_SHARDS)
                .map(|_| Mutex::new(HashSet::with_capacity(4096)))
                .collect(),
        }
    }
    /// True if this (volume, inode) pair had not been seen before.
    fn claim(&self, fsid: i64, fileid: u64) -> bool {
        let key = ((fsid as u64 as u128) << 64) | fileid as u128;
        let s = (fileid as usize) & (DEDUPE_SHARDS - 1);
        self.shards[s].lock().unwrap().insert(key)
    }
}

struct Families {
    shards: Vec<Mutex<HashMap<u64, Family>>>,
}

impl Families {
    fn new() -> Self {
        Families {
            shards: (0..DEDUPE_SHARDS)
                .map(|_| Mutex::new(HashMap::new()))
                .collect(),
        }
    }
    fn record(&self, cloneid: u64, refcnt: u32, shared: u64) {
        let s = (cloneid as usize) & (DEDUPE_SHARDS - 1);
        let mut g = self.shards[s].lock().unwrap();
        let e = g.entry(cloneid).or_insert(Family {
            refcnt: refcnt.max(1),
            seen: 0,
            shared_bytes: 0,
        });
        e.seen += 1;
        e.refcnt = e.refcnt.max(refcnt);
        // Members of a family can diverge; keep the largest observed share.
        e.shared_bytes = e.shared_bytes.max(shared);
    }
    fn drain(self) -> HashMap<u64, Family> {
        let mut out = HashMap::new();
        for s in self.shards {
            out.extend(s.into_inner().unwrap());
        }
        out
    }
}

/// A directory waiting to be expanded. The path is carried rather than
/// rebuilt from the tree, so workers never lock the arena just to walk up.
struct Task {
    id: NodeId,
    path: PathBuf,
}

#[derive(Default)]
struct Queue {
    stack: Vec<Task>,
    active: usize,
    quit: bool,
}

struct Shared {
    q: Mutex<Queue>,
    cv: Condvar,
    tree: Mutex<Tree>,
    dedupe: Dedupe,
    families: Families,
    clone_of: Mutex<HashMap<NodeId, u64>>,
    stats: Mutex<Stats>,
    allowed_devs: HashSet<libc::dev_t>,
    /// Mount points inside the root's own volume group. Their contents are
    /// already reachable through firmlinks, so entering them double-counts.
    group_mounts: HashSet<PathBuf>,
    opts: Options,
    cancel: Arc<AtomicBool>,
    p_dirs: AtomicU64,
    p_files: AtomicU64,
    p_bytes: AtomicU64,
}

/// Join without producing `"//name"` when the parent is `/`.
fn join(parent: &Path, name: &[u8]) -> PathBuf {
    let mut v = parent.as_os_str().as_bytes().to_vec();
    if !v.ends_with(b"/") {
        v.push(b'/');
    }
    v.extend_from_slice(name);
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(v))
}

fn sizes_of(e: &Entry) -> Sizes {
    let physical = e.alloc_size;
    let exclusive = e.private_size.min(physical);
    let shared = physical.saturating_sub(exclusive);
    // Verified discriminator: a snapshot-pinned file reports no EF_MAY_SHARE
    // and refcnt <= 1, while a real clone reports EF_MAY_SHARE and refcnt > 1.
    let (shared_clones, shared_snapshot) = if e.shares_with_clones() {
        (shared, 0)
    } else {
        (0, shared)
    };
    Sizes {
        logical: e.data_length,
        physical,
        exclusive,
        shared_clones,
        shared_snapshot,
        sparse_saving: if e.is_sparse() {
            e.data_length.saturating_sub(physical)
        } else {
            0
        },
    }
}

fn err_state(errno: i32) -> State {
    match errno {
        libc::EPERM => State::DeniedTcc,
        libc::EACCES => State::DeniedPerm,
        _ => State::Error,
    }
}

/// Open a directory without following symlinks and without triggering mounts.
fn open_dir(path: &Path) -> io::Result<libc::c_int> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

fn fstat_dev(fd: libc::c_int) -> Option<libc::dev_t> {
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) == 0 {
            Some(st.st_dev)
        } else {
            None
        }
    }
}

struct Pending {
    name: Vec<u8>,
    kind: Kind,
    state: State,
    detail: i32,
    flags: u32,
    ext_flags: u8,
    mtime: i64,
    size: Sizes,
    hardlink_alias: bool,
    descend: bool,
    /// `ATTR_CMNEXT_CLONEID`, or 0 when this entry is not a clone member.
    cloneid: u64,
}

impl Shared {
    /// Expand one directory: read its entries, decide what to descend into,
    /// and splice the children into the arena as one contiguous block.
    fn expand(&self, task: &Task) {
        let fd = match open_dir(&task.path) {
            Ok(fd) => fd,
            Err(e) => {
                self.mark_failed(task.id, e.raw_os_error().unwrap_or(0));
                return;
            }
        };

        // Device identity, not path strings, decides whether we may enter.
        if let Some(dev) = fstat_dev(fd) {
            if !self.allowed_devs.contains(&dev) {
                unsafe { libc::close(fd) };
                self.mark_skipped(task.id, 0);
                return;
            }
        }

        let mut children: Vec<Pending> = Vec::new();
        let mut local = Stats::default();
        // Count directories we actually read, so the root is included and the
        // figure matches "directories opened".
        local.dirs += 1;
        let mut local_bytes: u64 = 0;
        let mut bulk = BulkDir::new(fd, self.opts.buf_size);

        loop {
            match bulk.next_entry() {
                Ok(Some(e)) => {
                    if e.name.is_empty() || e.name == b"." || e.name == b".." {
                        continue;
                    }
                    if e.error != 0 {
                        local.errors += 1;
                        children.push(Pending {
                            name: e.name.clone(),
                            kind: Kind::Other,
                            state: err_state(e.error as i32),
                            detail: e.error as i32,
                            flags: 0,
                            ext_flags: 0,
                            mtime: 0,
                            size: Sizes::default(),
                            hardlink_alias: false,
                            descend: false,
                            cloneid: 0,
                        });
                        continue;
                    }
                    let p = self.classify(&e, &task.path, &mut local);
                    local_bytes += p.size.physical;
                    children.push(p);
                }
                Ok(None) => break,
                Err(e) => {
                    local.errors += 1;
                    let _ = e;
                    break;
                }
            }
        }
        unsafe { libc::close(fd) };

        // One lock acquisition per directory; children land contiguously.
        let mut subdirs: Vec<Task> = Vec::new();
        let mut clones: Vec<(NodeId, u64)> = Vec::new();
        {
            let mut tree = self.tree.lock().unwrap();
            let first = tree.len() as NodeId;
            let count = children.len() as u32;
            for c in &children {
                let id = tree.push(&c.name, task.id, c.kind, c.state);
                let n = &mut tree.nodes[id as usize];
                n.detail = c.detail;
                n.flags = c.flags;
                n.ext_flags = c.ext_flags;
                n.mtime = c.mtime;
                n.size = c.size;
                n.hardlink_alias = c.hardlink_alias;
                if c.cloneid != 0 {
                    clones.push((id, c.cloneid));
                }
                if c.descend {
                    subdirs.push(Task {
                        id,
                        path: join(&task.path, &c.name),
                    });
                }
            }
            let p = &mut tree.nodes[task.id as usize];
            p.first_child = if count == 0 { NO_NODE } else { first };
            p.child_count = count;
        }
        // Merged after the tree lock is released: clone members are a small
        // minority, so this lock is almost never contended.
        if !clones.is_empty() {
            self.clone_of.lock().unwrap().extend(clones);
        }

        self.p_dirs.fetch_add(local.dirs, Ordering::Relaxed);
        self.p_files.fetch_add(local.files, Ordering::Relaxed);
        self.p_bytes.fetch_add(local_bytes, Ordering::Relaxed);
        {
            let mut s = self.stats.lock().unwrap();
            s.dirs += local.dirs;
            s.files += local.files;
            s.symlinks += local.symlinks;
            s.hardlink_aliases += local.hardlink_aliases;
            s.dataless += local.dataless;
            s.compressed += local.compressed;
            s.sparse += local.sparse;
            s.clone_members += local.clone_members;
            s.denied_tcc += local.denied_tcc;
            s.denied_perm += local.denied_perm;
            s.errors += local.errors;
        }

        if !subdirs.is_empty() {
            let mut q = self.q.lock().unwrap();
            q.stack.extend(subdirs);
            self.cv.notify_all();
        }
    }

    fn classify(&self, e: &Entry, parent: &Path, local: &mut Stats) -> Pending {
        let ext = (e.ext_flags & 0xff) as u8;
        if e.is_dir() {
            let child_path = join(parent, &e.name);
            // A mount point inside our own volume group is reachable through
            // a firmlink elsewhere; entering it would double-count the volume.
            let is_group_mount = self.group_mounts.contains(&child_path);
            // Dedupe on (volume, inode). Deduping directories is what makes
            // firmlink double-counting impossible: if we never descend a
            // subtree twice, its files cannot be visited twice either.
            let fresh = self.dedupe.claim(e.realfsid, e.fileid);
            let descend = fresh && !is_group_mount;
            return Pending {
                name: e.name.clone(),
                kind: Kind::Dir,
                state: if descend { State::Ok } else { State::Skipped },
                detail: 0,
                flags: e.flags,
                ext_flags: ext,
                mtime: e.mtime,
                size: Sizes::default(),
                hardlink_alias: false,
                descend,
                cloneid: 0,
            };
        }

        if e.is_symlink() {
            local.symlinks += 1;
            return Pending {
                name: e.name.clone(),
                kind: Kind::Symlink,
                state: State::Ok,
                detail: 0,
                flags: e.flags,
                ext_flags: ext,
                mtime: e.mtime,
                size: Sizes::default(),
                hardlink_alias: false,
                descend: false,
                cloneid: 0,
            };
        }

        if !e.is_file() {
            return Pending {
                name: e.name.clone(),
                kind: Kind::Other,
                state: State::Ok,
                detail: 0,
                flags: e.flags,
                ext_flags: ext,
                mtime: e.mtime,
                size: Sizes::default(),
                hardlink_alias: false,
                descend: false,
                cloneid: 0,
            };
        }

        local.files += 1;
        if e.is_dataless() {
            local.dataless += 1;
        }
        if e.is_compressed() {
            local.compressed += 1;
        }
        if e.is_sparse() {
            local.sparse += 1;
        }

        // Only files with more than one link can be reached twice.
        let alias = if e.linkcount > 1 {
            !self.dedupe.claim(e.realfsid, e.fileid)
        } else {
            false
        };
        if alias {
            local.hardlink_aliases += 1;
        }

        let size = if alias { Sizes::default() } else { sizes_of(e) };

        let is_clone = !alias && e.shares_with_clones();
        if is_clone {
            local.clone_members += 1;
            self.families
                .record(e.cloneid, e.clone_refcnt, size.shared_clones);
        }

        Pending {
            name: e.name.clone(),
            kind: Kind::File,
            state: State::Ok,
            detail: 0,
            flags: e.flags,
            ext_flags: ext,
            mtime: e.mtime,
            size,
            hardlink_alias: alias,
            descend: false,
            cloneid: if is_clone { e.cloneid } else { 0 },
        }
    }

    fn mark_failed(&self, id: NodeId, errno: i32) {
        let st = err_state(errno);
        {
            let mut tree = self.tree.lock().unwrap();
            let n = &mut tree.nodes[id as usize];
            n.set_state(st);
            n.detail = errno;
        }
        let mut s = self.stats.lock().unwrap();
        match st {
            State::DeniedTcc => s.denied_tcc += 1,
            State::DeniedPerm => s.denied_perm += 1,
            _ => s.errors += 1,
        }
    }

    fn mark_skipped(&self, id: NodeId, reason: i32) {
        let mut tree = self.tree.lock().unwrap();
        let n = &mut tree.nodes[id as usize];
        n.set_state(State::Skipped);
        n.detail = reason;
    }
}

/// Walk `root`, returning the full tree.
///
/// `progress` is called from a monitor thread roughly ten times a second.
pub fn scan(
    root: &Path,
    opts: Options,
    cancel: Arc<AtomicBool>,
    progress: Option<Box<dyn Fn(Progress) + Send>>,
) -> io::Result<ScanResult> {
    let started = Instant::now();

    // Must happen before any worker thread exists: TRIGGER_RESOLVE is
    // process-scope only and returns EINVAL at thread scope.
    let policies = iopolicy::harden_process();

    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let table = mounts::mounts()?;
    let root_dev = {
        let c = std::ffi::CString::new(root.as_os_str().as_bytes())?;
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::stat(c.as_ptr(), &mut st) != 0 {
                return Err(io::Error::last_os_error());
            }
            st.st_dev
        }
    };

    let mut allowed_devs = HashSet::new();
    let mut group_mounts = HashSet::new();
    let mut skipped = Vec::new();
    for m in &table {
        match m.classify() {
            Ok(()) => {
                allowed_devs.insert(m.dev);
                if m.dev == root_dev && m.on != root {
                    group_mounts.insert(m.on.clone());
                }
            }
            Err(r) => skipped.push((m.on.clone(), r.as_str())),
        }
    }
    allowed_devs.insert(root_dev);

    let (volume_used, volume_total) = mounts::volume_used(&root).unwrap_or((0, 0));
    let root_is_volume_root = table.iter().any(|m| m.on == root);

    let mut tree = Tree::with_capacity(1 << 20);
    tree.push(root.as_os_str().as_bytes(), NO_NODE, Kind::Dir, State::Ok);

    let sh = Arc::new(Shared {
        q: Mutex::new(Queue {
            stack: vec![Task {
                id: ROOT,
                path: root.clone(),
            }],
            active: 0,
            quit: false,
        }),
        cv: Condvar::new(),
        tree: Mutex::new(tree),
        dedupe: Dedupe::new(),
        families: Families::new(),
        clone_of: Mutex::new(HashMap::new()),
        stats: Mutex::new(Stats::default()),
        allowed_devs,
        group_mounts,
        opts: opts.clone(),
        cancel: cancel.clone(),
        p_dirs: AtomicU64::new(0),
        p_files: AtomicU64::new(0),
        p_bytes: AtomicU64::new(0),
    });

    let mut handles = Vec::new();
    for _ in 0..opts.threads.max(1) {
        let sh = Arc::clone(&sh);
        handles.push(std::thread::spawn(move || worker(sh)));
    }

    // Progress monitor.
    let monitor = progress.map(|cb| {
        let sh = Arc::clone(&sh);
        std::thread::spawn(move || loop {
            {
                let q = sh.q.lock().unwrap();
                if q.quit {
                    break;
                }
            }
            cb(Progress {
                dirs: sh.p_dirs.load(Ordering::Relaxed),
                files: sh.p_files.load(Ordering::Relaxed),
                physical: sh.p_bytes.load(Ordering::Relaxed),
            });
            std::thread::sleep(Duration::from_millis(100));
        })
    });

    for h in handles {
        let _ = h.join();
    }
    {
        let mut q = sh.q.lock().unwrap();
        q.quit = true;
        sh.cv.notify_all();
    }
    if let Some(m) = monitor {
        let _ = m.join();
    }

    let sh = Arc::try_unwrap(sh).ok().expect("workers still running");
    let mut tree = sh.tree.into_inner().unwrap();
    tree.rollup();
    tree.shrink();

    let mut stats = sh.stats.into_inner().unwrap();
    stats.skipped_mounts = skipped.len() as u64;
    stats.elapsed = started.elapsed();

    Ok(ScanResult {
        tree,
        stats,
        root_path: root,
        skipped,
        families: sh.families.drain(),
        clone_of: sh.clone_of.into_inner().unwrap(),
        policies,
        volume_used,
        volume_total,
        root_is_volume_root,
        cancelled: cancel.load(Ordering::Relaxed),
    })
}

fn worker(sh: Arc<Shared>) {
    loop {
        let task = {
            let mut q = sh.q.lock().unwrap();
            loop {
                if sh.cancel.load(Ordering::Relaxed) {
                    q.quit = true;
                    sh.cv.notify_all();
                    return;
                }
                if let Some(t) = q.stack.pop() {
                    q.active += 1;
                    break t;
                }
                if q.active == 0 {
                    // No work queued and nobody producing: we are done.
                    q.quit = true;
                    sh.cv.notify_all();
                    return;
                }
                q = sh.cv.wait(q).unwrap();
            }
        };

        sh.expand(&task);

        let mut q = sh.q.lock().unwrap();
        q.active -= 1;
        // Wake everyone: either there is new work, or we just went idle and
        // the others need to observe that the scan is finished.
        sh.cv.notify_all();
    }
}

/// Number of distinct (volume, inode) pairs recorded — for diagnostics.
impl ScanResult {
    pub fn dedupe_note(&self) -> String {
        format!("{} hardlink aliases folded", self.stats.hardlink_aliases)
    }
}
