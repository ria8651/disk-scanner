//! Bringing a finished scan up to date without walking it again.
//!
//! # The one operation
//!
//! Every change the front end hears about — a file written, a folder moved to
//! the Trash, a download landing — reduces to "this folder's listing changed".
//! So there is exactly one primitive: re-list folder D. Its files are read
//! again from that one listing; its subfolders that already existed keep the
//! subtrees we have (their own changes arrive as their own events); only
//! subfolders that are new get walked.
//!
//! # Why it costs what it costs
//!
//! Children must stay contiguous in the arena, so D's new listing is appended
//! at the end and D is pointed at it. A kept subfolder's node is copied into
//! that block, which means re-parenting its direct children — nothing deeper.
//! The old block is left in place, unlinked: garbage, reclaimed by `compact`
//! once it outweighs the live tree.
//!
//! "Already existed" means the same folder, not the same name: a kept folder
//! is matched on name *and* `Node::ino`. A folder swapped for a new one of the
//! same name — `mv build build.old; mkdir build`, or an app saving a document
//! bundle by building it beside the old one and renaming — is walked fresh,
//! because no event will ever arrive for the files inside it.
//!
//! **This breaks the parent-before-child ordering** `Tree::rollup` relies on:
//! a kept folder's node moves to the end of the arena while its children stay
//! where they were. Nothing rolls up a finished scan, and `compact` restores
//! the order; anything that ever needs it in between must compact first.
//!
//! Node ids of D's children change, and compaction changes all of them. Ids
//! are therefore only stable *between* refreshes; anything holding one across
//! a refresh re-resolves it by path with `lookup`.
//!
//! # Split in two
//!
//! `prepare` does the I/O and only reads the scan, so it can run off the main
//! thread while the UI keeps reading. `apply` mutates and is quick; the caller
//! must ensure nothing else is reading while it runs. One folder per patch,
//! each prepared after the previous one applied (see `Patch`); a patch
//! prepared before another applied is refused.
//!
//! # What stays approximate
//!
//! Deleting one APFS clone raises its siblings' exclusive size. Those siblings
//! may live outside every refreshed folder, so their figures go stale until a
//! full rescan. `ScanResult::refreshes` lets the UI say so.
//!
//! Hardlinks are kept exact (see `relink`): a new link to an inode already
//! counted is folded to zero, and when the counted link goes, a surviving
//! alias takes the size over.

use crate::model::{Kind, NodeId, Sizes, State, NO_NODE, ROOT};
use crate::scan::{self, Options, ScanResult};

use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// One folder's re-listing, ready to graft. Built off-thread by `prepare`.
///
/// One folder, not a batch, on purpose: everything here is computed against
/// the tree as `prepare` saw it, and an earlier graft in the same batch would
/// change what that means — a folder walked in full by its parent's graft,
/// then grafted again from its own stale walk; a hardlink carrier folded into
/// by one item and dropped by the next. Applying each before preparing the
/// next makes every step see the one before it.
pub struct Patch {
    path: PathBuf,
    fresh: ScanResult,
    links: LinkPlan,
    /// `ScanResult::refreshes` when prepared. A patch is refused if anything
    /// was applied since.
    stamp: u32,
}

/// Hardlink questions a re-listing raises that the arena alone cannot answer
/// cheaply, answered in `prepare` — off the main thread, and checked against
/// the filesystem — so `apply` only follows paths. See `relink`.
#[derive(Default)]
struct LinkPlan {
    /// Inodes the walk counted that the scan has no live carrier for, but
    /// which a live link elsewhere in the tree already carries: that path.
    carriers: HashMap<u128, PathBuf>,
    /// Inodes whose carrier this re-listing drops while another link
    /// survives elsewhere as an alias: that path, and the size to hand over.
    heirs: HashMap<u128, (PathBuf, Sizes)>,
}


/// The node at `path`, if the scan has it.
///
/// With `nearest`, the deepest node on the way there instead — where to put
/// someone whose folder has just disappeared. Outside the scan root is `None`
/// either way.
pub fn lookup(r: &ScanResult, path: &Path, nearest: bool) -> Option<NodeId> {
    let rel = path.strip_prefix(&r.root_path).ok()?;
    let t = &r.tree;
    let mut cur = ROOT;
    for comp in rel.components() {
        let Component::Normal(name) = comp else {
            continue;
        };
        let name = name.as_bytes();
        match t.node(cur).children().find(|&c| t.name(c) == name) {
            Some(c) => cur = c,
            None => return nearest.then_some(cur),
        }
    }
    Some(cur)
}

