//! `ovenv run`: build the staged view in a private mount namespace and exec the command.

use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::fs::File;
use std::io;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::exit;

use crate::apply::own;
use crate::baseline::record_base;
use crate::env::{create_state, lock, mnt_ns, under, Env, Mode};
use crate::files::{copy_xattrs, lstat};
use crate::or_die;
use crate::sys::{check, cstr, escape_opt, mount, move_mount, open_tree, set_readonly};

pub(crate) fn run(mut env: Env, cmd: Vec<OsString>) -> ! {
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

    let mut rw = vec![PathBuf::from("/tmp")];
    for (is_rw, p) in env.paths() {
        if !is_rw {
            continue;
        }
        if lstat(&p).is_none() {
            // creating it on the host would be a side effect ovenv exists to avoid
            eprintln!(
                "ovenv: rw path {} doesn't exist on the host; writes there are staged or refused",
                p.display()
            );
        }
        rw.push(p);
    }
    let recorded = env.state().join("overlays");
    let fresh = lstat(&recorded).is_none();
    // Compare the text, not the resolved set, which also moves when the host changes.
    let paths_at_start = env.state().join("paths");
    let candidates = if fresh {
        or_die(
            fs::write(&paths_at_start, env.paths_text()),
            "cannot record .ovenv/paths",
        );
        env.wanted_overlays(true)
    } else {
        if fs::read_to_string(&paths_at_start).ok() != Some(env.paths_text()) {
            eprintln!("ovenv: .ovenv/paths changed since staging started; the staged paths stay until apply or discard");
        }
        env.overlays()
    };
    let mut ov = Vec::new();
    for p in candidates {
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
    if fresh {
        let list: Vec<u8> = ov
            .iter()
            .flat_map(|p| p.as_os_str().as_bytes().iter().copied().chain([0]))
            .collect();
        or_die(fs::write(&recorded, list), "cannot record the staged paths");
    }
    // The merged root takes its mode, owner and xattrs from the upper dir, so start it as a copy of the host dir.
    for p in &ov {
        let upper = env.upper(p);
        if lstat(&upper).is_some() {
            continue;
        }
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

    // The child reports its mount namespace through this pipe, so later commands can spot leftovers.
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        die!("pipe: {}", io::Error::last_os_error());
    }
    let (mut from_child, to_parent) = unsafe {
        use std::os::fd::FromRawFd;
        (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1]))
    };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        die!("fork: {}", io::Error::last_os_error());
    }
    if pid == 0 {
        drop(from_child);
        let err = enter(env, &ov, &rw, &cwd, (uid, gid), &cmd, to_parent);
        eprintln!("ovenv: {err}");
        unsafe { libc::_exit(127) }
    }
    drop(to_parent);
    let mut ns = String::new();
    let _ = from_child.read_to_string(&mut ns);
    if !ns.is_empty() {
        or_die(
            fs::write(env.state().join("session"), ns),
            "cannot record the session",
        );
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

pub(crate) fn write_proc(file: &str, data: &str) -> Result<(), String> {
    fs::write(file, data).map_err(|e| format!("{file}: {e}"))
}

/// Runs in the forked child: build the staged view, then exec. Returns only on error.
pub(crate) fn enter(
    env: &Env,
    ov: &[PathBuf],
    rw: &[PathBuf],
    cwd: &Path,
    (uid, gid): (u32, u32),
    cmd: &[OsString],
    report: File,
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
        if let Some((dev, ino)) = mnt_ns("self") {
            let _ = (&report).write_all(format!("{dev}:{ino}").as_bytes());
        }
        drop(report);
        mount(
            None,
            Path::new("/"),
            None,
            libc::MS_REC | libc::MS_PRIVATE,
            None,
        )?;

        // Grab the host's view of rw paths now, before an overlay above them would hide it.
        let mut clones = HashMap::new();
        for p in rw {
            if fs::symlink_metadata(p).is_ok() {
                clones.insert(p, open_tree(p)?);
            }
        }

        let xopt = if env.mode == Mode::User {
            ",userxattr"
        } else {
            ""
        };
        // Outer paths first, so the inner one wins either way: a staged dir inside the project,
        // or an rw dir inside a staged ~. Overlay needs a writable upper, so all this precedes the read-only step.
        let mut order: Vec<(&PathBuf, bool)> = ov
            .iter()
            .map(|p| (p, false))
            .chain(clones.keys().map(|p| (*p, true)))
            .collect();
        order.sort_by_key(|(p, is_rw)| (p.components().count(), *is_rw));
        for (p, is_rw) in &order {
            if *is_rw {
                move_mount(&clones[p], p)?;
                continue;
            }
            let upper = env.upper(p);
            let work = under(&env.state().join("work"), p);
            for d in [&upper, &work] {
                fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
            }
            let mut opts = Vec::new();
            for (key, dir) in [
                ("lowerdir=", *p),
                (",upperdir=", &upper),
                (",workdir=", &work),
            ] {
                opts.extend_from_slice(key.as_bytes());
                opts.extend(escape_opt(dir));
            }
            opts.extend_from_slice(xopt.as_bytes());
            let opts = OsStr::from_bytes(&opts);
            mount(Some(OsStr::new("ovenv")), p, Some("overlay"), 0, Some(opts))?;
        }
        // Anything not overlaid or passed through is read-only, so writes can't silently reach the host.
        set_readonly(Path::new("/"), true, true)?;
        set_readonly(Path::new("/proc"), false, true)?;
        set_readonly(Path::new("/dev"), false, true)?;
        for (p, _) in &order {
            set_readonly(p, false, false)?;
        }
        // The command may read ovenv's own bookkeeping but must not change it.
        for p in [&env.dir, env.state()] {
            mount(Some(p.as_os_str()), p, None, libc::MS_BIND, None)?;
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
