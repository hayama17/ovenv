//! ovenv: stage a command's filesystem side effects in OverlayFS, then review, apply or discard them.

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{
    fchown, lchown, symlink, DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt,
    PermissionsExt,
};
use std::path::{Component, Path, PathBuf};
use std::process::exit;

use sha2::{Digest, Sha256};

const ROOT_DEFAULT: &[&str] = &["/usr", "/opt", "/etc", "/var", "/root"];
const USER_DEFAULT: &[&str] = &["~/.local", "~/.cargo", "~/.npm", "~/.cache", "~/.config"];

macro_rules! die {
    ($($a:tt)*) => {{ eprintln!("ovenv: {}", format!($($a)*)); exit(1) }};
}

fn or_die<T, E: std::fmt::Display>(r: Result<T, E>, what: impl std::fmt::Display) -> T {
    r.unwrap_or_else(|e| die!("{what}: {e}"))
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Root,
    User,
}

struct Env {
    /// .ovenv/: paths, mode, lock and the pointer to the state directory
    dir: PathBuf,
    project: PathBuf,
    mode: Mode,
    /// /tmp/ovenv-*/: upper, work and base
    state: Option<PathBuf>,
}

impl Env {
    /// `allow_gone` lets discard clean up after the state directory vanished (e.g. a reboot).
    fn load(allow_gone: bool) -> Env {
        let dir = match std::env::var_os("OVENV_DIR") {
            Some(d) => resolve(Path::new(&d)),
            None => find_dir(),
        };
        let project = dir.parent().unwrap_or(Path::new("/")).to_path_buf();
        let root = unsafe { libc::geteuid() } == 0;
        // root and user mode store overlay metadata in different xattr namespaces, so an env sticks to one.
        let mode = match fs::read_to_string(dir.join("mode"))
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("root") => Mode::Root,
            Some("user") => Mode::User,
            _ if root => Mode::Root,
            _ => Mode::User,
        };
        if mode == Mode::Root && !root {
            die!("{} was created as root; use sudo", dir.display());
        }
        if mode == Mode::User && root {
            die!(
                "{} was created without root; run without sudo",
                dir.display()
            );
        }
        let state = fs::read_to_string(dir.join("state"))
            .ok()
            .map(|s| PathBuf::from(s.trim_end_matches('\n')));
        if let Some(st) = &state {
            if lstat(st).is_some() {
                check_state(st);
            } else if !allow_gone {
                die!(
                    "staged changes in {} are gone (e.g. after a reboot); run `ovenv discard` to start over",
                    st.display()
                );
            }
        }
        Env {
            dir,
            project,
            mode,
            state,
        }
    }

    fn state(&self) -> &Path {
        self.state.as_deref().expect("state directory not set")
    }

    fn upper(&self, p: &Path) -> PathBuf {
        under(&self.state().join("upper"), p)
    }

    fn opaque_xattr(&self) -> &'static str {
        match self.mode {
            Mode::Root => "trusted.overlay.opaque",
            Mode::User => "user.overlay.opaque",
        }
    }

    /// (writable straight through, path) for every line of .ovenv/paths or the defaults.
    fn paths(&self) -> Vec<(bool, PathBuf)> {
        let text = match fs::read_to_string(self.dir.join("paths")) {
            Ok(t) => t,
            Err(_) => match self.mode {
                Mode::Root => ROOT_DEFAULT.join("\n"),
                Mode::User => USER_DEFAULT.join("\n"),
            },
        };
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/".into());
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (rw, p) = match line
                .strip_prefix("rw")
                .filter(|r| r.starts_with([' ', '\t']))
            {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            let p = if p == "~" {
                home.clone()
            } else if let Some(rest) = p.strip_prefix("~/") {
                home.join(rest)
            } else {
                PathBuf::from(p)
            };
            out.push((rw, resolve(&p)));
        }
        // A path under another staged path is already staged by it; mounting both would nest one upper in the other.
        let staged: Vec<PathBuf> = out
            .iter()
            .filter(|(rw, _)| !rw)
            .map(|(_, p)| p.clone())
            .collect();
        let mut seen = HashSet::new();
        out.retain(|(rw, p)| {
            *rw || (!staged.iter().any(|q| q != p && p.starts_with(q)) && seen.insert(p.clone()))
        });
        out
    }

    fn overlays(&self) -> Vec<PathBuf> {
        self.paths()
            .into_iter()
            .filter(|(rw, _)| !rw)
            .map(|(_, p)| p)
            .collect()
    }
}

