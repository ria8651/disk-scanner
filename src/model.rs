//! The scan result: an arena tree with APFS-aware size accounting.
//!
//! # Why three numbers and not one
//!
//! On APFS no single number answers "how big is this". Measured on a real
//! 500 GB Mac: apparent size totalled 9.2 TiB (sparse disk images), allocated
//! size 433 GiB, and exclusively-owned bytes just 16.6 GiB — because two
//! automatic Time Machine snapshots pin almost every block on the volume.
//! All three are correct answers to different questions, so the model carries
//! all three and never pretends there is one truth.

use std::path::PathBuf;

pub type NodeId = u32;
pub const ROOT: NodeId = 0;
pub const NO_NODE: NodeId = u32::MAX;

/// Size accounting for one node, or for a whole subtree after rollup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sizes {
    /// Apparent size (`ATTR_FILE_DATALENGTH`). What Finder's "size" shows.
    /// Wildly overstates sparse files and counts iCloud-evicted files that
    /// occupy nothing locally.
    pub logical: u64,
    /// Bytes allocated on disk (`ATTR_FILE_ALLOCSIZE`). This is the `du`
    /// answer. It counts every clone at full size, so a family of N clones
    /// is counted N times.
    pub physical: u64,
    /// Bytes this object alone owns (`ATTR_CMNEXT_PRIVATESIZE`) — freed if it
    /// were deleted right now. Goes to zero the moment anything else shares
    /// the blocks, including a snapshot.
    pub exclusive: u64,
    /// Of `physical - exclusive`: shared with APFS clones of this file.
    pub shared_clones: u64,
    /// Of `physical - exclusive`: pinned by a snapshot or the sealed system
    /// volume. Not reclaimable by deleting files at all.
    pub shared_snapshot: u64,
    /// `logical - physical` for sparse files: holes that were never written.
    pub sparse_saving: u64,
}

impl Sizes {
    pub fn add(&mut self, o: &Sizes) {
        self.logical += o.logical;
        self.physical += o.physical;
        self.exclusive += o.exclusive;
        self.shared_clones += o.shared_clones;
        self.shared_snapshot += o.shared_snapshot;
        self.sparse_saving += o.sparse_saving;
    }
    /// Total bytes this object shares with something else.
    pub fn shared(&self) -> u64 {
        self.shared_clones + self.shared_snapshot
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Dir = 0,
    File = 1,
    Symlink = 2,
    Other = 3,
}

/// Whether this node's contents are actually accounted for.
///
/// Unreadable subtrees are represented explicitly rather than as zero bytes:
/// a silently-plausible total is worse than a visible gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Fully scanned.
    Ok,
    /// Refused by TCC. The user can fix this by granting Full Disk Access.
    DeniedTcc,
    /// Refused by ownership/mode. Granting FDA would *not* help.
    DeniedPerm,
    /// Some other I/O error; payload is the errno.
    Error,
    /// Deliberately not entered (see `Node::detail` for the reason code).
    Skipped,
}

#[derive(Clone, Copy, Debug)]
pub struct Node {
    name_off: u32,
    name_len: u16,
    kind: u8,
    state: u8,
    /// errno for `Error`, or a `SkipReason` discriminant for `Skipped`.
    pub detail: i32,
    pub parent: NodeId,
    pub first_child: NodeId,
    pub child_count: u32,
    /// Number of `Denied`/`Skipped`/`Error` nodes at or below this one.
    /// Non-zero means this subtree's totals are a lower bound.
    pub unknown_below: u32,
    /// Raw `st_flags` (`SF_DATALESS`, `UF_COMPRESSED`, `SF_FIRMLINK`, ...).
    pub flags: u32,
    /// `EF_*` bits from `ATTR_CMNEXT_EXT_FLAGS`; all defined bits fit in a u8.
    pub ext_flags: u8,
    /// True when this is an additional hard link to an inode already counted
    /// elsewhere in the scan. Its sizes are zero so totals stay correct.
    pub hardlink_alias: bool,
    pub mtime: i64,
    /// Own size for files; whole-subtree total for directories after rollup.
    pub size: Sizes,
}

impl Node {
    #[inline]
    pub fn kind(&self) -> Kind {
        match self.kind {
            0 => Kind::Dir,
            1 => Kind::File,
            2 => Kind::Symlink,
            _ => Kind::Other,
        }
    }
    #[inline]
    pub fn state(&self) -> State {
        match self.state {
            0 => State::Ok,
            1 => State::DeniedTcc,
            2 => State::DeniedPerm,
            3 => State::Error,
            _ => State::Skipped,
        }
    }
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.kind == 0
    }
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.unknown_below == 0
    }
    #[inline]
    pub(crate) fn set_state(&mut self, s: State) {
        self.state = state_code(s);
    }
    #[inline]
    pub fn children(&self) -> std::ops::Range<NodeId> {
        if self.child_count == 0 {
            0..0
        } else {
            self.first_child..self.first_child + self.child_count
        }
    }
}