/// The data volume's mount point. On a whole-disk scan everything under it
/// is reachable twice — `/Users` is a firmlink to `/System/Volumes/Data/Users`
/// — and the walk entered whichever it met first, skipping the other.
const DATA_VOLUME: &str = "/System/Volumes/Data";

/// The folder at `path` as the scan holds it, trying the firmlink twin when
/// the path as given leads into a skipped copy. A skipped folder is never
/// returned: re-listing it would count it twice. Nothing below one can be
/// found at all, since a skipped folder has no children.

fn resolve(r: &ScanResult, path: &Path) -> Option<NodeId> {
    let twin = match path.strip_prefix(DATA_VOLUME) {
        Ok(rest) => Path::new("/").join(rest),
        Err(_) => Path::new(DATA_VOLUME).join(path.strip_prefix("/").ok()?),
    };
    let usable = |p: &Path| {
        let id = lookup(r, p, false)?;
        let n = r.tree.node(id);
        (n.is_dir() && n.state() != State::Skipped).then_some(id)
    };
    usable(path).or_else(|| usable(&twin))
}

/// Whether `prepare` would re-list `path`. Cheap — a walk down the tree that
/// stops at the first missing name — so a queue of changes can drop folders
/// already gone (a deleted tree reports every folder in it) before paying
/// for a trip to a worker thread.
pub fn holds(r: &ScanResult, path: &Path) -> bool {
    resolve(r, path).is_some()
}

/// Re-list folder `dir`. Reads the scan, never writes it. `None` if the scan
/// does not hold it, or deliberately did not enter it: a skipped mount or
/// firmlink twin stays skipped, or a refresh would count it twice.
pub fn prepare(r: &ScanResult, dir: &Path) -> Option<Patch> {
    let id = resolve(r, dir)?;
    let n = r.tree.node(id);
    let keep: HashMap<Vec<u8>, u32> = n
        .children()
        .filter(|&c| r.tree.node(c).is_dir())
        .map(|c| (r.tree.name(c).to_vec(), r.tree.node(c).ino))
        .collect();
    let path = r.tree.path(id);
    let opts = Options {
        threads: 2,
        ..Options::default()
    };
    // Sized for a folder, not a disk: the walk's default reserves room for a
    // million nodes.
    let fresh = scan::walk(&path, opts, Arc::new(AtomicBool::new(false)), None, keep, 256).ok()?;
    let links = plan_links(r, id, &fresh);
    Some(Patch {
        path,
        fresh,
        links,
        stamp: r.refreshes,
    })
}