/// The state directory holds staged file contents, so refuse anything we didn't create for ourselves.
fn check_state(st: &Path) {
    let m = lstat(st);
    let ok = st.parent() == Some(Path::new("/tmp"))
        && st
            .file_name()
            .is_some_and(|n| n.as_bytes().starts_with(b"ovenv-"))
        && m.as_ref().is_some_and(|m| {
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0
        });
    if !ok {
        die!(
            "refusing to use {}: expected a /tmp/ovenv-* directory owned by you with mode 700",
            st.display()
        );
    }
}

fn create_state(env: &mut Env) {
    let mut tmpl = b"/tmp/ovenv-XXXXXXXX\0".to_vec();
    // mkdtemp picks an unguessable name and creates it with mode 700
    if unsafe { libc::mkdtemp(tmpl.as_mut_ptr() as *mut libc::c_char) }.is_null() {
        die!(
            "cannot create a state directory in /tmp: {}",
            io::Error::last_os_error()
        );
    }
    tmpl.pop();
    let st = PathBuf::from(OsStr::from_bytes(&tmpl));
    or_die(
        fs::write(env.dir.join("state"), format!("{}\n", st.display())),
        "cannot write .ovenv/state",
    );
    env.state = Some(st);
}

fn find_dir() -> PathBuf {
    let cwd = or_die(std::env::current_dir(), "cannot read the current directory");
    for d in cwd.ancestors() {
        if d != Path::new("/") && d.join(".ovenv").is_dir() {
            return d.join(".ovenv");
        }
    }
    cwd.join(".ovenv")
}

/// Like `realpath -m`: resolve symlinks in the part that exists, keep the rest as written.
fn resolve(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut clean = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            c => clean.push(c),
        }
    }
    let mut rest = Vec::new();
    let mut head = clean.as_path();
    loop {
        if let Ok(real) = head.canonicalize() {
            return rest.iter().rev().fold(real, |acc, c| acc.join(c));
        }
        match (head.parent(), head.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                head = parent;
            }
            _ => return clean,
        }
    }
}

/// `base` + absolute path `p`.
fn under(base: &Path, p: &Path) -> PathBuf {
    base.join(p.strip_prefix("/").unwrap_or(p))
}

fn cstr(p: impl AsRef<OsStr>) -> CString {
    CString::new(p.as_ref().as_bytes()).unwrap_or_else(|_| die!("path contains a NUL byte"))
}

fn check(r: libc::c_int, what: impl std::fmt::Display) -> Result<(), String> {
    if r == 0 {
        Ok(())
    } else {
        Err(format!("{what}: {}", io::Error::last_os_error()))
    }
}

// ---------------------------------------------------------------- locking

/// One run, shell, apply or discard per .ovenv; held until that command has fully finished.
fn lock(env: &Env) -> File {
    or_die(
        fs::create_dir_all(&env.dir),
        format!("cannot create {}", env.dir.display()),
    );
    let path = env.dir.join("lock");
    or_die(
        OpenOptions::new().append(true).create(true).open(&path),
        "cannot create the lock file",
    );
    // read-only: an fd open for writing makes the read-only remount fail with EBUSY
    let f = or_die(File::open(&path), "cannot open the lock file");
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        die!(
            "{} is in use by another ovenv run, shell, apply or discard",
            env.dir.display()
        );
    }
    f
}

// ---------------------------------------------------------------- run

