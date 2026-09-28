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
Writes land in `.ovenv/upper/` instead of the host, so the upper layer *is* the list of changes.
Everything else is mounted read-only, so a write outside the staged paths fails with `EROFS` instead of silently reaching the host.

## Install

```sh
curl -fsSLo ~/.local/bin/ovenv https://raw.githubusercontent.com/hayama17/ovenv/main/ovenv
chmod +x ~/.local/bin/ovenv
```

Requires Linux 5.11+, util-linux 2.39+, GNU tar, and `getfattr` (the `attr` package).

## Usage

```
ovenv init              create .ovenv/ in the current directory
ovenv run <cmd>...      run a command with its writes staged
ovenv shell             start $SHELL with writes staged
ovenv diff              show staged changes
ovenv apply [--force]   write staged changes to the host
ovenv discard           throw staged changes away
```

State lives in the nearest `.ovenv/` up from the current directory (created on first `run`).
Add it to `.gitignore`.

### Root and user mode

- With `sudo`, the default staged paths are `/usr /opt /etc /var /root`.
- Without it, ovenv uses a user namespace and stages `~/.local ~/.cargo ~/.npm ~/.cache ~/.config`. Commands still run as you.

An `.ovenv/` sticks to the mode it was created in.

### Choosing paths

`.ovenv/paths` holds one path per line:

```
/usr/local
~/.cargo
rw ~/.ssh
```

Lines starting with `rw` are writable straight through to the host.
The project directory (the parent of `.ovenv/`), `/tmp`, `/dev` and `/proc` are always writable.
Everything else is read-only.

### Diff kinds

| Kind | Meaning |
| --- | --- |
| `ADD` | new file |
| `MODIFY` | content or symlink target changed |
| `ATTR` | only mode or owner changed |
| `DELETE` | removed |
| `REPLACE` | a directory was recreated, or a file and a directory swapped places |

Files that were only touched are not listed.

### Apply

`apply` refuses to run if a file it would overwrite changed on the host after staging started. Check them, then use `--force`.
Apply is not atomic: if it fails halfway, some changes are already on the host.

## Limits

- **Not a sandbox.** Network, processes and IPC are shared with the host. Don't use it to contain malicious code.
- **Only processes started by ovenv see the staged view.** Daemons, systemd services and your editor see the host.
- **Unix sockets can't be reached through an overlay.** That's why `/tmp` is passed through.
- **Package managers are not supported.** Applying part of a package database breaks it.
- **Don't change the host under a running session.** OverlayFS doesn't support changes to the lower layer while mounted.
- User mode can't stage paths owned by other users.

## License

MIT