/// Answer `LinkPlan`'s questions for re-listing `d` as `fresh`.
///
/// Both need the other links of an inode, which the arena only knows by the
/// low 32 bits of the file id. So candidates come from one pass over the
/// arena, and each is confirmed with `lstat` — device and full inode — before
/// it is trusted. Nothing here runs unless the re-listing involves a hardlink.
fn plan_links(r: &ScanResult, d: NodeId, fresh: &ScanResult) -> LinkPlan {
    use std::os::unix::fs::MetadataExt;
    let mut plan = LinkPlan::default();
    let t = &r.tree;
    let fileid = |key: u128| key as u64;
    let stat = |p: &Path| std::fs::symlink_metadata(p).ok().map(|m| (m.dev(), m.ino()));

    // Carriers wanted: inodes the walk counted that `linked` has no live
    // record of — typically a file that gained its second link after the scan.
    let mut want_carrier: HashMap<u32, Vec<(u128, u64)>> = HashMap::new();
    for (&key, &k) in &fresh.linked {
        let known = r.linked.get(&key).is_some_and(|&o| t.is_live(o));
        if !known {
            if let Some((dev, _)) = stat(&fresh.tree.path(k)) {
                want_carrier.entry(key as u64 as u32).or_default().push((key, dev));
            }
        }
    }

    // Heirs wanted: carriers inside what this re-listing drops — d's old
    // children, minus folders carried across — for inodes it does not re-meet.
    let fresh_dirs: HashSet<(&[u8], u32)> = fresh
        .tree
        .node(ROOT)
        .children()
        .filter(|&c| fresh.tree.node(c).is_dir())
        .map(|c| (fresh.tree.name(c), fresh.tree.node(c).ino))
        .collect();
    let mut dropped: HashSet<NodeId> = HashSet::new();
    let mut stack: Vec<NodeId> = t
        .node(d)
        .children()
        .filter(|&c| !(t.node(c).is_dir() && fresh_dirs.contains(&(t.name(c), t.node(c).ino))))
        .collect();
    while let Some(id) = stack.pop() {
        dropped.insert(id);
        stack.extend(t.node(id).children());
    }
    let mut want_heir: HashMap<u32, Vec<(u128, Sizes)>> = HashMap::new();
    for (&key, &o) in &r.linked {
        if dropped.contains(&o) && !fresh.linked.contains_key(&key) {
            want_heir.entry(key as u64 as u32).or_default().push((key, t.node(o).size));
        }
    }
    if want_carrier.is_empty() && want_heir.is_empty() {
        return plan;
    }
    let d_dev = stat(&t.path(d)).map(|(dev, _)| dev);

    for id in 0..t.len() as NodeId {
        let n = t.node(id);
        if n.kind() != Kind::File || dropped.contains(&id) {
            continue;
        }
        if let Some(keys) = want_carrier.get(&n.ino) {
            if !n.hardlink_alias && t.is_live(id) {
                let p = t.path(id);
                if let Some(here) = stat(&p) {
                    for &(key, dev) in keys {
                        if here == (dev, fileid(key)) {
                            plan.carriers.entry(key).or_insert_with(|| p.clone());
                        }
                    }
                }
            }
        }
        if let Some(keys) = want_heir.get(&n.ino) {
            if n.hardlink_alias && t.is_live(id) {
                let p = t.path(id);
                if let Some(here) = stat(&p) {
                    for &(key, size) in keys {
                        if Some(here) == d_dev.map(|dev| (dev, fileid(key))) {
                            plan.heirs.entry(key).or_insert_with(|| (p.clone(), size));
                        }
                    }
                }
            }
        }
    }
    plan
}

/// Graft a re-listing into the scan. Returns false if it no longer applies:
/// the folder is gone, or the scan changed since `prepare`.
pub fn apply(r: &mut ScanResult, patch: Patch) -> bool {
    if patch.stamp != r.refreshes {
        return false;
    }
    let Some(d) = lookup(r, &patch.path, false) else {
        return false;
    };
    if !r.tree.node(d).is_dir() {
        return false;
    }
    graft(r, d, patch.fresh, patch.links);
    r.refreshes += 1;
    // What the reconciliation note compares against moved with the change.
    if let Ok((used, total)) = crate::sys::mounts::volume_used(&r.root_path) {
        r.volume_used = used;
        r.volume_total = total;
    }
    if r.garbage > r.tree.len() / 2 {
        compact(r);
    }
    true
}

fn contribution(n: &crate::model::Node) -> u32 {
    n.unknown_below + (n.state() != State::Ok) as u32
}

