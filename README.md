# ovenv

Run a messy command, see exactly what it changed on disk, then apply or discard it.

```console
$ sudo ovenv run make install
$ sudo ovenv diff
ADD     /usr/local/bin/foo
ADD     /usr/local/lib/libfoo.so
ADD     /usr/local/share/man/man1/foo.1

2.1M staged in /var/tmp/ovenv-Xq3vT9kA
$ sudo ovenv apply      # or: sudo ovenv discard
```

ovenv runs the command in a private mount namespace where selected paths are covered by OverlayFS.
Writes land in an upper layer instead of the host, so the upper layer *is* the list of changes.
Everything else is mounted read-only, so a write outside the staged paths fails with `EROFS` instead of silently reaching the host.

## Install

```sh
cargo install --git ssh://git@github.com/hayama17/ovenv.git
sudo install ~/.cargo/bin/ovenv /usr/local/bin/   # sudo's PATH usually skips ~/.cargo/bin
```

Requires Linux 5.12+. No other runtime dependencies.

## Quick start

```sh
cd my-project
ovenv init                     # writes .ovenv/paths; add .ovenv/ to .gitignore
ovenv run sh -c 'curl -fsSL https://example.com/install.sh | sh'
ovenv diff --content           # what it did to ~/.local, ~/.cargo, ...
ovenv apply                    # or: ovenv discard
```

The defaults stage a few directories under `~`. Add `~` to `.ovenv/paths` to catch dotfile edits like `~/.bashrc` too,
and use `sudo` for commands that write system paths (`/usr`, `/etc`, ...).

## Commands

| Command | What it does |
| --- | --- |
| `ovenv init` | Create `.ovenv/` with a `paths` file in the current directory |
| `ovenv run <cmd>...` | Run a command with its writes staged |
| `ovenv shell` | Start `$SHELL` with writes staged |
| `ovenv diff [--content]` | List staged changes; `--content` adds `diff -u` for modified files |
| `ovenv apply [--force] [--drop-skipped]` | Write staged changes to the host |
| `ovenv discard` | Throw staged changes away |

- ovenv uses the nearest `.ovenv/` up from the current directory, and creates one on the first `run` if there is none.
- Several `run`s add to the same staged changes until `apply` or `discard`.
- Only one `run`, `shell`, `apply` or `discard` can use an `.ovenv/` at a time; different `.ovenv/`s run in parallel.
- A process a session left running in the background (a daemon, `cmd &`) keeps the session alive: `run`, `apply` and `discard` refuse, naming its PIDs, until it exits.
- Run pipelines inside a shell, otherwise only the first command is staged: `ovenv run sh -c 'a | b'`.

## Choosing paths

`.ovenv/paths` holds one path per line:

```
~
rw .
rw ~/.ssh
rw ~/.gnupg
```

- A plain line is **staged**: writes go to the upper layer. `~` stages your whole home directory, dotfiles included.
- A line starting with `rw` **writes straight through** to the host, even inside a staged path.
- `rw .` is the project (the parent of `.ovenv/`). Without it, the project is staged if it is under a staged path and read-only otherwise.
- `/tmp`, `/dev` and `/proc` are always writable.
- `.ovenv/` and the state directory are read-only inside a session.
- **Everything else is read-only.**

Relative paths are relative to the project, wherever you run ovenv from.

### Defaults

Without `.ovenv/paths`, and in the file `ovenv init` writes:

- **Root mode** (`sudo`): `/usr /opt /etc /var/lib /var/cache /var/log /var/spool /var/opt /root`, `rw /var/tmp` and `rw .`
- **User mode** (no `sudo`): `~/.local ~/.cargo ~/.npm ~/.cache ~/.config` and `rw .`. ovenv uses a user namespace, and commands still run as you. It can't stage paths owned by other users.

An `.ovenv/` sticks to the mode it was created in.

### Paths that don't exist

ovenv never creates a listed path on the host.
A staged path that doesn't exist yet is staged through its nearest existing parent, so an installer creating `~/.bun` works when only `~/.bun` is listed; `run` says which parent it used.
A missing `rw` path is not created either, and `run` warns about it.

### While changes are staged

The staged paths are fixed when staging starts. Editing `.ovenv/paths` takes effect after `apply` or `discard`, and `run` warns if it changed.

