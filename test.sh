#!/usr/bin/env bash
# Works as a normal user (user namespace) and as root: `./test.sh` and `sudo ./test.sh`.
set -euo pipefail
OVENV=$(realpath "$(dirname "$0")/ovenv")
T=$(mktemp -d "$HOME/.ovenv-test.XXXXXX")
trap 'chmod -R u+rwx "$T"; rm -rf "$T"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

mkdir -p "$T"/{proj/.ovenv,sys/bin,sys/d,out,share}
cd "$T/proj"
printf '%s\n' "$T/sys" "rw $T/share" >.ovenv/paths
echo old >"$T/sys/conf"; echo x >"$T/sys/gone"; echo x >"$T/sys/touched"
echo x >"$T/sys/mode"; chmod 644 "$T/sys/mode"; echo x >"$T/sys/d/old"

"$OVENV" run sh -c "
  echo new > $T/sys/conf; echo hi > $T/sys/bin/tool; rm $T/sys/gone
  touch $T/sys/touched; chmod 600 $T/sys/mode
  rm -rf $T/sys/d; mkdir $T/sys/d; echo n > $T/sys/d/new
  touch $T/out/x 2>/dev/null && echo leak > $T/proj/leak
  echo b > $T/proj/built; echo s > $T/share/s; id -u > $T/proj/uid
"

[[ $(cat "$T/sys/conf") == old && ! -e $T/sys/bin/tool && -e $T/sys/gone && -e $T/sys/d/old ]] || fail "host changed"
[[ ! -e $T/out/x && ! -e $T/proj/leak ]] || fail "write outside overlay reached the host"
[[ -e $T/proj/built && -e $T/share/s ]] || fail "rw paths not writable"
[[ $(cat "$T/proj/uid") == "$(id -u)" ]] || fail "uid changed inside"

out=$("$OVENV" diff)
echo "$out"
for want in "ADD     $T/sys/bin/tool" "MODIFY  $T/sys/conf" "DELETE  $T/sys/gone" \
            "ATTR    $T/sys/mode" "REPLACE $T/sys/d" "ADD     $T/sys/d/new"; do
  grep -qxF "$want" <<<"$out" || fail "missing: $want"
done
! grep -q touched <<<"$out" || fail "touch-only file reported"

"$OVENV" run sleep 2 & sleep 1
! "$OVENV" discard 2>/dev/null || fail "discard ran during a session"
wait

sleep 1; chmod 644 "$T/sys/conf"
! "$OVENV" apply 2>/dev/null || fail "apply ignored a host-side change"
"$OVENV" apply --force

[[ $(cat "$T/sys/conf") == new && $(cat "$T/sys/bin/tool") == hi && ! -e $T/sys/gone ]] || fail "apply content"
[[ $(stat -c %a "$T/sys/mode") == 600 && -e $T/sys/d/new && ! -e $T/sys/d/old ]] || fail "apply attr/replace"
[[ ! -d $T/proj/.ovenv/upper && -z $("$OVENV" diff) ]] || fail "not discarded after apply"
echo OK