/// Replace `d`'s children with `fresh`'s, carrying kept subfolders across.
fn graft(r: &mut ScanResult, d: NodeId, fresh: ScanResult, links: LinkPlan) {
    let f = &fresh.tree;
    let before = *r.tree.node(d);

    let old: HashMap<Vec<u8>, NodeId> = before
        .children()
        .map(|c| (r.tree.name(c).to_vec(), c))
        .collect();

    // Fresh id k >= 1 lands at base + k - 1; the fresh root *is* d.
    let base = r.tree.len() as NodeId;
    let map = |k: NodeId| match k {
        ROOT => d,
        NO_NODE => NO_NODE,
        k => base + k - 1,
    };
    for k in 1..f.len() as NodeId {
        let mut n = *f.node(k);
        n.parent = map(n.parent);
        n.first_child = if n.child_count == 0 { NO_NODE } else { map(n.first_child) };
        r.tree.push_copy(n, f.name(k));
    }

    let froot = *f.node(ROOT);
    let new_children: Vec<NodeId> = froot.children().map(map).collect();
    let mut carried: HashSet<NodeId> = HashSet::new();
    let mut placeholders: HashSet<NodeId> = HashSet::new();
    for &c in &new_children {
        let Some(&o) = old.get(r.tree.name(c)) else {
            continue;
        };
        let on = *r.tree.node(o);
        let cn = *r.tree.node(c);
        // The walk did not enter a folder matching the keep set on name and
        // inode; the same test here picks out exactly those placeholders.
        if on.is_dir() && cn.is_dir() && on.ino == cn.ino {
            // A kept subfolder: same contents, new slot.
            let n = &mut r.tree.nodes[c as usize];
            n.first_child = on.first_child;
            n.child_count = on.child_count;
            n.size = on.size;
            n.unknown_below = on.unknown_below;
            n.set_state(on.state());
            n.detail = on.detail;
            for g in on.children() {
                r.tree.nodes[g as usize].parent = c;
            }
            let o_node = &mut r.tree.nodes[o as usize];
            o_node.child_count = 0;
            o_node.first_child = NO_NODE;
            carried.insert(o);
            placeholders.insert(c);
        }
    }

    // What the walk found, tallied from its nodes rather than taken from its
    // `stats`: those also count read errors no node carries, which would be
    // added again on every refresh and never taken out. Placeholders are the
    // carried folders, already counted.
    let mut found = scan::Stats::default();
    scan::tally(&froot, false, &mut found);
    for k in 1..f.len() as NodeId {
        if !placeholders.contains(&map(k)) {
            scan::tally(f.node(k), fresh.clone_of.contains_key(&k), &mut found);
        }
    }

    // Everything in the old block that was not carried across is gone from
    // the live tree. Forget its clone memberships so family counts stay true.
    // Their counts come out of the stats; the fresh walk's go in below. d
    // itself was re-read, so its own old contribution goes too.
    let mut gone = scan::Stats::default();
    scan::tally(&before, false, &mut gone);
    let mut stack: Vec<NodeId> = before.children().collect();
    while let Some(id) = stack.pop() {
        r.garbage += 1;
        let n = *r.tree.node(id);
        stack.extend(n.children());
        // A carried folder's old slot is garbage, but the folder lives on.
        if !carried.contains(&id) {
            scan::tally(&n, r.clone_of.contains_key(&id), &mut gone);
        }
        forget_clone(r, id, true);
    }

    for (k, cid) in fresh.clone_of {
        r.clone_of.insert(map(k), cid);
    }
    for (cid, nf) in fresh.families {
        let fam = r.families.entry(cid).or_insert(scan::Family {
            refcnt: nf.refcnt,
            seen: 0,
            shared_bytes: 0,
        });
        fam.seen += nf.seen;
        // The fresh read is the kernel's current count; the old one may
        // predate a deletion.
        fam.refcnt = nf.refcnt.max(1);
        fam.shared_bytes = fam.shared_bytes.max(nf.shared_bytes);
    }

    // Link d to its new block first: the hardlink pass below asks whether
    // the link counted so far is still live, and an old slot of d's is not.
    {
        let n = &mut r.tree.nodes[d as usize];
        n.first_child = if new_children.is_empty() { NO_NODE } else { new_children[0] };
        n.child_count = new_children.len() as u32;
    }

    relink(r, d, base, fresh.linked, links);

    // Recount d from its children. Their own figures are already rolled up:
    // fresh ones by the walk, carried ones by the original scan.
    let mut size = Sizes::default();
    let mut unknown = 0;
    for &c in &new_children {
        let n = r.tree.node(c);
        size.add(&n.size);
        unknown += contribution(n);
    }
    {
        let n = &mut r.tree.nodes[d as usize];
        n.size = size;
        n.unknown_below = unknown;
        n.set_state(froot.state());
        n.detail = froot.detail;
        n.mtime = n.mtime.max(froot.mtime);
    }

    // Move the difference up the chain.
    let after = *r.tree.node(d);
    let (was, now) = (contribution(&before), contribution(&after));
    let mut p = after.parent;
    let mut cur = d;
    while p != NO_NODE && p != cur {
        let n = &mut r.tree.nodes[p as usize];
        n.size.sub(&before.size);
        n.size.add(&after.size);
        n.unknown_below = (n.unknown_below + now).saturating_sub(was);
        cur = p;
        p = n.parent;
    }

    // `relink` already moved the counts for links it folded or promoted.
    r.stats.sub(&gone);
    r.stats.add(&found);
}