fn run(mut env: Env, cmd: Vec<OsString>) -> ! {
    let _lock = lock(&env);
    if env.state.is_none() {
        create_state(&mut env);
    }
    let env = &env;
    let mode_file = env.dir.join("mode");
    if !mode_file.exists() {
        let m = if env.mode == Mode::Root {
            "root\n"
        } else {
            "user\n"
        };
        or_die(fs::write(&mode_file, m), "cannot write the mode file");
    }
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    let mut ov = Vec::new();
    let mut rw = vec![PathBuf::from("/tmp")];
    for (is_rw, p) in env.paths() {
        if is_rw {
            rw.push(p);
            continue;
        }
        if env.state().starts_with(&p) {
            die!(
                "cannot stage {}: ovenv keeps staged changes in {}",
                p.display(),
                env.state().display()
            );
        }
        if env.mode == Mode::User {
            if let Ok(m) = fs::symlink_metadata(&p) {
                if m.uid() != uid {
                    eprintln!("ovenv: skipping {} (needs root)", p.display());
                    continue;
                }
            }
        }
        ov.push(p);
    }
    // A project under a staged path is staged too; otherwise it stays writable as before.
    if !ov.iter().any(|p| env.project.starts_with(p)) {
        rw.insert(0, env.project.clone());
    }
    // The merged root takes its mode, owner and xattrs from the upper dir, so start it as a copy of the host dir.
    for p in &ov {
        let upper = env.upper(p);
        if lstat(&upper).is_some() {
            continue;
        }
        or_die(
            fs::create_dir_all(p),
            format!("cannot create {}", p.display()),
        );
        if let Some(parent) = upper.parent() {
            or_die(
                fs::create_dir_all(parent),
                "cannot create the state directory",
            );
        }
        let hm = lstat(p).unwrap_or_else(|| die!("{} vanished", p.display()));
        or_die(
            fs::DirBuilder::new().mode(0o700).create(&upper),
            "cannot create the upper directory",
        );
        or_die(
            own(env, &hm, &upper),
            "cannot set the upper directory's owner",
        );
        or_die(
            fs::set_permissions(&upper, fs::Permissions::from_mode(hm.mode() & 0o7777)),
            "cannot set the upper directory's mode",
        );
        // best effort: an xattr we can't copy only means the root shows up as SKIP
        let _ = copy_xattrs(p, &upper);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        die!("fork: {}", io::Error::last_os_error());
    }
    if pid == 0 {
        let err = enter(env, &ov, &rw, &cwd, uid, gid, &cmd);
        eprintln!("ovenv: {err}");
        unsafe { libc::_exit(127) }
    }
    // Keep waiting through Ctrl-C so the baseline still gets recorded; the child gets the signal itself.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    let mut status = 0;
    while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            die!("waitpid: {}", io::Error::last_os_error());
        }
    }
    record_base(env);
    exit(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    })
}

fn write_proc(file: &str, data: &str) -> Result<(), String> {
    fs::write(file, data).map_err(|e| format!("{file}: {e}"))
}

/// Runs in the forked child: build the staged view, then exec. Returns only on error.
fn enter(
    env: &Env,
    ov: &[PathBuf],
    rw: &[PathBuf],
    cwd: &Path,
    uid: u32,
    gid: u32,
    cmd: &[OsString],
) -> String {
    let r = (|| -> Result<(), String> {
        if env.mode == Mode::User {
            check(
                unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) },
                "unshare",
            )?;
            write_proc("/proc/self/setgroups", "deny")?;
            write_proc("/proc/self/uid_map", &format!("0 {uid} 1"))?;
            write_proc("/proc/self/gid_map", &format!("0 {gid} 1"))?;
        } else {
            check(unsafe { libc::unshare(libc::CLONE_NEWNS) }, "unshare")?;
        }
        mount(
            None,
            Path::new("/"),
            None,
            libc::MS_REC | libc::MS_PRIVATE,
            None,
        )?;

        // Grab the host's view of rw paths now, before an overlay above them would hide it.
        let mut clones = Vec::new();
        for p in rw {
            if fs::symlink_metadata(p).is_ok() {
                clones.push((open_tree(p)?, p));
            }
        }

        // Overlay needs a writable upper at mount time, so mount before making everything read-only.
        let xopt = if env.mode == Mode::User {
            ",userxattr"
        } else {
            ""
        };
        for p in ov {
            let upper = env.upper(p);
            let work = under(&env.state().join("work"), p);
            for d in [p, &upper, &work] {
                fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
            }
            // ponytail: commas and colons in paths are not escaped in the overlay options (#6)
            let opts = format!(
                "lowerdir={},upperdir={},workdir={}{xopt}",
                p.display(),
                upper.display(),
                work.display()
            );
            mount(Some("ovenv"), p, Some("overlay"), 0, Some(&opts))?;
        }
        // Anything not overlaid or passed through is read-only, so writes can't silently reach the host.
        set_readonly(Path::new("/"), true, true)?;
        set_readonly(Path::new("/proc"), false, true)?;
        set_readonly(Path::new("/dev"), false, true)?;
        for p in ov {
            set_readonly(p, false, false)?;
        }
        for (fd, p) in &clones {
            move_mount(fd, p)?;
            set_readonly(p, false, false)?;
        }
        // The command may read ovenv's own bookkeeping but must not change it.
        for p in [&env.dir, env.state()] {
            mount(Some(&p.to_string_lossy()), p, None, libc::MS_BIND, None)?;
            set_readonly(p, true, false)?;
        }

        // Re-resolve cwd, otherwise it still points below the new mounts.
        if std::env::set_current_dir(cwd).is_err() {
            let _ = std::env::set_current_dir("/");
        }
        std::env::set_var("OVENV", &env.dir);
        if env.mode == Mode::User {
            check(unsafe { libc::unshare(libc::CLONE_NEWUSER) }, "unshare")?;
            write_proc("/proc/self/setgroups", "deny")?;
            write_proc("/proc/self/uid_map", &format!("{uid} 0 1"))?;
            write_proc("/proc/self/gid_map", &format!("{gid} 0 1"))?;
        }
        Ok(())
    })();
    if let Err(e) = r {
        return e;
    }
    let args: Vec<CString> = cmd.iter().map(cstr).collect();
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    unsafe { libc::execvp(argv[0], argv.as_ptr()) };
    format!(
        "cannot run {}: {}",
        cmd[0].to_string_lossy(),
        io::Error::last_os_error()
    )
}

