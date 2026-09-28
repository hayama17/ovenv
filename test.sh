#!/usr/bin/env bash
# Works as a normal user (user namespace) and as root: `./test.sh` and `sudo ./test.sh`.
set -euo pipefail
OVENV=$(realpath "$(dirname "$0")/ovenv")
T=$(mktemp -d "$HOME/.ovenv-test.XXXXXX")
trap 'chmod -R u+rwx "$T"; rm -rf "$T"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
ov() { "$OVENV" "$@"; }
S=$T/sys
U=$T/proj/.ovenv/upper$S

mkdir -p "$T"/{proj/.ovenv,proj2/.ovenv,sys2,out,share} "$S"/{bin,d,pd,deldir,d2f}
printf '%s\n' "$S" "rw $T/share" >"$T/proj/.ovenv/paths"
echo "$T/sys2" >"$T/proj2/.ovenv/paths"
cd "$S"
echo old >conf; echo old >conf2; echo old >hostgone; echo x >gone
echo x >touched; touch -d 2020-01-01 touched
echo keep >mode; chmod 644 mode
echo x >d/old; echo x >pd/f; chmod 755 pd; echo x >deldir/f; echo x >d2f/f; echo x >f2d
printf '\0\1old' >bin.dat; ln -s a link
cd "$T/proj"

ov run sh -c "
  set -e; cd $S
  echo new > conf; echo new > conf2; echo staged > hostgone; rm gone
  echo hi > bin/tool; touch touched; chmod 600 mode
  rm -rf d; mkdir d; echo n > d/new
  mkdir emptydir; chmod 700 pd; rm -rf deldir
  rm -rf d2f; echo file > d2f; rm f2d; mkdir f2d; echo in > f2d/in
  printf '\0\1new' > bin.dat; ln -sfn b link
  echo x > xa; setfattr -n user.ovenv_test -v 1 xa
  touch $T/out/x 2>/dev/null && echo leak > $T/proj/leak || true
  echo b > $T/proj/built; echo s > $T/share/s; id -u > $T/proj/uid
"

[[ $(cat "$S/conf") == old && ! -e $S/bin/tool && -e $S/gone && -e $S/d/old ]] || fail "host changed"
[[ ! -e $S/emptydir && -f $S/f2d && -d $S/d2f && $(stat -c %a "$S/pd") == 755 ]] || fail "host changed"
[[ ! -e $T/out/x && ! -e $T/proj/leak ]] || fail "write outside overlay reached the host"
[[ -e $T/proj/built && -e $T/share/s ]] || fail "rw paths not writable"
[[ $(cat "$T/proj/uid") == "$(id -u)" ]] || fail "uid changed inside"

out=$(ov diff)
echo "$out"
for want in "ADD     $S/bin/tool" "MODIFY  $S/conf" "DELETE  $S/gone" "ATTR    $S/mode" \
            "REPLACE $S/d/" "ADD     $S/d/new" "ADD     $S/emptydir/" "ATTR    $S/pd/" \
            "DELETE  $S/deldir/" "REPLACE $S/d2f" "REPLACE $S/f2d/" "ADD     $S/f2d/in" \
            "MODIFY  $S/link" "MODIFY  $S/bin.dat" "SKIP    $S/xa  (xattrs, not applied)"; do
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

ov run sleep 2 & sleep 0.5
! ov run true 2>/dev/null || fail "second run allowed"
! ov shell </dev/null 2>/dev/null || fail "shell allowed during a run"
! ov apply 2>/dev/null || fail "apply allowed during a run"
err=$(ov discard 2>&1) && fail "discard allowed during a run"
grep -q "in use" <<<"$err" || fail "unclear lock error: $err"
(cd "$T/proj2" && ov run true) || fail "a different .ovenv was blocked"
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
  ! ov apply --force 2>/dev/null || fail "apply succeeded with an unreadable staged file"
  [[ $(cat "$S/conf") == old && $(stat -c %i "$S/conf") == "$ino_conf" ]] || fail "failed copy touched the host file"
  [[ -z $(find "$S" -name '.ovenv.*') ]] || fail "temp file left after failed copy"
  chmod 644 "$U/conf"
fi
ov apply --force

[[ $(cat "$S/conf") == new && $(stat -c %i "$S/conf") != "$ino_conf" ]] || fail "conf not replaced by rename"
[[ $(cat "$S/conf2") == new && $(cat "$S/hostgone") == staged && $(cat "$S/bin/tool") == hi && ! -e $S/gone ]] || fail "apply content"
[[ $(stat -c %a "$S/mode") == 600 && $(cat "$S/mode") == keep && $(stat -c %i "$S/mode") == "$ino_mode" ]] || fail "ATTR rewrote the file"
[[ -e $S/d/new && ! -e $S/d/old && -d $S/emptydir && ! -e $S/deldir ]] || fail "apply dirs"
[[ $(stat -c %a "$S/pd") == 700 && -e $S/pd/f ]] || fail "dir ATTR"
[[ -f $S/d2f && $(cat "$S/d2f") == file && -f $S/f2d/in ]] || fail "type changes"
[[ $(readlink "$S/link") == b && $(tail -c3 "$S/bin.dat") == new ]] || fail "symlink/binary"
[[ $(stat -c %Y "$S/touched") == "$mt" ]] || fail "touch-only file was applied"
[[ ! -e $S/xa ]] || fail "skipped entry was applied"
[[ -z $(find "$S" -name '.ovenv.*') ]] || fail "temp file left"
[[ ! -d $T/proj/.ovenv/upper && -z $(ov diff) ]] || fail "not discarded after apply"
echo OK
