//! apply's write-ahead journal. Each step is recorded before it touches the host, so a failed or
//! interrupted apply rolls back to the host as it was, and one that got as far as `commit` is finished.

use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{lchown, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::apply::{discard_files, remove};
use crate::env::{Env, Mode};
use crate::files::{hex, is_dir, lstat, set_xattrs, sync_path, unlock_tree, xattrs};
use crate::or_die;

pub(crate) enum Step {
    /// A temp file apply is about to create; removed on rollback and on commit.
    Temp(PathBuf),
    /// A path that didn't exist on the host; rollback removes it.
    Add(PathBuf),
    /// The host's `host`, moved or hard-linked to `old`; rollback puts it back, commit removes `old`.
    Keep { host: PathBuf, old: PathBuf },
    /// Attributes of `host` before apply changed them.
    Attr {
        host: PathBuf,
        mode: u32,
        uid: u32,
        gid: u32,
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

impl Step {
    pub(crate) fn attr(host: &Path) -> io::Result<Step> {
        let m = fs::symlink_metadata(host)?;
        Ok(Step::Attr {
            host: host.to_path_buf(),
            mode: m.mode() & 0o7777,
            uid: m.uid(),
            gid: m.gid(),
            xattrs: xattrs(host),
        })
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let mut put = |b: &[u8]| {
            out.extend_from_slice(b);
            out.push(0);
        };
        match self {
            Step::Temp(p) => {
                put(b"temp");
                put(p.as_os_str().as_bytes());
            }
            Step::Add(p) => {
                put(b"add");
                put(p.as_os_str().as_bytes());
            }
            Step::Keep { host, old } => {
                put(b"keep");
                put(host.as_os_str().as_bytes());
                put(old.as_os_str().as_bytes());
            }
            Step::Attr {
                host,
                mode,
                uid,
                gid,
                xattrs,
            } => {
                put(b"attr");
                put(host.as_os_str().as_bytes());
                for n in [*mode, *uid, *gid, xattrs.len() as u32] {
                    put(n.to_string().as_bytes());
                }
                for (name, value) in xattrs {
                    put(name);
                    put(hex(value).as_bytes());
                }
            }
        }
    }

    fn undo(&self, env: &Env) -> io::Result<()> {
        match self {
            Step::Temp(p) => remove(p),
            Step::Add(p) => {
                unlock_tree(p);
                remove(p)
            }
            Step::Keep { host, old } => {
                if lstat(old).is_none() {
                    return Ok(());
                }
                // Anything at `host` now is apply's; a dir there would block the rename.
                if is_dir(host) {
                    unlock_tree(host);
                    remove(host)?;
                }
                fs::rename(old, host)?;
                // rename is a no-op when both are links to one inode (crashed right after the link)
                if lstat(old).is_some() {
                    remove(old)?;
                }
                Ok(())
            }
            Step::Attr {
                host,
                mode,
                uid,
                gid,
                xattrs,
            } => {
                let Some(m) = lstat(host) else { return Ok(()) };
                if env.mode == Mode::Root {
                    lchown(host, Some(*uid), Some(*gid))?;
                }
                if !m.file_type().is_symlink() {
                    fs::set_permissions(host, fs::Permissions::from_mode(*mode))?;
                }
                // after chown, which clears file capabilities
                set_xattrs(host, xattrs)
            }
        }
    }

    fn finish(&self) -> io::Result<()> {
        match self {
            Step::Temp(p) => remove(p),
            Step::Keep { old, .. } => {
                unlock_tree(old);
                remove(old)
            }
            Step::Add(_) | Step::Attr { .. } => Ok(()),
        }
    }
}

/// Parse a journal into its steps, whether it committed, how many steps rollback already undid,
/// and the length of its complete records. A record cut short by a crash is dropped, since its step never started.
fn parse(data: &[u8]) -> (Vec<Step>, bool, usize, usize) {
    let mut tok = data.split(|&b| b == 0);
    // the piece after the last NUL is incomplete (or empty)
    let complete = data.iter().filter(|&&b| b == 0).count();
    let mut left = complete;
    let pos = Cell::new(0);
    let mut next = || {
        if left == 0 {
            return None;
        }
        left -= 1;
        let t = tok.next()?;
        pos.set(pos.get() + t.len() + 1);
        Some(t)
    };
    let path = |b: &[u8]| PathBuf::from(OsStr::from_bytes(b));
    let num = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<u32>().ok();
    let mut steps = Vec::new();
    let mut committed = false;
    let mut undone = 0;
    let mut valid = 0;
    while let Some(op) = next() {
        let step = match op {
            b"commit" => {
                committed = true;
                valid = pos.get();
                continue;
            }
            b"undone" => {
                undone += 1;
                valid = pos.get();
                continue;
            }
            b"temp" => next().map(|p| Step::Temp(path(p))),
            b"add" => next().map(|p| Step::Add(path(p))),
            b"keep" => (|| {
                Some(Step::Keep {
                    host: path(next()?),
                    old: path(next()?),
                })
            })(),
            b"attr" => (|| {
                let host = path(next()?);
                let mode = num(next()?)?;
                let uid = num(next()?)?;
                let gid = num(next()?)?;
                let n = num(next()?)?;
                let mut xattrs = Vec::new();
                for _ in 0..n {
                    let name = next()?.to_vec();
                    xattrs.push((name, unhex(next()?)?));
                }
                Some(Step::Attr {
                    host,
                    mode,
                    uid,
                    gid,
                    xattrs,
                })
            })(),
            _ => None,
        };
        match step {
            Some(s) => {
                steps.push(s);
                valid = pos.get();
            }
            None => break,
        }
    }
    (steps, committed, undone, valid)
}

fn unhex(s: &[u8]) -> Option<Vec<u8>> {
    s.chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok())
        .collect()
}

pub(crate) struct Journal {
    path: PathBuf,
    f: File,
    steps: Vec<Step>,
    /// how many of the newest steps rollback has undone
    undone: usize,
}

impl Journal {
    pub(crate) fn create(env: &Env) -> Result<Journal, String> {
        let path = env.dir.join("journal");
        let f = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        File::open(&env.dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| format!("cannot sync {}: {e}", env.dir.display()))?;
        Ok(Journal {
            path,
            f,
            steps: Vec::new(),
            undone: 0,
        })
    }

    /// The journal an interrupted apply left, and whether it had committed.
    fn open(env: &Env) -> Option<(Journal, bool)> {
        let path = env.dir.join("journal");
        let data = fs::read(&path).ok()?;
        let (steps, committed, undone, valid) = parse(&data);
        let f = or_die(
            OpenOptions::new().append(true).open(&path),
            format!("cannot open {}", path.display()),
        );
        // Drop a torn tail, or the markers rollback appends would join it and be lost.
        or_die(
            f.set_len(valid as u64).and_then(|()| f.sync_data()),
            format!("cannot truncate {}", path.display()),
        );
        Some((
            Journal {
                path,
                f,
                steps,
                undone,
            },
            committed,
        ))
    }

    fn append(&mut self, buf: &[u8]) -> Result<(), String> {
        self.f
            .write_all(buf)
            .and_then(|()| self.f.sync_data())
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }

    /// Record steps durably; only then may they touch the host.
    // ponytail: one fsync per change; batch them if applying tens of thousands of files is too slow
    pub(crate) fn log(&mut self, steps: impl IntoIterator<Item = Step>) -> Result<(), String> {
        let mut buf = Vec::new();
        let start = self.steps.len();
        self.steps.extend(steps);
        for s in &self.steps[start..] {
            s.encode(&mut buf);
        }
        self.append(&buf)
    }

    /// Undo steps newest first. Each undo is synced and then marked, so a rollback cut short resumes
    /// where it stopped instead of undoing an earlier step on top of what it already restored.
    pub(crate) fn rollback(mut self, env: &Env) -> Result<(), String> {
        while self.undone < self.steps.len() {
            let s = &self.steps[self.steps.len() - 1 - self.undone];
            s.undo(env)
                .map_err(|e| format!("cannot roll back {}: {e}", target(s)))?;
            match s {
                Step::Attr { host, .. } => sync_path(host),
                Step::Temp(p) | Step::Add(p) | Step::Keep { host: p, .. } => {
                    sync_path(p.parent().unwrap_or(Path::new("/")))
                }
            }
            self.append(b"undone\0")?;
            self.undone += 1;
        }
        self.remove()
    }

    /// Commit and finish. New contents only need to be on disk once rollback is no longer possible.
    pub(crate) fn commit(mut self, env: &Env) -> Result<(), String> {
        // SAFETY: sync(2) takes no arguments and cannot fail.
        unsafe { libc::sync() };
        self.append(b"commit\0")?;
        self.finish(env)
    }

    /// Remove the backups and the staged state. The journal goes last, so a crash in between is
    /// finished by recover() instead of leaving applied changes staged.
    fn finish(self, env: &Env) -> Result<(), String> {
        for s in &self.steps {
            s.finish()
                .map_err(|e| format!("cannot clean up {}: {e}", target(s)))?;
        }
        discard_files(env);
        // SAFETY: sync(2) takes no arguments and cannot fail.
        unsafe { libc::sync() };
        self.remove()
    }

    fn remove(self) -> Result<(), String> {
        fs::remove_file(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        sync_path(self.path.parent().unwrap_or(Path::new("/")));
        Ok(())
    }
}

fn target(s: &Step) -> std::path::Display<'_> {
    match s {
        Step::Temp(p) | Step::Add(p) => p.display(),
        Step::Keep { host, .. } | Step::Attr { host, .. } => host.display(),
    }
}

/// Settle an apply that was interrupted: roll it back, or finish it if it had committed.
pub(crate) fn recover(env: &Env) {
    let Some((j, committed)) = Journal::open(env) else {
        return;
    };
    let path = j.path.clone();
    let r = if committed {
        j.finish(env)
    } else {
        j.rollback(env)
    };
    if let Err(e) = r {
        die!(
            "an interrupted apply left {}: {e}; fix it and rerun",
            path.display()
        );
    }
    if committed {
        die!("finished an interrupted apply; its changes are on the host");
    }
    eprintln!("ovenv: rolled back an interrupted apply; the host is as it was before it");
}

pub(crate) fn interrupted(env: &Env) -> bool {
    lstat(&env.dir.join("journal")).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_torn_tail() {
        let steps = [
            Step::Temp("/a/.ovenv.1".into()),
            Step::Keep {
                host: "/a/b\nc".into(),
                old: "/a/.ovenv-old.1".into(),
            },
            Step::Attr {
                host: "/d".into(),
                mode: 0o755,
                uid: 1,
                gid: 2,
                xattrs: vec![(b"security.capability".to_vec(), vec![0, 1, 255])],
            },
        ];
        let mut buf = Vec::new();
        for s in &steps {
            s.encode(&mut buf);
        }
        let (got, committed, undone, valid) = parse(&buf);
        assert!(!committed && undone == 0 && valid == buf.len());
        assert_eq!(got.len(), 3);
        match &got[2] {
            Step::Attr { mode, xattrs, .. } => {
                assert_eq!(*mode, 0o755);
                assert_eq!(xattrs[0].1, vec![0, 1, 255]);
            }
            _ => panic!("expected attr"),
        }
        // a record cut mid-way is dropped
        let cut = buf.len() - 3;
        let (got, _, _, valid) = parse(&buf[..cut]);
        assert_eq!(got.len(), 2);
        // a marker appended after truncating the torn tail is read back
        let mut torn = buf[..valid].to_vec();
        torn.extend_from_slice(b"undone\0");
        assert_eq!(parse(&torn).2, 1);
        buf.extend_from_slice(b"undone\0undone\0");
        assert_eq!(parse(&buf).2, 2);
        buf.extend_from_slice(b"commit\0");
        assert!(parse(&buf).1);
    }
}