fn mount(
    src: Option<&str>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
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
fn open_tree(p: &Path) -> Result<File, String> {
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

fn move_mount(fd: &File, target: &Path) -> Result<(), String> {
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
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// mount_setattr(2) only touches the read-only flag, unlike a remount that must restate locked flags.
fn set_readonly(p: &Path, ro: bool, recursive: bool) -> Result<(), String> {
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

// ---------------------------------------------------------------- inspecting files

fn lstat(p: &Path) -> Option<Metadata> {
    fs::symlink_metadata(p).ok()
}

fn is_dir(p: &Path) -> bool {
    lstat(p).is_some_and(|m| m.is_dir())
}

fn is_whiteout(m: &Metadata) -> bool {
    m.file_type().is_char_device() && m.rdev() == 0
}

fn attrs(m: &Metadata) -> String {
    format!("{:o}:{}:{}", m.mode() & 0o7777, m.uid(), m.gid())
}

fn getxattr(p: &Path, name: &[u8]) -> Option<Vec<u8>> {
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
fn xattrs(p: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
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

fn is_opaque(env: &Env, p: &Path) -> bool {
    getxattr(p, env.opaque_xattr().as_bytes()).as_deref() == Some(b"y")
}

fn file_hash(p: &Path) -> Option<String> {
    let mut f = File::open(p).ok()?;
    let mut h = Sha256::new();
    io::copy(&mut f, &mut h).ok()?;
    Some(hex(&h.finalize()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn same_content(a: &Path, b: &Path) -> bool {
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

fn read_full(f: &mut File, buf: &mut [u8]) -> Option<usize> {
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
fn walk(root: &Path) -> Vec<PathBuf> {
    fn go(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(root.join(rel)) else {
            return;
        };
        let mut names: Vec<OsString> = rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        names.sort();
        for n in names {
            let r = rel.join(&n);
            out.push(r.clone());
            if is_dir(&root.join(&r)) {
                go(root, &r, out);
            }
        }
    }
    let mut out = Vec::new();
    go(root, Path::new(""), &mut out);
    out
}

fn kind_name(m: &Metadata) -> &'static str {
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

/// What the host has at `h`, precise enough to notice any change apply could overwrite.
fn host_state(env: &Env, h: &Path, up: &Path) -> String {
    let Some(m) = lstat(h) else { return "-".into() };
    let t = m.file_type();
    if t.is_symlink() {
        format!("l {}", fs::read_link(h).unwrap_or_default().display())
    } else if t.is_dir() {
        let mut s = format!("d {}", attrs(&m));
        // contents only matter when the staged entry deletes or replaces the whole directory
        if !is_dir(up) || is_opaque(env, up) {
            let mut hasher = Sha256::new();
            for rel in walk(h) {
                let Some(cm) = lstat(&h.join(&rel)) else {
                    continue;
                };
                let target = fs::read_link(h.join(&rel)).unwrap_or_default();
                hasher.update(rel.as_os_str().as_bytes());
                hasher.update(
                    format!(
                        "\0{} {} {} {}.{} {}\0",
                        kind_name(&cm),
                        attrs(&cm),
                        cm.size(),
                        cm.mtime(),
                        cm.mtime_nsec(),
                        target.display()
                    )
                    .as_bytes(),
                );
            }
            s += &format!(" {}", hex(&hasher.finalize()));
        }
        s
    } else if t.is_file() {
        format!(
            "f {} {}",
            attrs(&m),
            file_hash(h).unwrap_or_else(|| "unreadable".into())
        )
    } else {
        format!("o {}", kind_name(&m))
    }
}

// ---------------------------------------------------------------- baseline

fn read_base(env: &Env) -> HashMap<PathBuf, String> {
    let Some(st) = &env.state else {
        return HashMap::new();
    };
    let data = fs::read(st.join("base")).unwrap_or_default();
    let mut fields = data.split(|&b| b == 0);
    let mut out = HashMap::new();
    while let (Some(p), Some(s)) = (fields.next(), fields.next()) {
        if !p.is_empty() {
            out.insert(
                PathBuf::from(OsStr::from_bytes(p)),
                String::from_utf8_lossy(s).into_owned(),
            );
        }
    }
    out
}

/// Remember what the host looked like when each path was first staged; apply checks against it.
fn record_base(env: &Env) {
    let seen = read_base(env);
    let mut buf = Vec::new();
    for p in env.overlays() {
        let u = env.upper(&p);
        let entries = walk(&u).into_iter().map(|rel| (p.join(&rel), u.join(&rel)));
        // the overlay root itself counts too: its mode and owner can change
        for (h, up) in std::iter::once((p.clone(), u.clone())).chain(entries) {
            if seen.contains_key(&h) {
                continue;
            }
            buf.extend_from_slice(h.as_os_str().as_bytes());
            buf.push(0);
            buf.extend_from_slice(host_state(env, &h, &up).as_bytes());
            buf.push(0);
        }
    }
    let f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(env.state().join("base"));
    or_die(
        f.and_then(|mut f| f.write_all(&buf)),
        "cannot record the host state",
    );
}

// ---------------------------------------------------------------- changes

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Add,
    Modify,
    Attr,
    Delete,
    Replace,
    Skip,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Add => "ADD",
            Kind::Modify => "MODIFY",
            Kind::Attr => "ATTR",
            Kind::Delete => "DELETE",
            Kind::Replace => "REPLACE",
            Kind::Skip => "SKIP",
        }
    }
}

struct Change {
    kind: Kind,
    host: PathBuf,
    up: PathBuf,
    note: &'static str,
}

/// Every real change, in apply order. diff, the conflict check and apply all use this.
fn changes(env: &Env) -> Vec<Change> {
    let mut out = Vec::new();
    if env.state.is_none() {
        return out;
    }
    for p in env.overlays() {
        let u = env.upper(&p);
        // The overlay root is the upper dir itself and is never copied up, so only its attributes can change.
        if let Some(um) = lstat(&u) {
            let found = match lstat(&p) {
                // gone from the host: listing it keeps it in the conflict check
                None => Some((Kind::Add, "")),
                Some(_) if xattrs(&u) != xattrs(&p) => Some((Kind::Skip, "xattrs")),
                Some(hm) if attrs(&um) != attrs(&hm) => Some((Kind::Attr, "")),
                _ => None,
            };
            if let Some((kind, note)) = found {
                out.push(Change {
                    kind,
                    host: p.clone(),
                    up: u.clone(),
                    note,
                });
            }
        }
        // directories whose host contents go away / whose staged contents are skipped
        let mut gone: Option<PathBuf> = None;
        let mut skip: Option<PathBuf> = None;
        for rel in walk(&u) {
            if gone.as_ref().is_some_and(|g| !rel.starts_with(g)) {
                gone = None;
            }
            if skip.as_ref().is_some_and(|s| !rel.starts_with(s)) {
                skip = None;
            }
            if skip.is_some() {
                continue;
            }
            let up = u.join(&rel);
            let host = p.join(&rel);
            let Some(um) = lstat(&up) else { continue };
            let hm = if gone.is_some() { None } else { lstat(&host) };
            let mut add = |kind, note| {
                out.push(Change {
                    kind,
                    host: host.clone(),
                    up: up.clone(),
                    note,
                })
            };

            if is_whiteout(&um) {
                if hm.is_some() {
                    add(Kind::Delete, "");
                }
                continue;
            }
            let ut = um.file_type();
            if !ut.is_dir() && !ut.is_file() && !ut.is_symlink() {
                add(Kind::Skip, "special file");
                continue;
            }
            let hx = if hm.is_some() {
                xattrs(&host)
            } else {
                Vec::new()
            };
            if xattrs(&up) != hx {
                add(Kind::Skip, "xattrs");
                if ut.is_dir() {
                    skip = Some(rel.clone());
                }
                continue;
            }
            let Some(hm) = hm else {
                add(Kind::Add, "");
                continue;
            };
            let ht = hm.file_type();
            if ut.is_dir() {
                if !ht.is_dir() || is_opaque(env, &up) {
                    add(Kind::Replace, "");
                    gone = Some(rel.clone());
                } else if attrs(&um) != attrs(&hm) {
                    add(Kind::Attr, "");
                }
            } else if ht.is_dir() {
                add(Kind::Replace, "");
            } else if ut.is_symlink() && ht.is_symlink() {
                if fs::read_link(&up).ok() != fs::read_link(&host).ok() {
                    add(Kind::Modify, "");
                } else if (um.uid(), um.gid()) != (hm.uid(), hm.gid()) {
                    add(Kind::Skip, "symlink owner");
                }
            } else if ut.is_symlink() || !ht.is_file() || !same_content(&up, &host) {
                add(Kind::Modify, "");
            // copy-up also happens on touch, so a file with the same content and attrs is not a change
            } else if attrs(&um) != attrs(&hm) {
                add(Kind::Attr, "");
            }
        }
    }
    out
}

// ---------------------------------------------------------------- diff

fn human(bytes: u64) -> String {
    let mut v = bytes as f64;
    for unit in ["B", "K", "M", "G", "T"] {
        if v < 1024.0 || unit == "T" {
            return if v < 10.0 && unit != "B" {
                format!("{v:.1}{unit}")
            } else {
                format!("{v:.0}{unit}")
            };
        }
        v /= 1024.0;
    }
    unreachable!()
}

fn describe(p: &Path) -> String {
    match lstat(p) {
        Some(m) if m.file_type().is_symlink() => format!(
            "symlink -> {}",
            fs::read_link(p).unwrap_or_default().display()
        ),
        Some(m) => kind_name(&m).into(),
        None => "missing".into(),
    }
}

fn show_diff(env: &Env, content: bool) {
    let list = changes(env);
    if list.is_empty() {
        return;
    }
    let mut out = io::stdout().lock();
    for c in &list {
        let slash = is_dir(&c.up) || (c.kind == Kind::Delete && is_dir(&c.host));
        let _ = write!(
            out,
            "{:<8}{}{}",
            c.kind.label(),
            c.host.display(),
            if slash { "/" } else { "" }
        );
        if !c.note.is_empty() {
            let _ = write!(out, "  ({}, not applied)", c.note);
        }
        let _ = writeln!(out);
    }
    let staged: u64 = walk(&env.state().join("upper"))
        .iter()
        .filter_map(|r| lstat(&env.state().join("upper").join(r)))
        .map(|m| m.blocks() * 512)
        .sum();
    let _ = writeln!(out, "\n{} staged", human(staged));
    if !content {
        return;
    }

    for c in list.iter().filter(|c| c.kind == Kind::Modify) {
        let h = c.host.display();
        let _ = writeln!(out);
        let link = lstat(&c.up).is_some_and(|m| m.file_type().is_symlink())
            || lstat(&c.host).is_some_and(|m| m.file_type().is_symlink());
        if link {
            let _ = writeln!(
                out,
                "--- host:{h}\n+++ staged:{h}\n-{}\n+{}",
                describe(&c.host),
                describe(&c.up)
            );
            continue;
        }
        let read = |p: &Path| fs::read(p).unwrap_or_else(|e| die!("cannot compare {h}: {e}"));
        let (a, b) = (read(&c.host), read(&c.up));
        let binary = |d: &[u8]| d[..d.len().min(8000)].contains(&0);
        if binary(&a) || binary(&b) {
            let _ = writeln!(out, "Binary files host:{h} and staged:{h} differ");
            continue;
        }
        let (a, b) = (String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
        let diff = similar::TextDiff::from_lines(a.as_ref(), b.as_ref());
        let _ = write!(
            out,
            "{}",
            diff.unified_diff()
                .header(&format!("host:{h}"), &format!("staged:{h}"))
        );
    }
}

// ---------------------------------------------------------------- apply

fn random_suffix() -> String {
    let mut b = [0u8; 6];
    let ok = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    or_die(ok, "cannot read /dev/urandom");
    hex(&b)
}

fn own(env: &Env, m: &Metadata, p: &Path) -> io::Result<()> {
    if env.mode == Mode::Root {
        lchown(p, Some(m.uid()), Some(m.gid()))?;
    }
    Ok(())
}

/// Put a copy of regular file or symlink `up` at `h` with rename(2), so `h` is never missing or half-written.
fn put(env: &Env, up: &Path, h: &Path) -> Result<(), String> {
    let dir = h.parent().unwrap_or(Path::new("/"));
    let um =
        lstat(up).ok_or_else(|| format!("{} vanished from the staged changes", h.display()))?;
    let tmp = loop {
        let tmp = dir.join(format!(".ovenv.{}", random_suffix()));
        let made = if um.file_type().is_symlink() {
            fs::read_link(up)
                .and_then(|t| symlink(t, &tmp))
                .and_then(|()| {
                    copy_xattrs(up, &tmp).inspect_err(|_| {
                        let _ = fs::remove_file(&tmp);
                    })
                })
        } else {
            (|| {
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp)?;
                let prepared = (|| {
                    io::copy(&mut File::open(up)?, &mut f)?;
                    if env.mode == Mode::Root {
                        fchown(&f, Some(um.uid()), Some(um.gid()))?;
                    }
                    // after chown, which clears setuid bits
                    f.set_permissions(fs::Permissions::from_mode(um.mode() & 0o7777))?;
                    copy_xattrs(up, &tmp)
                })();
                if prepared.is_err() {
                    let _ = fs::remove_file(&tmp);
                }
                prepared
            })()
        };
        match made {
            Ok(()) => break tmp,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(format!(
                    "cannot prepare {}: {e}; left it unchanged",
                    h.display()
                ))
            }
        }
    };
    fs::rename(&tmp, h).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot replace {}: {e}; left it unchanged", h.display())
    })
}

/// Give `to` the xattrs of `from`. changes() only lets entries through whose xattrs match the host,
/// so this keeps the host's xattrs on a new inode, and restores file capabilities that chown clears.
fn copy_xattrs(from: &Path, to: &Path) -> io::Result<()> {
    let c = cstr(to);
    for (name, value) in xattrs(from) {
        let n = CString::new(name).map_err(io::Error::other)?;
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

fn remove(p: &Path) -> io::Result<()> {
    match lstat(p) {
        Some(m) if m.is_dir() => fs::remove_dir_all(p),
        Some(_) => fs::remove_file(p),
        None => Ok(()),
    }
}

fn apply(env: &Env, force: bool) {
    let _lock = lock(env);
    if env.state.is_none() {
        println!("nothing staged");
        return;
    }
    let list = changes(env);
    let base = read_base(env);
    let mut conflicts = Vec::new();
    let mut made = HashSet::new();
    for c in list.iter().filter(|c| c.kind != Kind::Skip) {
        match base.get(&c.host) {
            None => conflicts.push(format!(
                "{} (no record of the host state)",
                c.host.display()
            )),
            Some(s) if *s != host_state(env, &c.host, &c.up) => {
                conflicts.push(c.host.display().to_string())
            }
            _ => {}
        }
        if matches!(c.kind, Kind::Add | Kind::Replace) && is_dir(&c.up) {
            made.insert(c.host.clone());
        }
        let parent = c.host.parent().unwrap_or(Path::new("/"));
        if c.kind == Kind::Add && !is_dir(parent) && !made.contains(parent) {
            conflicts.push(format!("{} (parent directory is gone)", parent.display()));
        }
    }
    if !conflicts.is_empty() && !force {
        eprintln!("ovenv: changed on the host since it was staged:");
        for c in &conflicts {
            eprintln!("  {c}");
        }
        die!("review them, then rerun with --force");
    }

    let mut dirmodes = Vec::new();
    let mut skipped = Vec::new();
    for c in &list {
        let h = &c.host;
        let fail = |e: io::Error| die!("{}: {e}", h.display());
        match c.kind {
            Kind::Skip => skipped.push(format!("{} ({})", h.display(), c.note)),
            Kind::Delete => remove(h).unwrap_or_else(fail),
            Kind::Attr => {
                let um = lstat(&c.up)
                    .unwrap_or_else(|| die!("{} vanished from the staged changes", h.display()));
                own(env, &um, h).unwrap_or_else(fail);
                if um.is_dir() {
                    dirmodes.push((h.clone(), um.mode()));
                } else {
                    fs::set_permissions(h, fs::Permissions::from_mode(um.mode() & 0o7777))
                        .unwrap_or_else(fail);
                }
                copy_xattrs(&c.up, h).unwrap_or_else(fail);
            }
            Kind::Add | Kind::Replace | Kind::Modify => {
                if c.kind == Kind::Replace {
                    remove(h).unwrap_or_else(fail);
                }
                if is_dir(&c.up) {
                    let um = lstat(&c.up).unwrap_or_else(|| {
                        die!("{} vanished from the staged changes", h.display())
                    });
                    fs::DirBuilder::new()
                        .mode(0o700)
                        .create(h)
                        .unwrap_or_else(fail);
                    own(env, &um, h).unwrap_or_else(fail);
                    copy_xattrs(&c.up, h).unwrap_or_else(fail);
                    dirmodes.push((h.clone(), um.mode()));
                } else {
                    put(env, &c.up, h).unwrap_or_else(|e| die!("{e}"));
                }
            }
        }
    }
    // Directory modes go last, deepest first, so a read-only directory doesn't block writes below it.
    for (h, mode) in dirmodes.iter().rev() {
        or_die(
            fs::set_permissions(h, fs::Permissions::from_mode(mode & 0o7777)),
            h.display(),
        );
    }
    for s in &skipped {
        println!("not applied: {s}");
    }
    println!("applied {} change(s)", list.len() - skipped.len());
    discard_files(env);
}

// ---------------------------------------------------------------- discard / init

/// Remove the staged state; .ovenv/paths stays.
fn discard_files(env: &Env) {
    if let Some(st) = env.state.as_ref().filter(|st| lstat(st).is_some()) {
        // overlay leaves a mode-000 dir in workdir, which blocks removal for non-root
        let work = st.join("work");
        for rel in std::iter::once(PathBuf::new()).chain(walk(&work)) {
            let p = work.join(rel);
            if is_dir(&p) {
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o700));
            }
        }
        or_die(remove(st), format!("cannot remove {}", st.display()));
    }
    for name in ["state", "mode"] {
        let p = env.dir.join(name);
        or_die(remove(&p), format!("cannot remove {}", p.display()));
    }
}

fn init() {
    let dir = or_die(std::env::current_dir(), "cannot read the current directory").join(".ovenv");
    if dir.exists() {
        die!(".ovenv already exists");
    }
    or_die(fs::create_dir_all(&dir), "cannot create .ovenv");
    let defaults = if unsafe { libc::geteuid() } == 0 {
        ROOT_DEFAULT
    } else {
        USER_DEFAULT
    };
    let text = format!(
        "# One path per line. Writes under these paths are staged in OverlayFS.\n\
         # Prefix with 'rw ' to let writes go straight to the host.\n\
         # Use ~ to stage your whole home directory, dotfiles included.\n\
         # Everything else is read-only inside ovenv, except /tmp, /dev, /proc\n\
         # and this project (unless it is under a staged path).\n{}\n",
        defaults.join("\n")
    );
    or_die(
        fs::write(dir.join("paths"), text),
        "cannot write .ovenv/paths",
    );
    println!("created {} (add it to .gitignore)", dir.display());
}

const USAGE: &str = "usage: ovenv <command>

  init              create .ovenv/ in the current directory
  run <cmd>...      run a command with its writes staged
  shell             start $SHELL with writes staged
  diff [--content]  show staged changes (--content adds diff -u for modified files)
  apply [--force]   write staged changes to the host
  discard           throw staged changes away

Run with sudo to stage writes to system paths (/usr, /etc, ...).";

fn usage_error() -> ! {
    eprintln!("{USAGE}");
    exit(2)
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let cmd = args.first().and_then(|a| a.to_str()).unwrap_or("");
    let flag = |name: &str| match args.get(1).and_then(|a| a.to_str()) {
        None => false,
        Some(a) if a == name && args.len() == 2 => true,
        _ => usage_error(),
    };
    match cmd {
        "init" => init(),
        "run" if args.len() > 1 => run(Env::load(false), args[1..].to_vec()),
        "run" => die!("usage: ovenv run <cmd>..."),
        "shell" => run(
            Env::load(false),
            vec![std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into())],
        ),
        "diff" => {
            let content = flag("--content");
            show_diff(&Env::load(false), content)
        }
        "apply" => {
            let force = flag("--force");
            apply(&Env::load(false), force)
        }
        "discard" => {
            let env = Env::load(true);
            let _lock = lock(&env);
            discard_files(&env);
        }
        "--version" => println!("ovenv {}", env!("CARGO_PKG_VERSION")),
        "-h" | "--help" | "help" => println!("{USAGE}"),
        _ => usage_error(),
    }
}
