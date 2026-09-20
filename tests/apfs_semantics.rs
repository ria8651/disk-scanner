//! Accounting tests against a real APFS volume.
//!
//! These build genuine clones with `clonefile(2)`, real hard links and real
//! sparse files, then assert what the scanner reports. They are the reason to
//! trust the numbers: every claim about CoW accounting is checked against the
//! filesystem rather than against a mock.

use disk_scanner::model::{Kind, ROOT};
use disk_scanner::{scan, Options};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

extern "C" {
    fn clonefile(
        src: *const libc::c_char,
        dst: *const libc::c_char,
        flags: libc::c_int,
    ) -> libc::c_int;
}

fn clone_file(src: &Path, dst: &Path) {
    let s = std::ffi::CString::new(src.as_os_str().as_encoded_bytes()).unwrap();
    let d = std::ffi::CString::new(dst.as_os_str().as_encoded_bytes()).unwrap();
    let rc = unsafe { clonefile(s.as_ptr(), d.as_ptr(), 0) };
    assert_eq!(
        rc,
        0,
        "clonefile failed: {}",
        std::io::Error::last_os_error()
    );
}

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn tmp(tag: &str) -> Tmp {
    let p = std::env::temp_dir().join(format!(
        "dscan-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    Tmp(p)
}

fn write_random(path: &Path, bytes: usize) {
    // Incompressible, so transparent compression cannot skew allocated size.
    let mut f = fs::File::create(path).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut left = bytes;
    while left > 0 {
        for c in buf.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            c.copy_from_slice(&x.to_ne_bytes()[..c.len()]);
        }
        let n = left.min(buf.len());
        f.write_all(&buf[..n]).unwrap();
        left -= n;
    }
    f.sync_all().unwrap();
}

fn run(root: &Path) -> disk_scanner::ScanResult {
    scan(
        root,
        Options {
            threads: 2,
            buf_size: 64 * 1024,
        },
        Arc::new(AtomicBool::new(false)),
        None,
    )
    .unwrap()
}

fn find<'a>(r: &'a disk_scanner::ScanResult, name: &str) -> &'a disk_scanner::Node {
    (0..r.tree.len() as u32)
        .map(|i| (i, r.tree.name_str(i).into_owned()))
        .find(|(_, n)| n == name)
        .map(|(i, _)| r.tree.node(i))
        .unwrap_or_else(|| panic!("no node named {name}"))
}

/// Sizes reported for plain files must agree with `std::fs::metadata`.
/// This is the regression test for the attrlist buffer parser: any drift in
/// field order or width shows up here as garbage.
#[test]
fn parser_agrees_with_metadata() {
    let t = tmp("parse");
    let mut expected = Vec::new();
    for (i, size) in [0usize, 1, 4095, 4096, 100_000].iter().enumerate() {
        let p = t.0.join(format!("f{i}.bin"));
        write_random(&p, *size);
        expected.push((format!("f{i}.bin"), *size as u64));
    }
    // A name with multibyte UTF-8, to exercise the attrreference path.
    let uni = t.0.join("ünïcode–ﬁle.bin");
    write_random(&uni, 1234);
    expected.push(("ünïcode–ﬁle.bin".to_string(), 1234));

    let r = run(&t.0);
    for (name, size) in expected {
        let n = find(&r, &name);
        assert_eq!(n.kind(), Kind::File, "{name} wrong kind");
        assert_eq!(n.size.logical, size, "{name} logical size mismatch");
        let meta = fs::metadata(t.0.join(&name)).unwrap();
        assert_eq!(n.size.logical, meta.len(), "{name} disagrees with metadata");
    }
}

/// Three clones of one 32 MiB file occupy 32 MiB, but ALLOCSIZE reports
/// 32 MiB *each*. The scanner must surface both facts: physical triple-counts,
/// while the shared bytes are attributed to clones rather than to snapshots.
#[test]
fn clones_are_identified_and_attributed() {
    let t = tmp("clone");
    const SZ: usize = 32 << 20;
    let orig = t.0.join("orig.bin");
    write_random(&orig, SZ);
    clone_file(&orig, &t.0.join("c1.bin"));
    clone_file(&orig, &t.0.join("c2.bin"));

    let r = run(&t.0);
    let root = r.tree.node(ROOT);

    // du-style physical counts every clone at full size.
    assert!(
        root.size.physical >= (SZ as u64) * 3,
        "expected >= 3x{SZ} physical, got {}",
        root.size.physical
    );

    // All three share, so almost nothing is exclusively owned...
    assert!(
        root.size.exclusive < (SZ as u64),
        "clones should leave little exclusive, got {}",
        root.size.exclusive
    );
    // ...and the sharing must be attributed to clones, not to a snapshot.
    assert!(
        root.size.shared_clones >= (SZ as u64) * 2,
        "expected clone-attributed sharing, got clones={} snapshot={}",
        root.size.shared_clones,
        root.size.shared_snapshot
    );

    // The family must be recognised as wholly contained in the scan.
    assert_eq!(
        r.stats.clone_members, 3,
        "all three should be clone members"
    );
    let whole: Vec<_> = r
        .families
        .values()
        .filter(|f| f.refcnt == 3 && f.seen == 3)
        .collect();
    assert_eq!(whole.len(), 1, "expected one complete 3-member family");
}

