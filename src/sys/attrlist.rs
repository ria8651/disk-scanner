//! `getattrlistbulk` wrapper and buffer parser.
//!
//! Three things about this buffer format are easy to get wrong, and all three
//! were confirmed empirically against `lstat` on macOS 26 (924/924 entries in
//! `/usr/bin`, zero mismatches):
//!
//! 1. `ATTR_CMN_ERROR` is written **immediately after** `ATTR_CMN_RETURNED_ATTRS`,
//!    out of bit order. The `getattrlistbulk(2)` man page states this.
//! 2. `FSOPT_PACK_INVAL_ATTRS` does **not** give you a fixed layout. The whole
//!    directory-attribute group is absent on file entries and vice versa, so
//!    a fixed-offset parser silently mis-reads every row. Presence is driven
//!    by `returned_attrs`, which is what this parser does.
//! 3. `ATTR_CMNEXT_CLONE_REFCNT` is a `u32`; `ATTR_CMNEXT_EXT_FLAGS` is a `u64`.
//!    Reading both as `u64` happens to consume the same 12 bytes, so the bug
//!    hides behind a parse that still validates.
//!
//! Fields are read unaligned: the buffer is packed and offsets are not
//! guaranteed to satisfy the natural alignment of `u64`/`off_t`.

use super::ffi::*;
use std::io;
use std::os::unix::io::RawFd;

/// The attribute set we ask for. Kept in one place so the request and the
/// parser cannot drift apart.
pub const REQ_COMMON: u32 = ATTR_CMN_RETURNED_ATTRS
    | ATTR_CMN_NAME
    | ATTR_CMN_ERROR
    | ATTR_CMN_DEVID
    | ATTR_CMN_OBJTYPE
    | ATTR_CMN_MODTIME
    | ATTR_CMN_FLAGS
    | ATTR_CMN_FILEID;
pub const REQ_DIR: u32 = ATTR_DIR_ENTRYCOUNT;
pub const REQ_FILE: u32 =
    ATTR_FILE_LINKCOUNT | ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE | ATTR_FILE_DATALENGTH;
pub const REQ_FORK: u32 = ATTR_CMNEXT_PRIVATESIZE
    | ATTR_CMNEXT_LINKID
    | ATTR_CMNEXT_REALFSID
    | ATTR_CMNEXT_CLONEID
    | ATTR_CMNEXT_EXT_FLAGS
    | ATTR_CMNEXT_CLONE_REFCNT;

const OPTS: u64 = (FSOPT_PACK_INVAL_ATTRS | FSOPT_ATTR_CMN_EXTENDED | FSOPT_NOFOLLOW) as u64;

/// One directory entry, with only the fields the kernel said it filled in.
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub name: Vec<u8>,
    pub error: u32,
    pub objtype: u32,
    pub devid: i32,
    pub mtime: i64,
    pub flags: u32,
    pub fileid: u64,
    pub entrycount: u32,
    pub linkcount: u32,
    pub total_size: u64,
    pub alloc_size: u64,
    pub data_length: u64,
    pub private_size: u64,
    pub linkid: u64,
    pub realfsid: i64,
    pub cloneid: u64,
    pub ext_flags: u64,
    pub clone_refcnt: u32,
}

impl Entry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.objtype == VDIR
    }
    #[inline]
    pub fn is_file(&self) -> bool {
        self.objtype == VREG
    }
    #[inline]
    pub fn is_symlink(&self) -> bool {
        self.objtype == VLNK
    }
    #[inline]
    pub fn is_dataless(&self) -> bool {
        self.flags & SF_DATALESS != 0
    }
    #[inline]
    pub fn is_firmlink(&self) -> bool {
        self.flags & SF_FIRMLINK != 0
    }
    #[inline]
    pub fn is_compressed(&self) -> bool {
        self.flags & UF_COMPRESSED != 0
    }
    #[inline]
    pub fn is_sparse(&self) -> bool {
        self.ext_flags & EF_IS_SPARSE != 0
    }
    /// True when this file shares extents with *clones*, as opposed to merely
    /// being pinned by a snapshot. Verified discriminator: a snapshot-pinned
    /// file reports `private == 0`, `clone_refcnt == 1`, `ext_flags == 0`,
    /// while a real clone reports `EF_MAY_SHARE_BLOCKS` and `refcnt > 1`.
    #[inline]
    pub fn shares_with_clones(&self) -> bool {
        self.ext_flags & (EF_MAY_SHARE_BLOCKS | EF_SHARES_ALL_BLOCKS) != 0 || self.clone_refcnt > 1
    }
}

struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    #[inline]
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
    #[inline]
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_ne_bytes(self.take(4)?.try_into().ok()?))
    }
    #[inline]
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_ne_bytes(self.take(4)?.try_into().ok()?))
    }
    #[inline]
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_ne_bytes(self.take(8)?.try_into().ok()?))
    }
    #[inline]
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_ne_bytes(self.take(8)?.try_into().ok()?))
    }
    #[inline]
    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }
}

