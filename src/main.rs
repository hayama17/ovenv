//! ovenv: stage a command's filesystem side effects in OverlayFS, then review, apply or discard them.

macro_rules! die {
    ($($a:tt)*) => {{ eprintln!("ovenv: {}", format!($($a)*)); std::process::exit(1) }};
}

mod apply;
mod baseline;
mod changes;
mod diff;
mod env;
mod files;
mod run;
mod sys;

use std::ffi::OsString;
use std::fs;
use std::process::exit;

use crate::apply::{apply, discard_files};
use crate::diff::show_diff;
use crate::env::{lock, Env, ROOT_DEFAULT, USER_DEFAULT};
use crate::run::run;

pub(crate) fn or_die<T, E: std::fmt::Display>(r: Result<T, E>, what: impl std::fmt::Display) -> T {
    r.unwrap_or_else(|e| die!("{what}: {e}"))
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
         # Relative paths are relative to this project. 'rw .' lets the project write\n\
         # straight through; delete it to stage the project as well.\n\
         # Everything else is read-only inside ovenv, except /tmp, /dev and /proc.\n{}\n",
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
  apply [--force] [--drop-skipped]
                    write staged changes to the host
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
            let (mut force, mut drop_skipped) = (false, false);
            for a in &args[1..] {
                match a.to_str() {
                    Some("--force") => force = true,
                    Some("--drop-skipped") => drop_skipped = true,
                    _ => usage_error(),
                }
            }
            apply(&Env::load(false), force, drop_skipped)
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