Unix sockets can't be reached through an overlay, so pass through directories that hold them (`rw ~/.gnupg` for gpg-agent).
`/` can't be staged, nor `/var` and `/tmp` together, since the state directory has to live outside every staged path.

## Reading the diff

| Kind | Meaning |
| --- | --- |
| `ADD` | new file, symlink or directory |
| `MODIFY` | content or symlink target changed |
| `ATTR` | only mode or owner changed (files and directories) |
| `DELETE` | removed |
| `REPLACE` | a directory was recreated, or a file and a directory swapped places |
| `SKIP` | a change ovenv can't apply: xattrs (incl. ACLs and file capabilities), device/FIFO/socket files, symlink ownership |

- Directories end with `/`.
- Files that were only touched (mtime) are not listed.
- Paths are shown after symlinks are resolved, so they point at what actually changes on disk (on Arch, `make install` into `/usr/local/share/man` shows up under `/usr/local/man`).
- `diff --content` compares against the host as it is now, not as it was when staging started. Binary files are reported as differing without printing their contents; symlinks show their targets.
- If a staged directory can't be read (a session left it at mode 000 or 444), `diff` and `apply` stop and name it. Make it readable with `ovenv run chmod`, or `discard`.

## How apply works

### What is applied

Exactly the changes `diff` lists, and nothing else. Touch-only files are not applied, and hard links are applied as separate files.

If anything is listed as `SKIP`, apply stops before touching the host, because the staged copy is the only one.
`--drop-skipped` applies the rest and discards the skipped entries.

### Conflicts with the host

When a session ends, ovenv records the host state of every staged path: type, mode, owner, and the SHA-256 of files or the target of symlinks.
For directories that are deleted or replaced, it also records a listing of their contents (names, types, modes, owners, sizes, mtimes).
`apply` compares each change against that record and refuses if the host differs, including files deleted on the host. Check them, then use `--force`.

This does not catch everything:

- The record is taken when the session ends, not when a file is first written. Host changes during a session go unnoticed.
- Directory contents are compared by metadata, not content.
- xattrs are not recorded.
- Entries under a staged directory ovenv couldn't read at session end are recorded by the next `run` after it is made readable, so host changes in between go unnoticed.

### Writes

- Regular files and symlinks are written to a temp file in the same directory and renamed into place, so an existing path never shows a half-written file or goes missing. If preparing the temp file fails, the original stays and the temp file is removed.
- xattrs that are the same on the host and in staging are copied onto the new inode, and restored after an owner change (which clears file capabilities). If they can't be set, apply stops with the original file left in place.
- Because the rename creates a new inode, processes that already have the old file open keep reading the old content, and other hard links to the old file keep the old content.
- Staged files that can't be read stop apply before anything reaches the host.

This is per file only. Apply as a whole is not atomic, nothing is fsynced, and deletions and directory replacements are plain `rm` and `mkdir`.
If apply fails halfway, some changes are already on the host; staged changes are kept, so fix the cause and rerun it (with `--force` if it flags what it already applied).

## Where state lives

| Where | What |
| --- | --- |
| `.ovenv/` | `paths`, the mode, a lock, and `state` pointing at the state directory |
| `/var/tmp/ovenv-<random>/` | staged changes (upper and work layers), the recorded host state and the staged path list; mode 700 |

Keeping staged changes outside the project lets you stage the directory `.ovenv/` lives in, including all of `~`.
`/var/tmp` survives a reboot, but systemd-tmpfiles removes files there that go unused for 30 days on most distros, so apply or discard before then.
If `/var/tmp` is staged, the state goes to `/tmp` instead and is lost on reboot; `run` warns and `diff` says so.
If the state directory is gone, `diff`, `apply` and `run` stop with an error, and `discard` resets.
`apply` and `discard` remove the state directory and keep `.ovenv/paths`.

## Limits

- **Not a sandbox.** Network, processes and IPC are shared with the host. Don't use it to contain malicious code.
- **Only processes started by ovenv see the staged view.** Daemons, systemd services and your editor see the host.
- **Package managers are not supported.** Applying part of a package database breaks it.
- **Don't change the host under a running session.** OverlayFS doesn't support changes to the lower layer while mounted.

## License

MIT
