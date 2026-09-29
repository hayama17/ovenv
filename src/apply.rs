//! `ovenv apply` and `ovenv discard`.

use std::collections::HashSet;
use std::fs;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::io::Read;
use std::os::unix::fs::{
    fchown, lchown, symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt,
};
use std::path::{Path, PathBuf};

use crate::baseline::{host_state, read_base};
use crate::changes::{changes, Change, Kind};
use crate::env::{lock, Env, Mode};
use crate::files::{copy_xattrs, hex, is_dir, lstat, sync_path, unlock_tree};
use crate::journal::{Journal, Step};
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

/// Write a copy of regular file or symlink `up` to `tmp`, which must not exist yet.
fn prepare(env: &Env, up: &Path, tmp: &Path) -> io::Result<()> {
    let um = fs::symlink_metadata(up)?;
    let made = if um.file_type().is_symlink() {
        fs::read_link(up)
            .and_then(|t| symlink(t, tmp))
            .and_then(|()| copy_xattrs(up, tmp))
    } else {
        (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(tmp)?;
            io::copy(&mut File::open(up)?, &mut f)?;
            if env.mode == Mode::Root {
                fchown(&f, Some(um.uid()), Some(um.gid()))?;
            }
            // after chown, which clears setuid bits
            f.set_permissions(fs::Permissions::from_mode(um.mode() & 0o7777))?;
            copy_xattrs(up, tmp)
        })()
    };
    if made.is_err() {
        let _ = fs::remove_file(tmp);
    }
    made
}

/// A name next to `h` that nothing uses yet.
fn unused(h: &Path, tag: &str) -> PathBuf {
    let dir = h.parent().unwrap_or(Path::new("/"));
    loop {
        let p = dir.join(format!("{tag}{}", random_suffix()));
        if lstat(&p).is_none() {
            return p;
        }
    }
}

/// Move whatever is at `h` aside, so rollback can put it back.
fn set_aside(j: &mut Journal, h: &Path) -> Result<(), String> {
    if lstat(h).is_none() {
        return Ok(());
    }
    let old = unused(h, ".ovenv-old.");
    j.log([Step::Keep {
        host: h.to_path_buf(),
        old: old.clone(),
    }])?;
    fs::rename(h, &old).map_err(|e| format!("cannot move {} aside: {e}", h.display()))?;
    // Later steps' rollback assumes the original is out of the way, even after a power loss.
    sync_path(h.parent().unwrap_or(Path::new("/")));
    Ok(())
}

/// Put a copy of regular file or symlink `up` at `h` with rename(2), so an existing `h` is never missing or half-written.
fn put(env: &Env, j: &mut Journal, up: &Path, h: &Path) -> Result<(), String> {
    let tmp = unused(h, ".ovenv.");
    // An existing file (MODIFY, or ADD with --force) stays reachable as a hard link until commit.
    let keep = lstat(h).is_some().then(|| unused(h, ".ovenv-old."));
    let step = match &keep {
        Some(old) => Step::Keep {
            host: h.to_path_buf(),
            old: old.clone(),
        },
        None => Step::Add(h.to_path_buf()),
    };
    j.log([Step::Temp(tmp.clone()), step])?;
    prepare(env, up, &tmp).map_err(|e| format!("cannot prepare {}: {e}", h.display()))?;
    if let Some(old) = &keep {
        // ponytail: needs hard links in the target dir; copy the old file instead if a filesystem lacks them
        fs::hard_link(h, old).map_err(|e| format!("cannot keep the old {}: {e}", h.display()))?;
        sync_path(h.parent().unwrap_or(Path::new("/")));
    }
    fs::rename(&tmp, h).map_err(|e| format!("cannot replace {}: {e}", h.display()))
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

    let mut j = Journal::create(env).unwrap_or_else(|e| die!("{e}"));
    let mut skipped = Vec::new();
    if let Err(e) = write_all(env, &mut j, &list, &mut skipped) {
        match j.rollback(env) {
            Ok(()) => die!("{e}; rolled back, the host is unchanged"),
            Err(r) => die!("{e}; {r}; fix it and rerun ovenv to finish the rollback"),
        }
    }
    if let Err(e) = j.commit(env) {
        die!("applied, but {e}; rerun ovenv to finish");
    }
    for s in &skipped {
        println!("not applied: {s}");
    }
    println!("applied {} change(s)", list.len() - skipped.len());
}

fn write_all(
    env: &Env,
    j: &mut Journal,
    list: &[Change],
    skipped: &mut Vec<String>,
) -> Result<(), String> {
    let mut dirmodes = Vec::new();
    for c in list {
        let h = &c.host;
        let fail = |e: io::Error| format!("{}: {e}", h.display());
        let staged = || {
            lstat(&c.up).ok_or_else(|| format!("{} vanished from the staged changes", h.display()))
        };
        match c.kind {
            Kind::Skip => skipped.push(format!("{} ({})", h.display(), c.note)),
            Kind::Delete => set_aside(j, h)?,
            Kind::Attr => {
                let um = staged()?;
                j.log([Step::attr(h).map_err(fail)?])?;
                own(env, &um, h).map_err(fail)?;
                if um.is_dir() {
                    dirmodes.push((h.clone(), um.mode()));
                } else {
                    fs::set_permissions(h, fs::Permissions::from_mode(um.mode() & 0o7777))
                        .map_err(fail)?;
                }
                copy_xattrs(&c.up, h).map_err(fail)?;
            }
            Kind::Add | Kind::Replace | Kind::Modify => {
                if c.kind == Kind::Replace {
                    set_aside(j, h)?;
                }
                if is_dir(&c.up) {
                    let um = staged()?;
                    j.log([Step::Add(h.clone())])?;
                    fs::DirBuilder::new().mode(0o700).create(h).map_err(fail)?;
                    own(env, &um, h).map_err(fail)?;
                    copy_xattrs(&c.up, h).map_err(fail)?;
                    dirmodes.push((h.clone(), um.mode()));
                } else {
                    put(env, j, &c.up, h)?;
                }
            }
        }
    }
    // Directory modes go last, deepest first, so a read-only directory doesn't block writes below it.
    for (h, mode) in dirmodes.iter().rev() {
        let fail = |e: io::Error| format!("{}: {e}", h.display());
        j.log([Step::attr(h).map_err(fail)?])?;
        fs::set_permissions(h, fs::Permissions::from_mode(mode & 0o7777)).map_err(fail)?;
    }
    Ok(())
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
