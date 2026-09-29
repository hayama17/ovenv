//! The .ovenv/ environment: staged paths, the state directory, the lock and session checks.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::files::{is_dir, lstat};
use crate::or_die;

// /var itself stays unstaged so the state directory can live in /var/tmp.
pub(crate) const ROOT_DEFAULT: &[&str] = &[
    "/usr",
    "/opt",
    "/etc",
    "/var/lib",
    "/var/cache",
    "/var/log",
    "/var/spool",
    "/var/opt",
    "/root",
    "rw /var/tmp",
    "rw .",
];

pub(crate) const USER_DEFAULT: &[&str] = &[
    "~/.local",
    "~/.cargo",
    "~/.npm",
    "~/.cache",
    "~/.config",
    "rw .",
];

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Root,
    User,
}

pub(crate) struct Env {
    /// .ovenv/: paths, mode, lock and the pointer to the state directory
    pub(crate) dir: PathBuf,
    pub(crate) project: PathBuf,
    pub(crate) mode: Mode,
    /// /var/tmp/ovenv-*/ (or /tmp/ovenv-*/): upper, work and base
    pub(crate) state: Option<PathBuf>,
}

impl Env {
    /// `allow_gone` lets discard clean up after the state directory vanished (e.g. a reboot).
    pub(crate) fn load(allow_gone: bool) -> Env {
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

    pub(crate) fn state(&self) -> &Path {
        self.state.as_deref().expect("state directory not set")
    }

    pub(crate) fn upper(&self, p: &Path) -> PathBuf {
        under(&self.state().join("upper"), p)
    }

    pub(crate) fn opaque_xattr(&self) -> &'static str {
        match self.mode {
            Mode::Root => "trusted.overlay.opaque",
            Mode::User => "user.overlay.opaque",
        }
    }

    /// (writable straight through, path) for every line of .ovenv/paths or the defaults.
    /// .ovenv/paths, or the defaults when it doesn't exist.
    pub(crate) fn paths_text(&self) -> String {
        match fs::read_to_string(self.dir.join("paths")) {
            Ok(t) => t,
            Err(_) => match self.mode {
                Mode::Root => ROOT_DEFAULT.join("\n"),
                Mode::User => USER_DEFAULT.join("\n"),
            },
        }
    }

    pub(crate) fn paths(&self) -> Vec<(bool, PathBuf)> {
        let text = self.paths_text();
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
                // relative to the project, not to wherever ovenv happens to run
                self.project.join(p)
            };
            out.push((rw, resolve(&p)));
        }
        out
    }

    /// Staged paths as `.ovenv/paths` asks for them. A path that doesn't exist yet is staged through
    /// its nearest existing parent, so the command can create it without ovenv touching the host.
    pub(crate) fn wanted_overlays(&self, notify: bool) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for (rw, p) in self.paths() {
            if rw {
                continue;
            }
            let mut q = p.as_path();
            while !is_dir(q) {
                match q.parent() {
                    Some(parent) => q = parent,
                    None => break,
                }
            }
            if notify && q != p {
                eprintln!(
                    "ovenv: {} doesn't exist; staging {} instead",
                    p.display(),
                    q.display()
                );
            }
            out.push(q.to_path_buf());
        }
        // A path under another staged path is already staged by it; mounting both would nest one upper in the other.
        let all = out.clone();
        let mut seen = HashSet::new();
        out.retain(|p| !all.iter().any(|q| q != p && p.starts_with(q)) && seen.insert(p.clone()));
        out
    }

    /// The staged paths, fixed when staging starts: editing .ovenv/paths or creating a path on the
    /// host mid-session must not hide what is already staged.
    pub(crate) fn overlays(&self) -> Vec<PathBuf> {
        match self
            .state
            .as_ref()
            .and_then(|st| fs::read(st.join("overlays")).ok())
        {
            Some(data) => data
                .split(|&b| b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| PathBuf::from(OsStr::from_bytes(p)))
                .collect(),
            None => self.wanted_overlays(false),
        }
    }
}

/// /var/tmp survives a reboot; /tmp is the fallback when /var/tmp is staged.
const STATE_BASES: [&str; 2] = ["/var/tmp", "/tmp"];

/// The state directory holds staged file contents, so refuse anything we didn't create for ourselves.
pub(crate) fn check_state(st: &Path) {
    let m = lstat(st);
    let ok = st
        .parent()
        .is_some_and(|d| STATE_BASES.iter().any(|b| d == Path::new(b)))
        && st
            .file_name()
            .is_some_and(|n| n.as_bytes().starts_with(b"ovenv-"))
        && m.as_ref().is_some_and(|m| {
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0
        });
    if !ok {
        die!(
            "refusing to use {}: expected a /var/tmp/ovenv-* or /tmp/ovenv-* directory owned by you with mode 700",
            st.display()
        );
    }
}

pub(crate) fn create_state(env: &mut Env) {
    // The state can't sit under a staged path, since overlay refuses an upper inside its lower.
    let staged = env.wanted_overlays(false);
    let base = STATE_BASES
        .into_iter()
        .find(|b| !staged.iter().any(|p| Path::new(b).starts_with(p)))
        .unwrap_or("/tmp");
    if base == "/tmp" {
        eprintln!(
            "ovenv: /var/tmp is staged, so staged changes go to /tmp and won't survive a reboot"
        );
    }
    let mut tmpl = format!("{base}/ovenv-XXXXXXXX\0").into_bytes();
    // mkdtemp picks an unguessable name and creates it with mode 700
    if unsafe { libc::mkdtemp(tmpl.as_mut_ptr() as *mut libc::c_char) }.is_null() {
        die!(
            "cannot create a state directory in {base}: {}",
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

pub(crate) fn find_dir() -> PathBuf {
    let cwd = or_die(std::env::current_dir(), "cannot read the current directory");
    for d in cwd.ancestors() {
        if d != Path::new("/") && d.join(".ovenv").is_dir() {
            return d.join(".ovenv");
        }
    }
    cwd.join(".ovenv")
}

/// Like `realpath -m`: resolve symlinks in the part that exists, keep the rest as written.
pub(crate) fn resolve(p: &Path) -> PathBuf {
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
pub(crate) fn under(base: &Path, p: &Path) -> PathBuf {
    base.join(p.strip_prefix("/").unwrap_or(p))
}

/// One run, shell, apply or discard per .ovenv; held until that command has fully finished.
pub(crate) fn lock(env: &Env) -> File {
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
    check_session(env);
    f
}

pub(crate) fn mnt_ns(pid: &str) -> Option<(u64, u64)> {
    fs::metadata(format!("/proc/{pid}/ns/mnt"))
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

/// A session's mount namespace outlives `ovenv run` while anything it started keeps running;
/// applying or discarding then would change the layers under a live overlay.
pub(crate) fn check_session(env: &Env) {
    let Some(st) = &env.state else { return };
    let Ok(id) = fs::read_to_string(st.join("session")) else {
        return;
    };
    let parsed = id
        .trim()
        .split_once(':')
        .and_then(|(d, i)| Some((d.parse::<u64>().ok()?, i.parse::<u64>().ok()?)));
    let Some(ns) = parsed else { return };
    // ponytail: a namespace inode can be reused after the session ends; that only causes a false refusal
    let mut pids: Vec<u32> = fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| mnt_ns(&pid.to_string()) == Some(ns))
        .collect();
    if !pids.is_empty() {
        pids.sort();
        let list: Vec<String> = pids.iter().map(u32::to_string).collect();
        die!(
            "processes started in {} are still running (pid {}); stop them first",
            env.dir.display(),
            list.join(" ")
        );
    }
}
