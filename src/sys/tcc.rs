//! Full Disk Access detection.
//!
//! There is no API to *request* FDA; it can only be detected and the user
//! pointed at System Settings. The probe below is gated by FDA and nothing
//! else, exists on every Mac, and never raises a prompt.
//!
//! Branch on `EPERM` specifically: `EACCES` means ownership/mode (which FDA
//! would not fix, so telling the user to grant it sends them on a wild goose
//! chase), and `ENOENT` means the probe itself is wrong.

use std::path::Path;

const PROBE: &str = "/Library/Application Support/com.apple.TCC/TCC.db";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Full,
    Denied,
    Unknown,
}

pub fn full_disk_access() -> Access {
    match std::fs::File::open(PROBE) {
        Ok(_) => Access::Full,
        Err(e) => match e.raw_os_error() {
            Some(libc::EPERM) => Access::Denied,
            _ => Access::Unknown,
        },
    }
}

/// The System Settings pane for macOS 13+. There is no programmatic grant.
pub const SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles";

/// Best-effort: which application the user actually has to add to the FDA list.
///
/// TCC evaluates the *responsible* process, not the one making the syscall, so
/// for a CLI the grant belongs to the hosting terminal. The only public way to
/// approximate this is to walk up the process tree to the nearest `.app`
/// bundle (`responsibility_get_pid_responsible_for_pid` is private SPI).
pub fn responsible_app_hint() -> Option<String> {
    let mut pid = unsafe { libc::getpid() };
    for _ in 0..24 {
        let mut buf = [0u8; 4096];
        let n =
            unsafe { proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
        if n > 0 {
            let path = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
            if let Some(idx) = path.find(".app/") {
                let app = &path[..idx + 4];
                return Some(
                    Path::new(app)
                        .file_stem()
                        .map(|s| format!("{} ({})", s.to_string_lossy(), app))
                        .unwrap_or_else(|| app.to_string()),
                );
            }
        }
        match parent_pid(pid) {
            Some(p) if p > 1 && p != pid => pid = p,
            _ => break,
        }
    }
    std::env::var("TERM_PROGRAM").ok()
}

fn parent_pid(pid: libc::pid_t) -> Option<libc::pid_t> {
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let sz = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            sz,
        );
        if n == sz {
            Some(info.pbi_ppid as libc::pid_t)
        } else {
            None
        }
    }
}

const PROC_PIDTBSDINFO: libc::c_int = 3;

extern "C" {
    fn proc_pidpath(pid: libc::pid_t, buf: *mut libc::c_void, size: u32) -> libc::c_int;
    fn proc_pidinfo(
        pid: libc::pid_t,
        flavor: libc::c_int,
        arg: u64,
        buf: *mut libc::c_void,
        size: libc::c_int,
    ) -> libc::c_int;
}