/// A clone that is partially rewritten takes on private blocks. This is the
/// case that makes exact CoW accounting impossible cheaply — APFS gives the
/// diverged file a fresh cloneid while it still shares most extents — so the
/// test pins the behaviour we rely on rather than an exact total.
#[test]
fn diverged_clone_gains_exclusive_bytes() {
    let t = tmp("diverge");
    const SZ: usize = 16 << 20;
    let orig = t.0.join("orig.bin");
    write_random(&orig, SZ);
    let c1 = t.0.join("c1.bin");
    clone_file(&orig, &c1);

    // Rewrite the first 4 MiB of the clone, breaking sharing for those blocks.
    {
        use std::os::unix::fs::FileExt;
        let f = fs::OpenOptions::new().write(true).open(&c1).unwrap();
        let buf = vec![0xABu8; 4 << 20];
        f.write_at(&buf, 0).unwrap();
        f.sync_all().unwrap();
    }

    let r = run(&t.0);
    let c = find(&r, "c1.bin");
    assert!(
        c.size.exclusive >= (4 << 20),
        "rewritten region should be exclusive, got {}",
        c.size.exclusive
    );
    assert!(
        c.size.shared_clones > 0,
        "the untouched remainder should still be shared"
    );
}

/// Hard links must be counted once. The second link is kept in the tree as a
/// visible alias with zero size, so totals stay correct without hiding a file.
#[test]
fn hardlinks_counted_once() {
    let t = tmp("hardlink");
    const SZ: usize = 8 << 20;
    let a = t.0.join("a.bin");
    write_random(&a, SZ);
    fs::hard_link(&a, t.0.join("b.bin")).unwrap();

    let r = run(&t.0);
    let root = r.tree.node(ROOT);

    assert_eq!(r.stats.files, 2, "both links should appear as files");
    assert_eq!(r.stats.hardlink_aliases, 1, "exactly one should be folded");
    assert!(
        root.size.physical < (SZ as u64) * 2,
        "hardlink double-counted: {}",
        root.size.physical
    );
    assert!(root.size.physical >= SZ as u64);

    let counted = ["a.bin", "b.bin"]
        .iter()
        .filter(|n| find(&r, n).size.physical > 0)
        .count();
    assert_eq!(counted, 1, "exactly one link should carry the bytes");
    let alias = ["a.bin", "b.bin"]
        .iter()
        .filter(|n| find(&r, n).hardlink_alias)
        .count();
    assert_eq!(alias, 1);
}

/// A sparse file claims far more logically than it allocates.
#[test]
fn sparse_file_reports_holes() {
    let t = tmp("sparse");
    let p = t.0.join("sparse.bin");
    {
        let f = fs::File::create(&p).unwrap();
        write_random(&p, 1 << 20);
        let f2 = fs::OpenOptions::new().write(true).open(&p).unwrap();
        f2.set_len(1 << 30).unwrap(); // 1 GiB logical
        f2.sync_all().unwrap();
        drop(f);
    }
    let r = run(&t.0);
    let n = find(&r, "sparse.bin");
    assert_eq!(n.size.logical, 1 << 30);
    assert!(
        n.size.physical < (16 << 20),
        "sparse file should allocate little, got {}",
        n.size.physical
    );
    assert!(n.size.sparse_saving > (1 << 29), "holes should be reported");
}

/// Rollup must be exact: every directory equals the sum of its children.
#[test]
fn rollup_is_exact() {
    let t = tmp("rollup");
    for d in ["a", "b", "a/x", "a/y"] {
        fs::create_dir_all(t.0.join(d)).unwrap();
    }
    for (i, d) in ["a", "b", "a/x", "a/y", "."].iter().enumerate() {
        write_random(&t.0.join(d).join(format!("f{i}.bin")), (i + 1) * 100_000);
    }

    let r = run(&t.0);
    let t2 = &r.tree;
    for id in 0..t2.len() as u32 {
        let n = t2.node(id);
        if !n.is_dir() {
            continue;
        }
        let mut sum = disk_scanner::Sizes::default();
        for c in n.children() {
            sum.add(&t2.node(c).size);
        }
        assert_eq!(n.size, sum, "rollup mismatch at {}", t2.path(id).display());
    }
}

/// Symlinks are never followed, so a self-referential link cannot loop.
#[test]
fn symlink_loops_are_safe() {
    let t = tmp("symlink");
    std::os::unix::fs::symlink(&t.0, t.0.join("loop")).unwrap();
    std::os::unix::fs::symlink("/", t.0.join("root")).unwrap();
    write_random(&t.0.join("real.bin"), 4096);

    let r = run(&t.0);
    assert_eq!(r.stats.symlinks, 2);
    assert_eq!(r.stats.dirs, 1, "no symlink should have been descended");
    assert_eq!(find(&r, "loop").kind(), Kind::Symlink);
}

/// Paths must round-trip, including at the root and for multibyte names.
#[test]
fn paths_round_trip() {
    let t = tmp("paths");
    fs::create_dir_all(t.0.join("sub/déep")).unwrap();
    write_random(&t.0.join("sub/déep/leaf.bin"), 1000);

    let r = run(&t.0);
    let leaf = (0..r.tree.len() as u32)
        .find(|&i| r.tree.name_str(i) == "leaf.bin")
        .unwrap();
    let p = r.tree.path(leaf);
    assert!(
        p.exists(),
        "reconstructed path does not exist: {}",
        p.display()
    );
    assert_eq!(fs::metadata(&p).unwrap().len(), 1000);
}
