//! `ovenv diff`.

use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::changes::{changes, Kind};
use crate::env::Env;
use crate::files::{is_dir, kind_name, lstat, walk};

pub(crate) fn human(bytes: u64) -> String {
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

pub(crate) fn describe(p: &Path) -> String {
    match lstat(p) {
        Some(m) if m.file_type().is_symlink() => format!(
            "symlink -> {}",
            fs::read_link(p).unwrap_or_default().display()
        ),
        Some(m) => kind_name(&m).into(),
        None => "missing".into(),
    }
}

pub(crate) fn show_diff(env: &Env, content: bool) {
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
    let staged: u64 = walk(&env.state().join("upper"), &mut Vec::new())
        .iter()
        .filter_map(|r| lstat(&env.state().join("upper").join(r)))
        .map(|m| m.blocks() * 512)
        .sum();
    let _ = writeln!(
        out,
        "\n{} staged in {}{}",
        human(staged),
        env.state().display(),
        if env.state().starts_with("/tmp") {
            " (lost on reboot)"
        } else {
            ""
        }
    );
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