/// Parse one packed entry. `buf` must start at the entry's length field.
/// Returns the entry and its total length in bytes.
fn parse_entry(buf: &[u8]) -> Option<(Entry, usize)> {
    let mut c = Cur { b: buf, p: 0 };
    let entry_len = c.u32()? as usize;
    if entry_len < 4 || entry_len > buf.len() {
        return None;
    }

    let r = attribute_set {
        commonattr: c.u32()?,
        volattr: c.u32()?,
        dirattr: c.u32()?,
        fileattr: c.u32()?,
        forkattr: c.u32()?,
    };

    let mut e = Entry::default();

    // (1) ERROR first, out of bit order.
    if r.commonattr & ATTR_CMN_ERROR != 0 {
        e.error = c.u32()?;
    }

    // (2) remaining common attributes, in bit order.
    if r.commonattr & ATTR_CMN_NAME != 0 {
        let at = c.p;
        let off = c.i32()?;
        let len = c.u32()? as usize;
        // attr_dataoffset is relative to the attrreference itself.
        let start = at.checked_add_signed(off as isize)?;
        let bytes = buf.get(start..start.checked_add(len)?)?;
        // Trim the trailing NUL the kernel includes in attr_length.
        let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
        e.name = bytes.to_vec();
    }
    if r.commonattr & ATTR_CMN_DEVID != 0 {
        e.devid = c.i32()?;
    }
    if r.commonattr & ATTR_CMN_OBJTYPE != 0 {
        e.objtype = c.u32()?;
    }
    if r.commonattr & ATTR_CMN_MODTIME != 0 {
        e.mtime = c.i64()?; // struct timespec: tv_sec then tv_nsec
        c.skip(8)?;
    }
    if r.commonattr & ATTR_CMN_FLAGS != 0 {
        e.flags = c.u32()?;
    }
    if r.commonattr & ATTR_CMN_FILEID != 0 {
        e.fileid = c.u64()?;
    }

    // (3) directory group — present only for directories.
    if r.dirattr & ATTR_DIR_ENTRYCOUNT != 0 {
        e.entrycount = c.u32()?;
    }

    // (4) file group — present only for regular files.
    if r.fileattr & ATTR_FILE_LINKCOUNT != 0 {
        e.linkcount = c.u32()?;
    }
    if r.fileattr & ATTR_FILE_TOTALSIZE != 0 {
        e.total_size = c.u64()?;
    }
    if r.fileattr & ATTR_FILE_ALLOCSIZE != 0 {
        e.alloc_size = c.u64()?;
    }
    if r.fileattr & ATTR_FILE_DATALENGTH != 0 {
        e.data_length = c.u64()?;
    }

    // (5) CMNEXT group, in bit order. Widths measured, not assumed.
    if r.forkattr & ATTR_CMNEXT_PRIVATESIZE != 0 {
        e.private_size = c.u64()?;
    }
    if r.forkattr & ATTR_CMNEXT_LINKID != 0 {
        e.linkid = c.u64()?;
    }
    if r.forkattr & ATTR_CMNEXT_REALFSID != 0 {
        e.realfsid = c.i64()?; // fsid_t: two int32
    }
    if r.forkattr & ATTR_CMNEXT_CLONEID != 0 {
        e.cloneid = c.u64()?;
    }
    if r.forkattr & ATTR_CMNEXT_EXT_FLAGS != 0 {
        e.ext_flags = c.u64()?; // u64, not u32
    }
    if r.forkattr & ATTR_CMNEXT_CLONE_REFCNT != 0 {
        e.clone_refcnt = c.u32()?; // u32, not u64
    }

    Some((e, entry_len))
}

/// Streams entries out of a directory fd, refilling the kernel buffer as needed.
pub struct BulkDir {
    fd: RawFd,
    buf: Vec<u8>,
    filled: usize,
    cursor: usize,
    remaining: i32,
    done: bool,
}

impl BulkDir {
    pub fn new(fd: RawFd, buf_size: usize) -> Self {
        BulkDir {
            fd,
            buf: vec![0u8; buf_size],
            filled: 0,
            cursor: 0,
            remaining: 0,
            done: false,
        }
    }

    /// Next entry, or `Ok(None)` at end of directory.
    pub fn next_entry(&mut self) -> io::Result<Option<Entry>> {
        loop {
            if self.remaining > 0 {
                let rest = &self.buf[self.cursor..self.filled];
                let (e, len) = parse_entry(rest).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "malformed attrlist entry")
                })?;
                self.cursor += len;
                self.remaining -= 1;
                return Ok(Some(e));
            }
            if self.done {
                return Ok(None);
            }
            let mut al = attrlist {
                bitmapcount: ATTR_BIT_MAP_COUNT,
                reserved: 0,
                commonattr: REQ_COMMON,
                volattr: 0,
                dirattr: REQ_DIR,
                fileattr: REQ_FILE,
                forkattr: REQ_FORK,
            };
            let n = unsafe {
                getattrlistbulk(
                    self.fd,
                    &mut al as *mut _ as *mut libc::c_void,
                    self.buf.as_mut_ptr() as *mut libc::c_void,
                    self.buf.len(),
                    OPTS,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                self.done = true;
                return Ok(None);
            }
            self.remaining = n;
            self.cursor = 0;
            self.filled = self.buf.len();
        }
    }
}
