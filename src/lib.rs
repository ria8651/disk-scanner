//! A macOS disk scanner that tells the truth about APFS.
//!
//! Most scanners answer "how big is this folder" with a single number. On
//! APFS that question has no single answer: clones make several files share
//! one set of blocks, snapshots pin blocks that no live file accounts for,
//! sparse files claim space they never took, and iCloud-evicted files report
//! a size while occupying nothing. This crate measures all of it and keeps
//! the distinctions instead of averaging them away.
//!
//! ```no_run
//! use disk_scanner::{scan, Options};
//! use std::sync::{Arc, atomic::AtomicBool};
//!
//! let cancel = Arc::new(AtomicBool::new(false));
//! let r = scan("/".as_ref(), Options::default(), cancel, None).unwrap();
//! let root = r.tree.node(disk_scanner::model::ROOT);
//! println!("{} files, {} bytes allocated", r.stats.files, root.size.physical);
//! ```

#![cfg(target_os = "macos")]

pub mod ffi;
pub mod model;
pub mod report;
pub mod scan;
pub mod sys;

pub use model::{Kind, Node, NodeId, Sizes, State, Tree};
pub use scan::{scan, Options, Progress, ScanResult, Stats};
pub use sys::tcc::{full_disk_access, Access};
