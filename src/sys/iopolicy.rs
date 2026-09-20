//! Process I/O policies that make a scan safe.
//!
//! Both of these must be set **before worker threads are spawned**:
//! `IOPOL_TYPE_VFS_TRIGGER_RESOLVE` is process-scope only and returns EINVAL
//! at thread scope (verified on macOS 26), so a per-worker call silently
//! fails to protect anything.

use super::ffi::*;

#[derive(Debug, Default, Clone, Copy)]
pub struct Policies {
    pub dataless_suppressed: bool,
    pub triggers_suppressed: bool,
}

/// Stop the kernel downloading iCloud files and stop autofs triggers from
/// firing. With materialisation off, a stray `read()` of a dataless file
/// fails with `EDEADLK` instead of quietly pulling gigabytes over the network.
pub fn harden_process() -> Policies {
    unsafe {
        let dataless = setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_PROCESS,
            IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
        ) == 0;
        let triggers = setiopolicy_np(
            IOPOL_TYPE_VFS_TRIGGER_RESOLVE,
            IOPOL_SCOPE_PROCESS,
            IOPOL_VFS_TRIGGER_RESOLVE_OFF,
        ) == 0;
        Policies {
            dataless_suppressed: dataless,
            triggers_suppressed: triggers,
        }
    }
}
