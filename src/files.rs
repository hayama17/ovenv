//! Reading files, xattrs and directory trees, in both the host and the staged layer.

use sha2::{Digest, Sha256};
use std::ffi::{CString, OsString};
use std::fs;
use std::fs::{File, Metadata};
use std::io;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::env::Env;
use crate::sys::cstr;

pub(crate) fn lstat(p: &Path) -> Option<Metadata> {
    fs::symlink_metadata(p).ok()
}

pub(crate) fn is_dir(p: &Path) -> bool {
    lstat(p).is_some_and(|m| m.is_dir())
}

pub(crate) fn is_whiteout(m: &Metadata) -> bool {
    m.file_type().is_char_device() && m.rdev() == 0
}

pub(crate) fn attrs(m: &Metadata) -> String {
    format!("{:o}:{}:{}", m.mode() & 0o7777, m.uid(), m.gid())
}

pub(crate) fn getxattr(p: &Path, name: &[u8]) -> Option<Vec<u8>> {
    let c = cstr(p);
    let n = CString::new(name).ok()?;
    let size = unsafe { libc::lgetxattr(c.as_ptr(), n.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let size = unsafe {
        libc::lgetxattr(
            c.as_ptr(),
            n.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if size < 0 {
        return None;
    }
    buf.truncate(size as usize);
    Some(buf)
}

/// xattrs that belong to the file, not to overlayfs or the host's security labels.
pub(crate) fn xattrs(p: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let c = cstr(p);
    let size = unsafe { libc::llistxattr(c.as_ptr(), std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size as usize];
    let size =
        unsafe { libc::llistxattr(c.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if size <= 0 {
        return Vec::new();
    }
    buf.truncate(size as usize);
    let mut out: Vec<_> = buf
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty())
        .filter(|n| {
            !n.starts_with(b"trusted.overlay.")
                && !n.starts_with(b"user.overlay.")
                && *n != b"security.selinux"
        })
        .map(|n| (n.to_vec(), getxattr(p, n).unwrap_or_default()))
        .collect();
    out.sort();
    out
}

pub(crate) fn is_opaque(env: &Env, p: &Path) -> bool {
    getxattr(p, env.opaque_xattr().as_bytes()).as_deref() == Some(b"y")
}

pub(crate) fn file_hash(p: &Path) -> Option<String> {
    let mut f = File::open(p).ok()?;
    let mut h = Sha256::new();
    io::copy(&mut f, &mut h).ok()?;
    Some(hex(&h.finalize()))
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(mut fa), Ok(mut fb)) = (File::open(a), File::open(b)) else {
        return false;
    };
    let (mut ba, mut bb) = (vec![0u8; 65536], vec![0u8; 65536]);
    loop {
        let na = read_full(&mut fa, &mut ba);
        let nb = read_full(&mut fb, &mut bb);
        match (na, nb) {
            (Some(na), Some(nb)) if na == nb && ba[..na] == bb[..nb] => {
                if na == 0 {
                    return true;
                }
            }
            _ => return false,
        }
    }
}

pub(crate) fn read_full(f: &mut File, buf: &mut [u8]) -> Option<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(n)
}

/// Relative paths below `root`, parents before children, siblings sorted.
/// Directories that can't be read go to `unreadable`, so callers decide whether a partial list is acceptable.
pub(crate) fn walk(root: &Path, unreadable: &mut Vec<PathBuf>) -> Vec<PathBuf> {
    fn go(root: &Path, rel: &Path, out: &mut Vec<PathBuf>, bad: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(root.join(rel)) else {
            bad.push(rel.to_path_buf());
            return;
        };
        let mut names: Vec<OsString> = rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        names.sort();
        // A dir that lists but can't be entered (e.g. mode 444) is just as unreadable.
        if names
            .iter()
            .any(|n| lstat(&root.join(rel).join(n)).is_none())
        {
            bad.push(rel.to_path_buf());
            return;
        }
        for n in names {
            let r = rel.join(&n);
            out.push(r.clone());
            if is_dir(&root.join(&r)) {
                go(root, &r, out, bad);
            }
        }
    }
    let mut out = Vec::new();
    go(root, Path::new(""), &mut out, unreadable);
    out
}

/// walk() over a staged tree for diff and apply: a partial list would silently drop changes.
pub(crate) fn walk_staged(u: &Path, p: &Path) -> Vec<PathBuf> {
    let mut bad = Vec::new();
    let list = walk(u, &mut bad);
    if let Some(rel) = bad.first() {
        die!(
            "cannot read staged directory {}; make it readable in `ovenv run`, or `ovenv discard`",
            p.join(rel).display()
        );
    }
    list
}

/// Make every directory below `p` removable; a session may have left staged ones at mode 000.
pub(crate) fn unlock_tree(p: &Path) {
    if !is_dir(p) {
        return;
    }
    let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            unlock_tree(&e.path());
        }
    }
}

pub(crate) fn kind_name(m: &Metadata) -> &'static str {
    let t = m.file_type();
    if t.is_symlink() {
        "symbolic link"
    } else if t.is_dir() {
        "directory"
    } else if t.is_file() {
        "regular file"
    } else if t.is_fifo() {
        "fifo"
    } else if t.is_socket() {
        "socket"
    } else if t.is_char_device() {
        "character special file"
    } else {
        "block special file"
    }
}

/// Give `to` the xattrs of `from`. changes() only lets entries through whose xattrs match the host,
/// so this keeps the host's xattrs on a new inode, and restores file capabilities that chown clears.
pub(crate) fn copy_xattrs(from: &Path, to: &Path) -> io::Result<()> {
    set_xattrs(to, &xattrs(from))
}

pub(crate) fn set_xattrs(to: &Path, list: &[(Vec<u8>, Vec<u8>)]) -> io::Result<()> {
    let c = cstr(to);
    for (name, value) in list {
        let n = CString::new(name.as_slice()).map_err(io::Error::other)?;
        let r = unsafe {
            libc::lsetxattr(
                c.as_ptr(),
                n.as_ptr(),
                value.as_ptr() as *const libc::c_void,
                value.len(),
                0,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