pub(crate) fn state_code(s: State) -> u8 {
    match s {
        State::Ok => 0,
        State::DeniedTcc => 1,
        State::DeniedPerm => 2,
        State::Error => 3,
        State::Skipped => 4,
    }
}

/// Arena of nodes plus an interned name blob.
///
/// Invariant relied upon by `rollup`: a node's index is always lower than any
/// of its children's, because children are appended when the parent is
/// expanded. That makes a single reverse pass a correct post-order traversal.
#[derive(Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    names: Vec<u8>,
}

impl Tree {
    pub fn new() -> Self {
        Tree {
            nodes: Vec::new(),
            names: Vec::new(),
        }
    }

    pub fn with_capacity(n: usize) -> Self {
        Tree {
            nodes: Vec::with_capacity(n),
            names: Vec::with_capacity(n * 16),
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn name(&self, id: NodeId) -> &[u8] {
        let n = &self.nodes[id as usize];
        let s = n.name_off as usize;
        &self.names[s..s + n.name_len as usize]
    }

    pub fn name_str(&self, id: NodeId) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.name(id))
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub(crate) fn push(&mut self, name: &[u8], parent: NodeId, kind: Kind, state: State) -> NodeId {
        let off = self.names.len() as u32;
        self.names.extend_from_slice(name);
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            name_off: off,
            name_len: name.len() as u16,
            kind: kind as u8,
            state: state_code(state),
            detail: 0,
            parent,
            first_child: NO_NODE,
            child_count: 0,
            unknown_below: 0,
            flags: 0,
            ext_flags: 0,
            hardlink_alias: false,
            mtime: 0,
            size: Sizes::default(),
        });
        id
    }

    /// Absolute path of a node, rebuilt by walking parent links.
    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut cur = id;
        loop {
            let n = &self.nodes[cur as usize];
            parts.push(self.name(cur));
            if n.parent == NO_NODE || n.parent == cur {
                break;
            }
            cur = n.parent;
        }
        parts.reverse();
        let mut out: Vec<u8> = Vec::with_capacity(64);
        for (i, p) in parts.iter().enumerate() {
            if i > 0 && !out.ends_with(b"/") {
                out.push(b'/');
            }
            out.extend_from_slice(p);
        }
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(out))
    }

    /// Accumulate every node's sizes into its ancestors.
    ///
    /// Single reverse pass; see the ordering invariant on `Tree`.
    pub fn rollup(&mut self) {
        for i in (1..self.nodes.len()).rev() {
            let (size, parent, unknown, incomplete) = {
                let n = &self.nodes[i];
                let incomplete = !matches!(n.state(), State::Ok) as u32;
                (n.size, n.parent, n.unknown_below, incomplete)
            };
            if parent == NO_NODE || parent as usize == i {
                continue;
            }
            let p = &mut self.nodes[parent as usize];
            p.size.add(&size);
            p.unknown_below += unknown + incomplete;
        }
    }

    /// Children of `id` sorted by a key, largest first. Convenience for CLIs.
    pub fn children_by<F: Fn(&Node) -> u64>(&self, id: NodeId, key: F) -> Vec<NodeId> {
        let n = self.node(id);
        let mut v: Vec<NodeId> = n.children().collect();
        v.sort_unstable_by_key(|&c| std::cmp::Reverse(key(self.node(c))));
        v
    }

    /// Release the growth slack a `Vec` leaves behind. Doubling can leave the
    /// arena ~2x larger than the data it holds, which on a whole-disk scan is
    /// hundreds of megabytes of nothing.
    pub fn shrink(&mut self) {
        self.nodes.shrink_to_fit();
        self.names.shrink_to_fit();
    }

    /// Rough resident cost of the arena, for reporting.
    pub fn memory_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>() + self.names.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_stays_compact() {
        // A whole-disk scan holds ~4.2M of these; guard against accidental bloat.
        assert!(
            std::mem::size_of::<Node>() <= 96,
            "Node grew to {} bytes",
            std::mem::size_of::<Node>()
        );
    }

    #[test]
    fn rollup_handles_deep_chain() {
        let mut t = Tree::new();
        t.push(b"/", NO_NODE, Kind::Dir, State::Ok);
        for i in 1..1000u32 {
            t.push(b"d", i - 1, Kind::Dir, State::Ok);
            t.nodes[i as usize].size.physical = 1;
        }
        t.rollup();
        assert_eq!(t.node(ROOT).size.physical, 999);
    }
}
