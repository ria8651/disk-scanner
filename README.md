# disk-scanner

A macOS disk scanner that tells the truth about APFS.

Most scanners answer "how big is this folder?" with one number. On APFS that
question has no single answer, and the ways it goes wrong are not edge cases —
they are the normal state of a Mac. This crate measures the difference instead
of averaging it away.

Status: **MVP — scanning engine and data model only.** No UI, no deletion.

## The problem, measured

Every claim below was measured on a real 500 GB Apple-silicon Mac running
macOS 26.6, not recalled from documentation. The probes live in the commit
history of this README's development and the assertions are encoded in
`tests/apfs_semantics.rs`.

**Clones make `du` lie.** Three `clonefile()` clones of a 64 MiB file report
`ALLOCSIZE = 64 MiB` *each* — 192 MiB apparent, 4 KiB actually consumed.
`st_blocks` agrees with the lie. Only `ATTR_CMNEXT_PRIVATESIZE` tells the truth.

**Snapshots make "reclaimable" meaningless — sometimes.** macOS keeps automatic
local Time Machine snapshots. Every file older than the newest snapshot reports
`PRIVATESIZE = 0`, because deleting it genuinely frees nothing until the
snapshot expires. Measured on the same disk hours apart:

| | with 2 local snapshots | after they rotated out |
|---|---|---|
| allocated | 433 GiB | 469 GiB |
| exclusively owned | **16.6 GiB** | **416 GiB** |

Same disk. Same files. A scanner reporting a single "reclaimable" number would
have swung by 25x for reasons that have nothing to do with the user's files.

**Apparent size is worse.** Summed logical size on that disk: **9.0 TiB**, on a
500 GB volume, because of sparse disk images.

**`st_dev` cannot tell the System volume from the Data volume.** Both report
`16777232`. So `find -xdev` does *not* stop at `/System/Volumes/Data`, and a
naive walk from `/` counts the entire data volume twice.

**iCloud files download if you touch them wrong.** 6092 dataless files in one
home directory. `open()` is safe; **`read()` is the trigger**. With the right
I/O policy set, a stray read fails with `EDEADLK` instead of silently pulling
gigabytes over someone's connection.

## What it reports

Every node carries six numbers, and the split is the point:

| field | meaning |
|---|---|
| `logical` | apparent size. Overstates sparse and iCloud-evicted files. |
| `physical` | allocated bytes (`du`). Counts each clone at full size. |
| `exclusive` | bytes freed by deleting *this object right now*. |
| `shared_clones` | of the remainder: shared with APFS clones. |
| `shared_snapshot` | of the remainder: pinned by a snapshot or the volume seal. |
| `sparse_saving` | holes never written. |

Separating the last two is what makes the output actionable: "12 GB shared" is
a shrug, "9 GB pinned by Time Machine, 3 GB cloned" is a decision. The
discriminator was verified experimentally — a snapshot-pinned file reports
`ext_flags == 0` and `clone_refcnt == 1`, a real clone reports
`EF_MAY_SHARE_BLOCKS` and `refcnt > 1`.

Unreadable subtrees are represented explicitly (`State::DeniedTcc`,
`DeniedPerm`, `Skipped`, `Error`) and an `unknown_below` count rolls up the
tree, so no total silently pretends to be complete. `EPERM` (TCC — the user can
fix it) is distinguished from `EACCES` (ownership — they cannot).

## Usage

```bash
cargo build --release
./sign.sh                    # see "Full Disk Access" below — do this once
./target/release/dscan / -n 20
```

```rust
use disk_scanner::{scan, Options, model::ROOT};
use std::sync::{Arc, atomic::AtomicBool};

let cancel = Arc::new(AtomicBool::new(false));
let r = scan("/".as_ref(), Options::default(), cancel, None)?;

let root = r.tree.node(ROOT);
println!("allocated {}", root.size.physical);
println!("of which pinned by snapshots {}", root.size.shared_snapshot);
if root.unknown_below > 0 {
    println!("incomplete: {} unreadable nodes", root.unknown_below);
}
```

## Full Disk Access

