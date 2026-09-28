//! The host state recorded per staged path, which apply checks for conflicts.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::env::Env;
use crate::files::{attrs, file_hash, hex, is_dir, is_opaque, kind_name, lstat, walk};
use crate::or_die;

/// What the host has at `h`, precise enough to notice any change apply could overwrite.
pub(crate) fn host_state(env: &Env, h: &Path, up: &Path) -> String {
    let Some(m) = lstat(h) else { return "-".into() };
    let t = m.file_type();
    if t.is_symlink() {
        format!("l {}", fs::read_link(h).unwrap_or_default().display())
    } else if t.is_dir() {
        let mut s = format!("d {}", attrs(&m));
        // contents only matter when the staged entry deletes or replaces the whole directory
        if !is_dir(up) || is_opaque(env, up) {
            let mut hasher = Sha256::new();
            let mut bad = Vec::new();
            let entries = walk(h, &mut bad);
            for rel in &bad {
                hasher.update(b"unreadable\0");
                hasher.update(rel.as_os_str().as_bytes());
            }
            for rel in entries {
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

pub(crate) fn read_base(env: &Env) -> HashMap<PathBuf, String> {
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
pub(crate) fn record_base(env: &Env) {
    let seen = read_base(env);
    let mut buf = Vec::new();
    for p in env.overlays() {
        let u = env.upper(&p);
        let mut bad = Vec::new();
        let entries = walk(&u, &mut bad);
        for rel in &bad {
            eprintln!(
                "ovenv: cannot read staged {}; it is recorded once it is readable",
                p.join(rel).display()
            );
        }
        let entries = entries.into_iter().map(|rel| (p.join(&rel), u.join(&rel)));
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