/// Settle which link of each multiply-linked inode carries its size, after
/// `d` has been re-listed. Runs once `d` points at its new block, so the old
/// one reads as dead. `base` is the first id the re-listing appended.
///
/// The walk counted every multiply-linked file it met. Where a live link
/// elsewhere already carries the inode — on record in `linked`, or found by
/// `prepare` — the fresh one is folded to zero and its size, which is the
/// inode's current size, moves to that carrier: a write through one link
/// changes them all, and only the written path gets an event. Where the
/// carrier was dropped with the old listing and another link survives, that
/// link takes the size over.
///
/// Only path lookups and walks up the parent chain: the searching was done
/// off the main thread in `plan_links`.
fn relink(r: &mut ScanResult, d: NodeId, base: NodeId, fresh: HashMap<u128, NodeId>, plan: LinkPlan) {
    let carries = |r: &ScanResult, o: NodeId, c: NodeId| {
        o != c && r.tree.is_live(o) && {
            let n = r.tree.node(o);
            n.kind() == Kind::File && !n.hardlink_alias
        }
    };
    for (&key, &k) in &fresh {
        let c = base + k - 1;
        let carrier = match r.linked.get(&key) {
            Some(&o) if carries(r, o, c) => Some(o),
            _ => plan
                .carriers
                .get(&key)
                .and_then(|p| lookup(r, p, false))
                .filter(|&o| carries(r, o, c)),
        };
        match carrier {
            Some(o) => {
                let now = r.tree.node(c).size;
                fold(r, d, c);
                resize(r, d, o, now);
                r.linked.insert(key, o);
            }
            None => {
                r.linked.insert(key, c);
            }
        }
    }
    for (key, (path, size)) in plan.heirs {
        if fresh.contains_key(&key) {
            continue;
        }
        let Some(h) = lookup(r, &path, false) else {
            continue;
        };
        if !(r.tree.is_live(h) && r.tree.node(h).hardlink_alias) {
            continue;
        }
        r.tree.nodes[h as usize].hardlink_alias = false;
        resize(r, d, h, size);
        r.linked.insert(key, h);
        r.stats.hardlink_aliases = r.stats.hardlink_aliases.saturating_sub(1);
        // `size` is the dropped link's last-known size, which may predate a
        // write. Re-listing the heir's folder reads the inode as it is now.
        let p = r.tree.node(h).parent;
        r.followups.push(r.tree.path(p));
    }
}

/// Make fresh node `c` an alias: zero it and take its size back out of the
/// fresh folders between it and `d`, which the walk rolled up with it.
fn fold(r: &mut ScanResult, d: NodeId, c: NodeId) {
    let s = r.tree.node(c).size;
    let n = &mut r.tree.nodes[c as usize];
    n.size = Sizes::default();
    n.hardlink_alias = true;
    // The walk recorded it as a clone member; an alias is not one. Nothing
    // was deleted, so the kernel's reference count stands.
    if forget_clone(r, c, false) {
        r.stats.clone_members = r.stats.clone_members.saturating_sub(1);
    }
    r.stats.hardlink_aliases += 1;
    let mut p = r.tree.node(c).parent;
    while p != d && p != NO_NODE {
        r.tree.nodes[p as usize].size.sub(&s);
        p = r.tree.node(p).parent;
    }
}

/// Set live node `id`'s size to `now`, moving the difference up its folders.
///
/// Stops at `d` when `id` is inside it: `d` is recounted from its children
/// afterwards, and that change moves up the chain with everything else.
/// Outside `d`, it goes all the way to the root.
fn resize(r: &mut ScanResult, d: NodeId, id: NodeId, now: Sizes) {
    let was = std::mem::replace(&mut r.tree.nodes[id as usize].size, now);
    if was == now {
        return;
    }
    let mut p = r.tree.node(id).parent;
    while p != NO_NODE {
        let n = &mut r.tree.nodes[p as usize];
        n.size.sub(&was);
        n.size.add(&now);
        if p == d || p == ROOT {
            break;
        }
        p = r.tree.node(p).parent;
    }
}

