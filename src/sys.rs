//! Thin wrappers over the mount syscalls.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(crate) fn cstr(p: impl AsRef<OsStr>) -> CString {
    CString::new(p.as_ref().as_bytes()).unwrap_or_else(|_| die!("path contains a NUL byte"))
}

pub(crate) fn check(r: libc::c_int, what: impl std::fmt::Display) -> Result<(), String> {
    if r == 0 {
        Ok(())
    } else {
        Err(format!("{what}: {}", io::Error::last_os_error()))
    }
}

/// A path for overlay's option string, where `,` separates options and `:` separates lower layers.
pub(crate) fn escape_opt(p: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in p.as_os_str().as_bytes() {
        if matches!(b, b'\\' | b',' | b':') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

pub(crate) fn mount(
    src: Option<&OsStr>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&OsStr>,
) -> Result<(), String> {
    let src = src.map(cstr);
    let fstype = fstype.map(cstr);
    let data = data.map(cstr);
    let target_c = cstr(target);
    let ptr = |c: &Option<CString>| c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    check(
        unsafe {
            libc::mount(
                ptr(&src),
                target_c.as_ptr(),
                ptr(&fstype),
                flags,
                ptr(&data) as *const libc::c_void,
            )
        },
        format!("mount {}", target.display()),
    )
}

/// A detached copy of the mount at `p` (like `mount --bind`), to attach later with move_mount.
pub(crate) fn open_tree(p: &Path) -> Result<File, String> {
    const SYS_OPEN_TREE: libc::c_long = 428;
    const OPEN_TREE_CLONE: libc::c_uint = 1;
    let c = cstr(p);
    let fd = unsafe {
        libc::syscall(
            SYS_OPEN_TREE,
            libc::AT_FDCWD,
            c.as_ptr(),
            OPEN_TREE_CLONE | libc::O_CLOEXEC as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(format!(
            "open_tree {}: {}",
            p.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fd as libc::c_int) })
}

pub(crate) fn move_mount(fd: &File, target: &Path) -> Result<(), String> {
    const SYS_MOVE_MOUNT: libc::c_long = 429;
    const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 4;
    let c = cstr(target);
    let r = unsafe {
        libc::syscall(
            SYS_MOVE_MOUNT,
            fd.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_FDCWD,
            c.as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH,
        )
    };
    check(r as libc::c_int, format!("move_mount {}", target.display()))
}

#[repr(C)]
pub(crate) struct MountAttr {
    pub(crate) attr_set: u64,
    pub(crate) attr_clr: u64,
    pub(crate) propagation: u64,
    pub(crate) userns_fd: u64,
}

/// mount_setattr(2) only touches the read-only flag, unlike a remount that must restate locked flags.
pub(crate) fn set_readonly(p: &Path, ro: bool, recursive: bool) -> Result<(), String> {
    const SYS_MOUNT_SETATTR: libc::c_long = 442;
    const MOUNT_ATTR_RDONLY: u64 = 1;
    const AT_RECURSIVE: libc::c_uint = 0x8000;
    let attr = MountAttr {
        attr_set: if ro { MOUNT_ATTR_RDONLY } else { 0 },
        attr_clr: if ro { 0 } else { MOUNT_ATTR_RDONLY },
        propagation: 0,
        userns_fd: 0,
    };
    let c = cstr(p);
    let flags = if recursive { AT_RECURSIVE } else { 0 };
    let r = unsafe {
        libc::syscall(
            SYS_MOUNT_SETATTR,
            libc::AT_FDCWD,
            c.as_ptr(),
            flags,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    check(
        r as libc::c_int,
        format!(
            "set {} {}",
            p.display(),
            if ro { "read-only" } else { "writable" }
        ),
    )
}
