# ovenv

Run a messy command, see exactly what it changed on disk, then apply or discard it.

```console
$ sudo ovenv run make install
$ sudo ovenv diff
ADD     /usr/local/bin/foo
ADD     /usr/local/lib/libfoo.so
ADD     /usr/local/share/man/man1/foo.1

2.1M staged
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

## Usage

```
ovenv init              create .ovenv/ in the current directory
ovenv run <cmd>...      run a command with its writes staged
ovenv shell             start $SHELL with writes staged
ovenv diff [--content]  show staged changes (--content adds diff -u for MODIFY)
ovenv apply [--force]   write staged changes to the host
ovenv discard           throw staged changes away
```

ovenv uses the nearest `.ovenv/` up from the current directory (created on first `run`). Add it to `.gitignore`.

| Where | What |
| --- | --- |
| `.ovenv/` | `paths`, the mode, a lock, and `state` pointing at the state directory |
| `/tmp/ovenv-<random>/` | staged changes (upper and work layers) and the recorded host state; mode 700 |

Keeping staged changes in `/tmp` lets you stage the directory `.ovenv/` lives in, including all of `~`.
They don't survive a reboot: if the state directory is gone, `diff`, `apply` and `run` stop with an error, and `discard` resets.
`apply` and `discard` remove the state directory and keep `.ovenv/paths`.
Only one `run`, `shell`, `apply` or `discard` can use an `.ovenv/` at a time; different `.ovenv/`s run in parallel.

Run pipelines inside a shell, otherwise only the first command is staged:

```sh
ovenv run sh -c 'curl -fsSL https://example.com/install.sh | sh'
```

### Root and user mode

- With `sudo`, the default staged paths are `/usr /opt /etc /var /root`.
- Without it, ovenv uses a user namespace and stages `~/.local ~/.cargo ~/.npm ~/.cache ~/.config`. Commands still run as you.

An `.ovenv/` sticks to the mode it was created in.

### Choosing paths

`.ovenv/paths` holds one path per line:

```
~
rw ~/.ssh
rw ~/.gnupg
```

`~` stages your whole home directory, so an installer appending to `~/.bashrc` shows up in `diff`.

- Lines starting with `rw` are writable straight through to the host, even inside a staged path.
- `/tmp`, `/dev` and `/proc` are always writable.
- The project (the parent of `.ovenv/`) is writable, unless it is under a staged path; then it is staged too.
- `.ovenv/` and the state directory are read-only inside a session.
- Everything else is read-only.

Unix sockets can't be reached through an overlay, so pass through directories that hold them (`rw ~/.gnupg` for gpg-agent).
`/tmp` and `/` can't be staged, since the state directory lives in `/tmp`.

### Diff kinds

| Kind | Meaning |
| --- | --- |
| `ADD` | new file, symlink or directory |
| `MODIFY` | content or symlink target changed |
| `ATTR` | only mode or owner changed (files and directories) |
| `DELETE` | removed |
| `REPLACE` | a directory was recreated, or a file and a directory swapped places |
| `SKIP` | a change ovenv doesn't apply: xattrs (incl. ACLs and file capabilities), device/FIFO/socket files, symlink ownership |

Directories end with `/`. Only listed changes are applied, and `SKIP` entries never are.
Files that were only touched (mtime) are neither listed nor applied. Hard links are applied as separate files.
Paths are shown after symlinks are resolved, so they point at what actually changes on disk
(on Arch, `make install` into `/usr/local/share/man` shows up under `/usr/local/man`).

`diff --content` compares against the host as it is now, not as it was when staging started.
Binary files are reported as differing without printing their contents; symlinks show their targets.

### Apply

When a session ends, ovenv records the host state of every staged path: type, mode, owner, and the SHA-256 of files or the target of symlinks.
For directories that are deleted or replaced, it also records a listing of their contents (names, types, modes, owners, sizes, mtimes).
`apply` compares each change against that record and refuses if the host differs, including files deleted on the host. Check them, then use `--force`.

This does not catch everything:

- The record is taken when the session ends, not when a file is first written. Host changes during a session go unnoticed.
- Directory contents are compared by metadata, not content.
- xattrs are not recorded.

Regular files and symlinks are written to a temp file in the same directory and renamed into place,
so an existing path never shows a half-written file or goes missing. If preparing the temp file fails, the original stays and the temp file is removed.
xattrs that are the same on the host and in staging are copied onto the new inode, and restored after an owner change (which clears file capabilities).
If they can't be set, apply stops with the original file left in place.
Because the rename creates a new inode, processes that already have the old file open keep reading the old content,
and other hard links to the old file keep the old content.

This is per file only. Apply as a whole is not atomic, nothing is fsynced, and deletions and directory replacements are plain `rm` and `mkdir`.
If apply fails halfway, some changes are already on the host; staged changes are kept, so fix the cause and rerun it (with `--force` if it flags what it already applied).

## Limits

- **Not a sandbox.** Network, processes and IPC are shared with the host. Don't use it to contain malicious code.
- **Only processes started by ovenv see the staged view.** Daemons, systemd services and your editor see the host.
- **Unix sockets can't be reached through an overlay.** That's why `/tmp` is passed through; pass through other socket directories with `rw`.
- **Package managers are not supported.** Applying part of a package database breaks it.
- **Don't change the host under a running session.** OverlayFS doesn't support changes to the lower layer while mounted.
- User mode can't stage paths owned by other users.

## License

MIT
