//! Mount table enumeration and entry policy.
//!
//! The scanner decides what to enter by **device identity**, never by path
//! string. A path-prefix prune is one normalisation bug away from walking an
//! external backup drive: during development a `"/" + "/Volumes"` join
//! produced `"//Volumes"`, defeated an exact-match skip list, and sent a test
//! walker into 2 TB of Time Machine backups.

use super::ffi::*;
use std::ffi::CStr;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Mount {
    pub on: PathBuf,
    pub from: String,
    pub fstype: String,
    pub flags: u32,
    pub dev: libc::dev_t,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkipReason {
    NotLocal,
    Removable,
    SnapshotMount,
    Automounted,
    UnsupportedFs,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::NotLocal => "network or FUSE mount",
            SkipReason::Removable => "removable/external volume",
            SkipReason::SnapshotMount => "APFS snapshot mount",
            SkipReason::Automounted => "automounted (autofs)",
            SkipReason::UnsupportedFs => "unsupported filesystem",
        }
    }
}

impl Mount {
    /// Should the walker descend into this mount?
    ///
    /// `/` itself carries `MNT_SNAPSHOT` on a sealed system volume, so the
    /// snapshot test must exempt `MNT_ROOTFS` or we would refuse to scan at all.
    pub fn classify(&self) -> Result<(), SkipReason> {
        if self.flags & MNT_SNAPSHOT != 0 && self.flags & MNT_ROOTFS == 0 {
            return Err(SkipReason::SnapshotMount);
        }
        if self.flags & MNT_LOCAL == 0 {
            return Err(SkipReason::NotLocal);
        }
        if self.flags & MNT_AUTOMOUNTED != 0 {
            return Err(SkipReason::Automounted);
        }
        if self.flags & MNT_REMOVABLE != 0 {
            return Err(SkipReason::Removable);
        }
        match self.fstype.as_str() {
            "apfs" | "hfs" => Ok(()),
            _ => Err(SkipReason::UnsupportedFs),
        }
    }
}

fn cstr(buf: &[libc::c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// Snapshot of the current mount table.
pub fn mounts() -> std::io::Result<Vec<Mount>> {
    unsafe {
        let n = libc::getfsstat(std::ptr::null_mut(), 0, MNT_NOWAIT);
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Ask for headroom: the table can grow between the two calls.
        let cap = (n as usize) + 8;
        let mut v: Vec<libc::statfs> = Vec::with_capacity(cap);
        let got = libc::getfsstat(
            v.as_mut_ptr(),
            (cap * std::mem::size_of::<libc::statfs>()) as libc::c_int,
            MNT_NOWAIT,
        );
        if got < 0 {
            return Err(std::io::Error::last_os_error());
        }
        v.set_len(got as usize);

        Ok(v.iter()
            .map(|s| {
                let on = PathBuf::from(cstr(&s.f_mntonname));
                let mut st: libc::stat = std::mem::zeroed();
                let dev = {
                    let c = std::ffi::CString::new(on.as_os_str().as_encoded_bytes()).unwrap();
                    if libc::stat(c.as_ptr(), &mut st) == 0 {
                        st.st_dev
                    } else {
                        -1
                    }
                };
                Mount {
                    on,
                    from: cstr(&s.f_mntfromname),
                    fstype: cstr(&s.f_fstypename),
                    flags: s.f_flags,
                    dev,
                    total_bytes: s.f_blocks * s.f_bsize as u64,
                    free_bytes: s.f_bfree * s.f_bsize as u64,
                }
            })
            .collect())
    }
}

/// Bytes currently in use on the volume containing `path`, per the kernel.
/// Used to reconcile the walked total against reality.
pub fn volume_used(path: &std::path::Path) -> std::io::Result<(u64, u64)> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut s) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let total = s.f_blocks * s.f_bsize as u64;
        let used = (s.f_blocks - s.f_bfree) * s.f_bsize as u64;
        Ok((used, total))
    }
}
