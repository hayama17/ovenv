#!/usr/bin/env bash
# Works as a normal user (user namespace) and as root: `./test.sh` and `sudo ./test.sh`.
set -euo pipefail
OVENV=$(realpath "${OVENV_BIN:-$(dirname "$0")/target/release/ovenv}")
T=$(mktemp -d "$HOME/.ovenv-test.XXXXXX")
cleanup() {
  cat "$T"/*/.ovenv/state "$T"/*/*/.ovenv/state 2>/dev/null | while read -r st; do chmod -R u+rwx "$st"; rm -rf "$st"; done || true
  chmod -R u+rwx "$T"; rm -rf "$T"
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
ov() { "$OVENV" "$@"; }
S=$T/sys

mkdir -p "$T"/{proj/.ovenv,proj2/.ovenv,skiponly/.ovenv,sys2,sys3,out,share} "$S"/{bin,d,pd,deldir,d2f}
printf '%s\n' "$S" "rw $T/share" "rw ." >"$T/proj/.ovenv/paths"
echo "$T/sys2" >"$T/proj2/.ovenv/paths"
cd "$S"
echo old >conf; echo old >conf2; echo old >hostgone; echo x >gone
echo x >touched; touch -d 2020-01-01 touched
echo keep >mode; chmod 644 mode
echo x >d/old; echo x >pd/f; chmod 755 pd; echo x >deldir/f; echo x >d2f/f; echo x >f2d
printf '\0\1old' >bin.dat; ln -s a link
echo old >xkeep; echo x >xattr-mode; chmod 644 xattr-mode
setfattr -n user.ovenv_keep -v 1 xkeep; setfattr -n user.ovenv_keep -v 1 xattr-mode
cd "$T/proj"

chmod 755 "$S"
ov run sh -c "
  set -e; cd $S
  chmod 750 $S
  echo new > conf; echo new > conf2; echo staged > hostgone; rm gone
  echo hi > bin/tool; touch touched; chmod 600 mode
  rm -rf d; mkdir d; echo n > d/new
  mkdir emptydir; chmod 700 pd; rm -rf deldir
  rm -rf d2f; echo file > d2f; rm f2d; mkdir f2d; echo in > f2d/in
  printf '\0\1new' > bin.dat; ln -sfn b link
  echo x > xa; setfattr -n user.ovenv_test -v 1 xa
  echo new > xkeep; chmod 600 xattr-mode
  touch $T/out/x 2>/dev/null && echo leak > $T/proj/leak || true
  echo b > $T/proj/built; echo s > $T/share/s; id -u > $T/proj/uid
  touch $T/proj/.ovenv/x 2>/dev/null && echo leak > $T/proj/ovenv-writable || true
  touch \"\$(cat $T/proj/.ovenv/state)/x\" 2>/dev/null && echo leak > $T/proj/state-writable || true
"
ST=$(cat "$T/proj/.ovenv/state")
U=$ST/upper$S
[[ $ST == /tmp/ovenv-* && $(stat -c %a:%u "$ST") == "700:$(id -u)" ]] || fail "state dir: $ST $(stat -c %a:%u "$ST")"
[[ ! -e $T/proj/ovenv-writable && ! -e $T/proj/state-writable ]] || fail ".ovenv or the state dir was writable inside"

[[ $(cat "$S/conf") == old && ! -e $S/bin/tool && -e $S/gone && -e $S/d/old ]] || fail "host changed"
[[ ! -e $S/emptydir && -f $S/f2d && -d $S/d2f && $(stat -c %a "$S/pd") == 755 ]] || fail "host changed"
[[ ! -e $T/out/x && ! -e $T/proj/leak ]] || fail "write outside overlay reached the host"
[[ -e $T/proj/built && -e $T/share/s ]] || fail "rw paths not writable"
# 'rw .' means the project, also when ovenv runs from a subdirectory.
mkdir -p "$T/proj/sub"
(cd "$T/proj/sub" && ov run sh -c "echo s > $T/proj/fromsub")
[[ -e $T/proj/fromsub ]] || fail "'rw .' resolved against the current directory"
[[ $(cat "$T/proj/uid") == "$(id -u)" ]] || fail "uid changed inside"

out=$(ov diff)
echo "$out"
for want in "ADD     $S/bin/tool" "MODIFY  $S/conf" "DELETE  $S/gone" "ATTR    $S/mode" \
            "REPLACE $S/d/" "ADD     $S/d/new" "ADD     $S/emptydir/" "ATTR    $S/pd/" \
            "DELETE  $S/deldir/" "REPLACE $S/d2f" "REPLACE $S/f2d/" "ADD     $S/f2d/in" \
            "MODIFY  $S/link" "MODIFY  $S/bin.dat" "SKIP    $S/xa  (xattrs, not applied)" \
            "MODIFY  $S/xkeep" "ATTR    $S/xattr-mode" "ATTR    $S/"; do
  grep -qxF -- "$want" <<<"$out" || fail "missing: $want"
done
! grep -q touched <<<"$out" || fail "touch-only file reported"

out=$(ov diff --content)
for want in "--- host:$S/conf" "+++ staged:$S/conf" "-old" "+new" \
            "Binary files host:$S/bin.dat and staged:$S/bin.dat differ" \
            "-symlink -> a" "+symlink -> b"; do
  grep -qxF -- "$want" <<<"$out" || fail "diff --content missing: $want"
done
! grep -q /upper/ <<<"$out" || fail "diff --content exposes upper paths"

err=$(ov apply --force 2>&1) && fail "apply went ahead with SKIP entries"
grep -qxF "  $S/xa (xattrs)" <<<"$err" || fail "SKIP entries not listed: $err"
[[ $(cat "$S/conf") == old && -f $U/xa ]] || fail "refused apply changed the host or dropped staged data"

ov run sleep 2 & sleep 0.5
! ov run true 2>/dev/null || fail "second run allowed"
! ov shell </dev/null 2>/dev/null || fail "shell allowed during a run"
! ov apply 2>/dev/null || fail "apply allowed during a run"
err=$(ov discard 2>&1) && fail "discard allowed during a run"
grep -q "in use" <<<"$err" || fail "unclear lock error: $err"
(cd "$T/proj2" && ov run true && ov discard) || fail "a different .ovenv was blocked"
# Without 'rw .' the project is read-only like anything else not listed.
(cd "$T/proj2" && { ov run sh -c "touch $T/proj2/x" 2>/dev/null || true; } && ov discard)
[[ ! -e $T/proj2/x ]] || fail "project writable without rw ."
wait

# Same-second content change and a deletion on the host, both after staging.
echo host >"$S/conf2"; rm "$S/hostgone"
err=$(ov apply 2>&1) && fail "apply ignored host-side changes"
grep -qxF "  $S/conf2" <<<"$err" || fail "same-second change not detected: $err"
grep -qxF "  $S/hostgone" <<<"$err" || fail "host-side deletion not detected: $err"
! grep -qxF "  $S/conf" <<<"$err" || fail "unchanged host file reported as conflict"
[[ $(cat "$S/conf2") == host ]] || fail "refused apply changed the host"

ino_conf=$(stat -c %i "$S/conf"); ino_mode=$(stat -c %i "$S/mode"); mt=$(stat -c %Y "$S/touched")
if [[ $EUID -ne 0 ]]; then
  chmod 000 "$U/conf"
  ! ov apply --force --drop-skipped 2>/dev/null || fail "apply succeeded with an unreadable staged file"
  [[ $(cat "$S/conf") == old && $(stat -c %i "$S/conf") == "$ino_conf" ]] || fail "failed copy touched the host file"
  [[ -z $(find "$S" -name '.ovenv.*') ]] || fail "temp file left after failed copy"
  chmod 644 "$U/conf"
fi
ov apply --force --drop-skipped

[[ $(cat "$S/conf") == new && $(stat -c %i "$S/conf") != "$ino_conf" ]] || fail "conf not replaced by rename"
[[ $(cat "$S/conf2") == new && $(cat "$S/hostgone") == staged && $(cat "$S/bin/tool") == hi && ! -e $S/gone ]] || fail "apply content"
[[ $(stat -c %a "$S/mode") == 600 && $(cat "$S/mode") == keep && $(stat -c %i "$S/mode") == "$ino_mode" ]] || fail "ATTR rewrote the file"
[[ -e $S/d/new && ! -e $S/d/old && -d $S/emptydir && ! -e $S/deldir ]] || fail "apply dirs"
[[ $(stat -c %a "$S/pd") == 700 && -e $S/pd/f ]] || fail "dir ATTR"
[[ $(stat -c %a "$S") == 750 ]] || fail "ATTR on the overlay root not applied"
[[ -f $S/d2f && $(cat "$S/d2f") == file && -f $S/f2d/in ]] || fail "type changes"
[[ $(readlink "$S/link") == b && $(tail -c3 "$S/bin.dat") == new ]] || fail "symlink/binary"
[[ $(stat -c %Y "$S/touched") == "$mt" ]] || fail "touch-only file was applied"
[[ ! -e $S/xa ]] || fail "skipped entry was applied"
[[ $(cat "$S/xkeep") == new && $(getfattr --absolute-names --only-values -n user.ovenv_keep "$S/xkeep") == 1 ]] || fail "MODIFY dropped an unchanged xattr"
[[ $(stat -c %a "$S/xattr-mode") == 600 && $(getfattr --absolute-names --only-values -n user.ovenv_keep "$S/xattr-mode") == 1 ]] || fail "ATTR dropped an unchanged xattr"
[[ -z $(find "$S" -name '.ovenv.*') ]] || fail "temp file left"
[[ ! -e $ST && ! -e $T/proj/.ovenv/state && -f $T/proj/.ovenv/paths && -z $(ov diff) ]] || fail "not discarded after apply"

# A session with nothing but a skipped directory keeps its staged files until --drop-skipped.
cd "$T/skiponly"
echo "$T/sys3" >.ovenv/paths
ov run sh -c "mkdir $T/sys3/sd; setfattr -n user.ovenv_test -v 1 $T/sys3/sd; echo in > $T/sys3/sd/inner"
inner=$(cat .ovenv/state)/upper$T/sys3/sd/inner
! ov apply 2>/dev/null || fail "apply went ahead with only SKIP entries"
[[ -f $inner && ! -e $T/sys3/sd ]] || fail "refused apply dropped staged files or changed the host"
[[ $(ov apply --drop-skipped) == *"applied 0 change(s)" && ! -e $T/sys3/sd && -z $(ov diff) ]] || fail "--drop-skipped"

# A path under another staged path is covered by it, whatever the order, and leaves the parent's attributes alone.
mkdir -p "$T/ovl/.ovenv" "$T/sys5/sub"; chmod 700 "$T/sys5"
printf '%s\n' "$T/sys5/sub" "$T/sys5" >"$T/ovl/.ovenv/paths"
cd "$T/ovl"
ov run sh -c "echo x > $T/sys5/sub/f"
out=$(ov diff)
[[ $(head -n1 <<<"$out") == "ADD     $T/sys5/sub/f" && $(grep -c . <<<"$out") == 2 ]] || fail "overlapping paths: $out"
ov apply >/dev/null
[[ $(stat -c %a "$T/sys5") == 700 && -f $T/sys5/sub/f ]] || fail "overlapping paths changed the parent"

# A staged directory deleted on the host is a conflict, and the staged state survives the refusal.
mkdir -p "$T/rootgone/.ovenv" "$T/sys4"; chmod 755 "$T/sys4"
echo "$T/sys4" >"$T/rootgone/.ovenv/paths"
cd "$T/rootgone"
ov run chmod 700 "$T/sys4"
rmdir "$T/sys4"
err=$(ov apply 2>&1) && fail "apply ignored a deleted staged directory"
grep -qxF "  $T/sys4" <<<"$err" || fail "deleted staged directory not reported: $err"
[[ -d $(cat .ovenv/state) ]] || fail "refused apply dropped the staged state"
ov discard

# An unreadable staged directory stops diff and apply instead of hiding what is inside it (root reads it anyway).
if [[ $EUID -ne 0 ]]; then
  mkdir -p "$T/lockp/.ovenv" "$T/sys6"; echo "$T/sys6" >"$T/lockp/.ovenv/paths"
  cd "$T/lockp"
  ov run sh -c "mkdir $T/sys6/locked; echo important > $T/sys6/locked/f; chmod 000 $T/sys6/locked" 2>/dev/null
  err=$(ov diff 2>&1) && fail "diff ignored an unreadable staged directory"
  grep -qF "cannot read staged directory $T/sys6/locked" <<<"$err" || fail "unclear unreadable error: $err"
  ! ov apply 2>/dev/null || fail "apply ignored an unreadable staged directory"
  [[ ! -e $T/sys6/locked ]] || fail "refused apply changed the host"
  ov run chmod 700 "$T/sys6/locked"
  grep -qxF "ADD     $T/sys6/locked/f" <<<"$(ov diff)" || fail "file in a formerly unreadable dir not listed"
  ov run chmod 000 "$T/sys6/locked" 2>/dev/null
  ov discard || fail "discard failed on an unreadable staged directory"
  [[ ! -e .ovenv/state ]] || fail "discard left the state behind"

  # A dir that lists but can't be entered hides its files just the same.
  ov run sh -c "mkdir $T/sys6/rd; echo important > $T/sys6/rd/f; chmod 444 $T/sys6/rd" 2>/dev/null
  err=$(ov diff 2>&1) && fail "diff ignored a non-traversable staged directory"
  grep -qF "cannot read staged directory $T/sys6/rd" <<<"$err" || fail "unclear error for mode 444: $err"
  ov discard

  # An unreadable staged file stops apply before anything reaches the host.
  echo old >"$T/sys6/a"
  ov run sh -c "echo new > $T/sys6/a; echo secret > $T/sys6/z; chmod 000 $T/sys6/z"
  err=$(ov apply 2>&1) && fail "apply went ahead with an unreadable staged file"
  grep -qF "cannot read staged $T/sys6/z" <<<"$err" || fail "unclear unreadable file error: $err"
  [[ $(cat "$T/sys6/a") == old && ! -e $T/sys6/z ]] || fail "apply changed the host before failing"
  ov discard
fi

# A staged path inside the project is staged, although the rest of the project writes through.
mkdir -p "$T/pc/.ovenv" "$T/pc/sys"; echo old >"$T/pc/sys/f"
printf '%s\n' "$T/pc/sys" "rw ." >"$T/pc/.ovenv/paths"
cd "$T/pc"
ov run sh -c "echo new > $T/pc/sys/f; echo b > $T/pc/built"
[[ $(cat "$T/pc/sys/f") == old && -e $T/pc/built ]] || fail "staged path inside the project wrote through"
grep -qxF "MODIFY  $T/pc/sys/f" <<<"$(ov diff)" || fail "staged path inside the project not listed"
ov discard

# A staged path that doesn't exist yet is staged through its parent; ovenv creates nothing on the host.
mkdir -p "$T/mis/.ovenv" "$T/sysm"
printf '%s\n' "$T/sysm/new" "rw ." "rw $T/sysm/norw" >"$T/mis/.ovenv/paths"
cd "$T/mis"
err=$(ov run sh -c "mkdir -p $T/sysm/new/x; echo y > $T/sysm/new/x/f" 2>&1)
grep -qF "$T/sysm/new doesn't exist; staging $T/sysm instead" <<<"$err" || fail "no notice for a missing staged path: $err"
grep -qF "rw path $T/sysm/norw doesn't exist" <<<"$err" || fail "no warning for a missing rw path: $err"
[[ ! -e $T/sysm/new && ! -e $T/sysm/norw ]] || fail "run created a path on the host"
mkdir "$T/sysm/new"  # the host creates the path mid-session; what is staged must stay visible
grep -qxF "ADD     $T/sysm/new/x/f" <<<"$(ov diff)" || fail "staged changes hidden after the host created the path"
err=$(ov run true 2>&1)
! grep -q "changed since staging started" <<<"$err" || fail "warned about .ovenv/paths although only the host changed"
echo "$T/sysm/other" >>.ovenv/paths
err=$(ov run true 2>&1)
grep -q "changed since staging started" <<<"$err" || fail "no warning after editing .ovenv/paths: $err"
ov discard
[[ -d $T/sysm/new && ! -e $T/sysm/new/x ]] || fail "discard after a missing staged path"

# A process left running by a session blocks run, apply and discard until it exits.
mkdir -p "$T/bg/.ovenv" "$T/sysb"; echo "$T/sysb" >"$T/bg/.ovenv/paths"
cd "$T/bg"
ov run sh -c "echo x > $T/sysb/f; sleep 3 >/dev/null 2>&1 &"
err=$(ov apply 2>&1) && fail "apply ran while a session process was alive"
grep -q "still running" <<<"$err" || fail "unclear leftover-process error: $err"
! ov discard 2>/dev/null || fail "discard ran while a session process was alive"
! ov run true 2>/dev/null || fail "run started while a session process was alive"
for _ in $(seq 20); do ov apply >/dev/null 2>&1 && break; sleep 0.5; done
[[ $(cat "$T/sysb/f") == x ]] || fail "apply did not go through after the process exited"

# Staging ~: dotfiles are staged, a project under it is staged too, and rw still writes through.
H=$T/home
mkdir -p "$H/proj/.ovenv" "$H/direct"; echo orig >"$H/.bashrc"; chmod 700 "$H"
printf '%s\n' '~' "rw $H/direct" >"$H/proj/.ovenv/paths"
cd "$H/proj"
export HOME=$H
ov run sh -c "echo added >> ~/.bashrc; echo b > $H/proj/built; echo d > $H/direct/d; stat -c %a ~ > $H/direct/mode"
[[ $(cat "$H/direct/mode") == 700 ]] || fail "staged ~ looked like mode $(cat "$H/direct/mode") inside"
[[ $(cat "$H/.bashrc") == orig && ! -e $H/proj/built && -e $H/direct/d ]] || fail "home staging"
out=$(ov diff)
grep -qxF "MODIFY  $H/.bashrc" <<<"$out" || fail "dotfile not staged: $out"
grep -qxF "ADD     $H/proj/built" <<<"$out" || fail "project under ~ not staged: $out"
! grep -q direct <<<"$out" || fail "rw path was staged"
! grep -qF "ATTR    $H/" <<<"$out" || fail "mounting the overlay reported a root change"

chmod 755 "$(cat .ovenv/state)"
err=$(ov diff 2>&1) && fail "used a state dir with mode 755"
grep -q "refusing to use" <<<"$err" || fail "unclear state check error: $err"
chmod 700 "$(cat .ovenv/state)"

st=$(cat .ovenv/state); chmod -R u+rwx "$st"; rm -rf "$st"
for c in diff apply "run true"; do
  # shellcheck disable=SC2086
  err=$(ov $c 2>&1) && fail "$c ignored a missing state dir"
  grep -q "are gone" <<<"$err" || fail "$c: unclear missing-state error: $err"
done
ov discard
[[ ! -e .ovenv/state && -f .ovenv/paths && -z $(ov diff) ]] || fail "discard after a missing state dir"
echo OK
