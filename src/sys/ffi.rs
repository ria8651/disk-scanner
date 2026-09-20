//! Raw constants and `extern` declarations.
//!
//! Every value here was read out of the macOS 26 SDK headers rather than
//! recalled; the header and line are noted so they can be re-checked.
#![allow(non_camel_case_types, dead_code)]

use libc::{c_char, c_int, c_uint, c_void};

// ---- sys/attr.h : attribute group bits -------------------------------------

pub const ATTR_BIT_MAP_COUNT: u16 = 5;

pub const ATTR_CMN_NAME: u32 = 0x0000_0001;
pub const ATTR_CMN_DEVID: u32 = 0x0000_0002;
pub const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
pub const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
pub const ATTR_CMN_FLAGS: u32 = 0x0004_0000;
pub const ATTR_CMN_FILEID: u32 = 0x0200_0000;
pub const ATTR_CMN_ERROR: u32 = 0x2000_0000;
pub const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;

pub const ATTR_DIR_ENTRYCOUNT: u32 = 0x0000_0002;

pub const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
pub const ATTR_FILE_TOTALSIZE: u32 = 0x0000_0002;
pub const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
pub const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;

// CMNEXT attributes ride in `forkattr` and need FSOPT_ATTR_CMN_EXTENDED.
pub const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
pub const ATTR_CMNEXT_LINKID: u32 = 0x0000_0010;
pub const ATTR_CMNEXT_REALFSID: u32 = 0x0000_0080;
pub const ATTR_CMNEXT_CLONEID: u32 = 0x0000_0100;
pub const ATTR_CMNEXT_EXT_FLAGS: u32 = 0x0000_0200;
pub const ATTR_CMNEXT_CLONE_REFCNT: u32 = 0x0000_1000;

// ---- sys/attr.h : getattrlist option flags ---------------------------------

pub const FSOPT_NOFOLLOW: u32 = 0x0000_0001;
pub const FSOPT_PACK_INVAL_ATTRS: u32 = 0x0000_0008;
pub const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020;

// ---- sys/vnode.h : enum vtype ----------------------------------------------

pub const VREG: u32 = 1;
pub const VDIR: u32 = 2;
pub const VLNK: u32 = 5;

// ---- sys/stat.h : st_flags -------------------------------------------------

pub const UF_COMPRESSED: u32 = 0x0000_0020;
pub const UF_DATAVAULT: u32 = 0x0000_0080;
pub const SF_FIRMLINK: u32 = 0x0080_0000;
pub const SF_DATALESS: u32 = 0x4000_0000;

// ---- sys/stat.h : EF_* returned by ATTR_CMNEXT_EXT_FLAGS -------------------
// NOTE: these live in sys/stat.h, *not* sys/attr.h.

pub const EF_MAY_SHARE_BLOCKS: u64 = 0x0000_0001;
pub const EF_NO_XATTRS: u64 = 0x0000_0002;
pub const EF_IS_PURGEABLE: u64 = 0x0000_0008;
pub const EF_IS_SPARSE: u64 = 0x0000_0010;
pub const EF_IS_SYNTHETIC: u64 = 0x0000_0020;
pub const EF_SHARES_ALL_BLOCKS: u64 = 0x0000_0040;

// ---- sys/mount.h : f_flags -------------------------------------------------

pub const MNT_RDONLY: u32 = 0x0000_0001;
pub const MNT_REMOVABLE: u32 = 0x0000_0200;
pub const MNT_LOCAL: u32 = 0x0000_1000;
pub const MNT_ROOTFS: u32 = 0x0000_4000;
pub const MNT_DONTBROWSE: u32 = 0x0010_0000;
pub const MNT_AUTOMOUNTED: u32 = 0x0040_0000;
pub const MNT_SNAPSHOT: u32 = 0x4000_0000;
pub const MNT_NOWAIT: c_int = 2;

// ---- sys/resource.h : I/O policies -----------------------------------------

pub const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
pub const IOPOL_TYPE_VFS_TRIGGER_RESOLVE: c_int = 5;
pub const IOPOL_SCOPE_PROCESS: c_int = 0;
pub const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;
pub const IOPOL_VFS_TRIGGER_RESOLVE_OFF: c_int = 1;

// ---- structs ---------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct attrlist {
    pub bitmapcount: u16,
    pub reserved: u16,
    pub commonattr: u32,
    pub volattr: u32,
    pub dirattr: u32,
    pub fileattr: u32,
    pub forkattr: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct attribute_set {
    pub commonattr: u32,
    pub volattr: u32,
    pub dirattr: u32,
    pub fileattr: u32,
    pub forkattr: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct attrreference {
    pub attr_dataoffset: i32,
    pub attr_length: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct fsid_t {
    pub val: [i32; 2],
}

extern "C" {
    pub fn getattrlistbulk(
        dirfd: c_int,
        alist: *mut c_void,
        attr_buf: *mut c_void,
        attr_buf_size: usize,
        options: u64,
    ) -> c_int;

    pub fn getattrlist(
        path: *const c_char,
        alist: *mut c_void,
        attr_buf: *mut c_void,
        attr_buf_size: usize,
        options: c_uint,
    ) -> c_int;

    pub fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
}