/// Drop `id`'s clone-family membership, if it has one. `deleted` also takes
/// one off the family's reference count — right for a node that left the
/// listing (a fresh walk re-reads the true count if it is still there), wrong
/// for one merely re-classified. Returns whether it was a member.
fn forget_clone(r: &mut ScanResult, id: NodeId, deleted: bool) -> bool {
    let Some(cid) = r.clone_of.remove(&id) else {
        return false;
    };
    if let Some(fam) = r.families.get_mut(&cid) {
        fam.seen = fam.seen.saturating_sub(1);
        if deleted {
            fam.refcnt = fam.refcnt.saturating_sub(1).max(1);
        }
        if fam.seen == 0 {
            r.families.remove(&cid);
        }
    }
    true
}

/// Drop unreachable slots and renumber. Invalidates every node id.
pub fn compact(r: &mut ScanResult) {
    let remap = r.tree.compact();
    r.clone_of = r
        .clone_of
        .drain()
        .filter_map(|(k, v)| {
            let n = *remap.get(k as usize)?;
            (n != NO_NODE).then_some((n, v))
        })
        .collect();
    r.linked = r
        .linked
        .drain()
        .filter_map(|(k, v)| {
            let n = *remap.get(v as usize)?;
            (n != NO_NODE).then_some((k, n))
        })
        .collect();
    r.garbage = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dscan-refresh-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    }

    fn full(root: &Path) -> ScanResult {
        scan::scan(root, Options::default(), Arc::new(AtomicBool::new(false)), None).unwrap()
    }

    /// Re-list `dirs` in order, one patch each as the app does, then any
    /// follow-ups that produces. Returns how many applied.
    fn settle(r: &mut ScanResult, dirs: &[PathBuf]) -> usize {
        let mut queue: Vec<PathBuf> = dirs.iter().rev().cloned().collect();
        let mut n = 0;
        while let Some(d) = queue.pop().or_else(|| r.followups.pop()) {
            if let Some(p) = prepare(r, &d) {
                n += apply(r, p) as usize;
            }
        }
        n
    }

    fn refresh(r: &mut ScanResult, dir: &Path) -> usize {
        settle(r, &[dir.to_path_buf()])
    }

    /// Everything a refresh touches must agree with a scan from scratch.
    fn assert_matches_fresh(r: &ScanResult) {
        assert_totals_match_fresh(r);
        let s = full(&r.root_path);
        for i in 1..s.tree.len() as NodeId {
            let p = s.tree.path(i);
            let id = lookup(r, &p, false).unwrap_or_else(|| panic!("missing {}", p.display()));
            assert!(r.tree.is_live(id));
            let (a, b) = (r.tree.node(id), s.tree.node(i));
            assert_eq!(a.size.logical, b.size.logical, "{}", p.display());
            assert_eq!(a.size.physical, b.size.physical, "{}", p.display());
            assert_eq!(a.child_count, b.child_count, "{}", p.display());
        }
    }

    /// Root totals and counts only. With hardlinks, which link carries the
    /// size depends on walk order, so per-folder figures may legitimately
    /// differ from a fresh scan while the totals may not.
    fn assert_totals_match_fresh(r: &ScanResult) {
        let s = full(&r.root_path);
        let (a, b) = (r.tree.node(ROOT), s.tree.node(ROOT));
        assert_eq!(a.size.logical, b.size.logical);
        assert_eq!(a.size.physical, b.size.physical);
        assert_eq!(a.unknown_below, b.unknown_below);
        let (x, y) = (&r.stats, &s.stats);
        assert_eq!(
            (x.files, x.dirs, x.symlinks, x.hardlink_aliases, x.clone_members),
            (y.files, y.dirs, y.symlinks, y.hardlink_aliases, y.clone_members),
            "files, dirs, symlinks, aliases, clone members"
        );
        assert_eq!((x.denied_tcc, x.denied_perm, x.errors), (y.denied_tcc, y.denied_perm, y.errors));
        // Every slot the root cannot reach is accounted for as garbage.
        let live = (0..r.tree.len() as NodeId).filter(|&i| r.tree.is_live(i)).count();
        assert_eq!(live + r.garbage, r.tree.len(), "garbage undercounted");
    }

    #[test]
    fn removal_and_addition() {
        let root = tmp("rm");
        fs::create_dir_all(root.join("a/deep/er")).unwrap();
        fs::write(root.join("a/deep/er/f"), vec![1u8; 70_000]).unwrap();
        fs::write(root.join("a/g"), vec![1u8; 30_000]).unwrap();
        fs::write(root.join("h"), vec![1u8; 5_000]).unwrap();
        let mut r = full(&root);
        let deep = lookup(&r, &root.join("a/deep/er/f"), false).unwrap();

        // Trash-like: a whole folder leaves, a new one arrives elsewhere.
        fs::rename(root.join("a/g"), root.join("g2")).unwrap();
        fs::create_dir_all(root.join("new/x")).unwrap();
        fs::write(root.join("new/x/y"), vec![1u8; 12_345]).unwrap();
        assert_eq!(refresh(&mut r, &root.join("a")), 1);
        assert_eq!(refresh(&mut r, &root), 1);

        assert!(lookup(&r, &root.join("a/g"), false).is_none());
        assert!(lookup(&r, &root.join("new/x/y"), false).is_some());
        // The untouched deep subtree was carried, not re-walked: same slot.
        assert_eq!(lookup(&r, &root.join("a/deep/er/f"), false), Some(deep));
        assert_matches_fresh(&r);

        compact(&mut r);
        assert_eq!(r.garbage, 0);
        assert_matches_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn file_grows_in_place() {
        let root = tmp("grow");
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/f"), vec![1u8; 1000]).unwrap();
        let mut r = full(&root);
        fs::write(root.join("d/f"), vec![1u8; 900_000]).unwrap();
        refresh(&mut r, &root.join("d"));
        assert_matches_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refreshing_a_vanished_folder_is_a_no_op() {
        let root = tmp("gone");
        fs::create_dir_all(root.join("d")).unwrap();
        let mut r = full(&root);
        fs::remove_dir_all(root.join("d")).unwrap();
        // Its own event may arrive before its parent's.
        assert_eq!(settle(&mut r, &[root.join("d"), root.clone()]), 1);
        assert!(lookup(&r, &root.join("d"), false).is_none());
        assert_matches_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn folder_replaced_under_the_same_name_is_walked_fresh() {
        let root = tmp("swap");
        fs::create_dir_all(root.join("d/build")).unwrap();
        fs::write(root.join("d/build/f"), vec![1u8; 500_000]).unwrap();
        let mut r = full(&root);
        // No event arrives for the files inside either folder: only the
        // listing of `d` changes.
        fs::rename(root.join("d/build"), root.join("d/build.old")).unwrap();
        fs::create_dir_all(root.join("d/build")).unwrap();
        fs::write(root.join("d/build/g"), vec![1u8; 7_000]).unwrap();
        refresh(&mut r, &root.join("d"));
        assert!(lookup(&r, &root.join("d/build/f"), false).is_none());
        assert!(lookup(&r, &root.join("d/build/g"), false).is_some());
        assert_matches_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn counts_survive_repeated_refreshes() {
        let root = tmp("count");
        for i in 0..5 {
            fs::create_dir_all(root.join(format!("d{i}/e"))).unwrap();
            fs::write(root.join(format!("d{i}/e/f")), b"x").unwrap();
        }
        let mut r = full(&root);
        for _ in 0..3 {
            refresh(&mut r, &root);
            refresh(&mut r, &root.join("d0"));
        }
        assert_matches_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn new_hardlink_is_not_counted_twice() {
        let root = tmp("link");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("a/f"), vec![1u8; 500_000]).unwrap();
        let mut r = full(&root);
        fs::hard_link(root.join("a/f"), root.join("b/g")).unwrap();
        refresh(&mut r, &root.join("b"));
        assert_totals_match_fresh(&r);
        // Re-listing the folder that holds the counted link must not hand
        // the size to nobody.
        refresh(&mut r, &root.join("a"));
        assert_totals_match_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn alias_replaced_by_an_unrelated_file_is_counted() {
        let root = tmp("alias");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("a/f"), vec![1u8; 500_000]).unwrap();
        fs::hard_link(root.join("a/f"), root.join("b/g")).unwrap();
        let mut r = full(&root);
        // An editor's atomic save: a new inode under the alias's name.
        fs::write(root.join("b/g.tmp"), vec![2u8; 300_000]).unwrap();
        fs::rename(root.join("b/g.tmp"), root.join("b/g")).unwrap();
        refresh(&mut r, &root.join("b"));
        assert_matches_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn writing_through_the_uncounted_link_updates_the_total() {
        use std::io::Write;
        let root = tmp("write");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("a/f"), vec![1u8; 100_000]).unwrap();
        fs::hard_link(root.join("a/f"), root.join("b/g")).unwrap();
        let mut r = full(&root);
        // Write through whichever link the walk did not count. Only that
        // path's folder gets an event.
        let g = lookup(&r, &root.join("b/g"), false).unwrap();
        let (dir, file) = if r.tree.node(g).hardlink_alias { ("b", "b/g") } else { ("a", "a/f") };
        let mut fh = fs::OpenOptions::new().append(true).open(root.join(file)).unwrap();
        fh.write_all(&vec![2u8; 5_000_000]).unwrap();
        drop(fh);
        refresh(&mut r, &root.join(dir));
        assert_totals_match_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn new_link_written_before_the_refresh_is_counted_once() {
        use std::io::Write;
        let root = tmp("newwrite");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("a/f"), vec![1u8; 100_000]).unwrap();
        let mut r = full(&root);
        fs::hard_link(root.join("a/f"), root.join("b/g")).unwrap();
        let mut fh = fs::OpenOptions::new().append(true).open(root.join("b/g")).unwrap();
        fh.write_all(&vec![2u8; 5_000_000]).unwrap();
        drop(fh);
        refresh(&mut r, &root.join("b"));
        assert_totals_match_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn replaced_folder_and_its_parent_in_either_order() {
        for order in [["p", "p/x"], ["p/x", "p"]] {
            let root = tmp(&format!("both-{}", order[0].len()));
            fs::create_dir_all(root.join("p/x")).unwrap();
            fs::write(root.join("p/x/old"), vec![1u8; 4_000]).unwrap();
            let mut r = full(&root);
            fs::rename(root.join("p/x"), root.join("p/x.old")).unwrap();
            fs::create_dir_all(root.join("p/x/new")).unwrap();
            for i in 0..10 {
                fs::write(root.join(format!("p/x/new/{i}")), vec![1u8; 1_000]).unwrap();
            }
            settle(&mut r, &order.map(|d| root.join(d)));
            assert_matches_fresh(&r);
            fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn hardlinked_folder_moved_between_siblings_keeps_its_size() {
        let root = tmp("mvlink");
        fs::create_dir_all(root.join("a/sub")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("a/sub/f1"), vec![1u8; 300_000]).unwrap();
        fs::hard_link(root.join("a/sub/f1"), root.join("a/sub/f2")).unwrap();
        let mut r = full(&root);
        fs::rename(root.join("a/sub"), root.join("c/sub")).unwrap();
        // Destination first: the order that used to lose the inode.
        settle(&mut r, &[root.join("c"), root.join("a")]);
        assert_totals_match_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn heir_gets_the_inodes_current_size() {
        use std::io::Write;
        let root = tmp("heir");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("a/f"), vec![1u8; 100_000]).unwrap();
        fs::hard_link(root.join("a/f"), root.join("b/g")).unwrap();
        let mut r = full(&root);
        // Grow it through the counted link, then remove that link: only the
        // counted link's folder hears about either.
        let g = lookup(&r, &root.join("b/g"), false).unwrap();
        let (dir, file) = if r.tree.node(g).hardlink_alias { ("a", "a/f") } else { ("b", "b/g") };
        let mut fh = fs::OpenOptions::new().append(true).open(root.join(file)).unwrap();
        fh.write_all(&vec![2u8; 5_000_000]).unwrap();
        drop(fh);
        fs::remove_file(root.join(file)).unwrap();
        refresh(&mut r, &root.join(dir));
        assert_totals_match_fresh(&r);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stale_patch_is_refused() {
        let root = tmp("stale");
        fs::create_dir_all(root.join("a")).unwrap();
        let mut r = full(&root);
        let p1 = prepare(&r, &root).unwrap();
        let p2 = prepare(&r, &root.join("a")).unwrap();
        assert!(apply(&mut r, p1));
        assert!(!apply(&mut r, p2));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn nearest_falls_back_to_ancestor() {
        let root = tmp("near");
        fs::create_dir_all(root.join("a")).unwrap();
        let r = full(&root);
        let a = lookup(&r, &root.join("a"), false).unwrap();
        assert_eq!(lookup(&r, &root.join("a/b/c"), true), Some(a));
        assert_eq!(lookup(&r, &root.join("a/b/c"), false), None);
        assert_eq!(lookup(&r, Path::new("/definitely/elsewhere"), true), None);
        fs::remove_dir_all(&root).unwrap();
    }
}