FDA can be **detected but never requested** — there is no API, only System
Settings. `disk_scanner::full_disk_access()` probes
`/Library/Application Support/com.apple.TCC/TCC.db` and branches on `EPERM`
specifically, since `EACCES` means ownership and telling the user to grant FDA
would send them chasing the wrong thing.

**Run `./sign.sh` before granting FDA.** TCC stores the code-signing
requirement at the moment of the grant and re-checks it on every access.
Ad-hoc signing (`codesign -s -`) pins to the binary's cdhash, so the grant
stops applying after the next `cargo build` — while System Settings still
shows the toggle as ON. Signing with a stable certificate binds to the
certificate instead, and the grant survives rebuilds.

For a CLI the grant belongs to the **hosting terminal**, not to this binary,
because TCC evaluates the responsible process. `responsible_app_hint()` walks
the process tree to the nearest `.app` to tell the user which one to add.

## Design notes worth knowing before editing

**The attrlist parser is driven by `returned_attrs`, never by fixed offsets.**
Three traps, all verified against `lstat` (924/924 entries, zero mismatches):

1. `ATTR_CMN_ERROR` is returned immediately after `ATTR_CMN_RETURNED_ATTRS`,
   out of bit order.
2. `FSOPT_PACK_INVAL_ATTRS` does **not** give a fixed layout — the whole
   directory-attribute group is absent on file entries and vice versa. A
   fixed-offset parser corrupts every row.
3. `ATTR_CMNEXT_CLONE_REFCNT` is a **`u32`**; `ATTR_CMNEXT_EXT_FLAGS` is a
   **`u64`**. Getting both wrong consumes the same 12 bytes, so the bug hides
   behind a parse that still appears to validate.

**Speed comes from threads, not from `getattrlistbulk`.** Measured:
`getattrlistbulk` is only **1.06x** faster than `readdir + fstatat +
per-file getattrlist` for the same data. It is still the right choice — one
code path, and it returns CoW attributes `stat` cannot express — but the
parallelism is the win: **3.2x at 4 threads**, flat beyond. Default is
`min(cores, 4)`.

**Traversal is gated by device identity, never by path strings.** During
development a `"/" + "/Volumes"` join produced `"//Volumes"`, defeated an
exact-match skip list, and sent a test walker into 2 TB of Time Machine
backups on an external drive. Path prunes are one normalisation bug away from
disaster; the mount table and `(volume, inode)` dedupe are the real invariant.

**Deduping *directories* is what prevents firmlink double-counting.** If a
subtree is never descended twice, its files cannot be visited twice, so the
dedupe set only needs directories plus files with `nlink > 1` — 705k keys
(~16 MB) instead of 4.0M (~92 MB) at identical correctness.

**Process I/O policies must be set before threads spawn.**
`IOPOL_TYPE_VFS_TRIGGER_RESOLVE` is process-scope only and returns `EINVAL` at
thread scope, so a per-worker call protects nothing.

## Known limitations

- **Partially-diverged clones defeat exact accounting.** When a clone is
  partially rewritten, APFS gives it a *fresh* `cloneid` while it still shares
  most of its extents. Family totals are therefore a **bound**, not an exact
  figure. Exact answers need extent-level enumeration (`F_LOG2PHYS_EXT`), which
  requires opening every file and is far slower.
- **Not verified:** whether `opendir` on a TCC-protected folder fails outright
  or succeeds with only per-file `open()` denied. The development machine had
  FDA granted, so no denial could be produced. The scanner assumes the
  conservative case (subtree opaque, marked `Denied`), which is correct either
  way, but the optimistic case would let us report sizes without the grant.
  Worth testing from a terminal without FDA.
- Whole-disk arena is ~400-500 MB for 4.2M nodes. `Node` is size-guarded by a
  test; the obvious further trim is moving the three `shared_*` fields to a
  directory-only side table.
- macOS only. The `sys` module is the platform boundary; `model.rs` is portable.
- Externals, network mounts, snapshot mounts, autofs and devfs are not entered.

## Tests

`cargo test` builds **real** clones with `clonefile(2)`, real hard links and
real sparse files on the actual filesystem, then asserts what the scanner
reports. Nothing about CoW accounting is checked against a mock.
