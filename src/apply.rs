//! `ovenv apply` and `ovenv discard`.

use std::collections::HashSet;
use std::fs;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::io::Read;
use std::os::unix::fs::{
    fchown, lchown, symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt,
};
use std::path::Path;

use crate::baseline::{host_state, read_base};
use crate::changes::{changes, Kind};
use crate::env::{lock, Env, Mode};
use crate::files::{copy_xattrs, hex, is_dir, lstat, unlock_tree};
use crate::or_die;

pub(crate) fn random_suffix() -> String {
    let mut b = [0u8; 6];
    let ok = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    or_die(ok, "cannot read /dev/urandom");
    hex(&b)
}

pub(crate) fn own(env: &Env, m: &Metadata, p: &Path) -> io::Result<()> {
    if env.mode == Mode::Root {
        lchown(p, Some(m.uid()), Some(m.gid()))?;
    }
    Ok(())
}

/// Put a copy of regular file or symlink `up` at `h` with rename(2), so `h` is never missing or half-written.
pub(crate) fn put(env: &Env, up: &Path, h: &Path) -> Result<(), String> {
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

pub(crate) fn remove(p: &Path) -> io::Result<()> {
    match lstat(p) {
        Some(m) if m.is_dir() => fs::remove_dir_all(p),
        Some(_) => fs::remove_file(p),
        None => Ok(()),
    }
}

pub(crate) fn apply(env: &Env, force: bool, drop_skipped: bool) {
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
    // A SKIP entry's staged copy is its only copy, so dropping it has to be asked for.
    let unsupported: Vec<_> = list.iter().filter(|c| c.kind == Kind::Skip).collect();
    if !unsupported.is_empty() && !drop_skipped {
        eprintln!("ovenv: these staged changes can't be applied:");
        for c in &unsupported {
            eprintln!("  {} ({})", c.host.display(), c.note);
        }
        die!("nothing was changed; rerun with --drop-skipped to apply the rest and discard these");
    }
    // Everything apply will copy must be readable up front, or it would stop halfway with the host half-changed.
    for c in &list {
        let copies = matches!(c.kind, Kind::Add | Kind::Modify | Kind::Replace);
        if copies && lstat(&c.up).is_some_and(|m| m.is_file()) && File::open(&c.up).is_err() {
            die!(
                "cannot read staged {}; nothing was changed. Make it readable in `ovenv run`, or `ovenv discard`",
                c.host.display()
            );
        }
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

/// Remove the staged state; .ovenv/paths stays.
pub(crate) fn discard_files(env: &Env) {
    if let Some(st) = env.state.as_ref().filter(|st| lstat(st).is_some()) {
        // overlay leaves a mode-000 dir in workdir, and staged dirs may be 000 too; both block removal for non-root
        unlock_tree(st);
        or_die(remove(st), format!("cannot remove {}", st.display()));
    }
    for name in ["state", "mode"] {
        let p = env.dir.join(name);
        or_die(remove(&p), format!("cannot remove {}", p.display()));
    }
}
