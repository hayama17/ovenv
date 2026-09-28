//! The list of staged changes that diff shows and apply writes.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use crate::env::Env;
use crate::files::{attrs, is_opaque, is_whiteout, lstat, same_content, walk_staged, xattrs};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Add,
    Modify,
    Attr,
    Delete,
    Replace,
    Skip,
}

impl Kind {
    pub(crate) fn label(self) -> &'static str {
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

pub(crate) struct Change {
    pub(crate) kind: Kind,
    pub(crate) host: PathBuf,
    pub(crate) up: PathBuf,
    pub(crate) note: &'static str,
}

/// Every real change, in apply order. diff, the conflict check and apply all use this.
pub(crate) fn changes(env: &Env) -> Vec<Change> {
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
        for rel in walk_staged(&u, &p) {
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
