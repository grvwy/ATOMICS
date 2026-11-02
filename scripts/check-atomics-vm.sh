#!/usr/bin/env bash
# Run in the guest, with the host's ATOMICS QA queue holding the slot (see docs/nixos-vm.md).
set -euo pipefail

test "$(uname -m)" = aarch64
test "$(findmnt -n -o FSTYPE /)" = btrfs
grep -q 'flags: .*F' /proc/sys/fs/binfmt_misc/rosetta

for tool in node git gh claude codex cargo rustc rustup docker atomics-onboard jq; do
  command -v "$tool"
done

# sshd takes keys only and no root login.
sshd_config=$(sudo sshd -T)
for setting in 'passwordauthentication no' 'kbdinteractiveauthentication no' 'permitrootlogin no'; do
  grep -qix "$setting" <<<"$sshd_config"
done
printf 'sshd: keys only, no root login\n'

# ATOMICS on the Mac (programs.atomics.macDesktop): shares at the Mac's paths, its binary path
# runs this machine's atomics, and sshd replaces a stale forwarded socket.
if [ -r /etc/atomics/mac-desktop.env ]; then
  # shellcheck source=/dev/null
  . /etc/atomics/mac-desktop.env
  systemctl is-active --quiet atomics-mac-shares
  for share in $ATOMICS_MAC_SHARES; do
    path=${share%:*}
    test "$(findmnt -n -o FSTYPE -M "$path")" = virtiofs
    findmnt -n -o OPTIONS -M "$path" | tr , '\n' | grep -qx "${share##*:}"
    if [ "${share##*:}" = rw ]; then
      probe=$(mktemp "$path/.atomics-vm-check.XXXXXX")
      rm -f -- "$probe"
    fi
    printf 'share %s: virtiofs, %s\n' "$path" "${share##*:}"
  done
  for exe in $ATOMICS_MAC_EXECUTABLES; do
    test "$(readlink -f "$exe")" = "$(readlink -f "$(command -v atomics)")"
  done
  test "$ATOMICS_SOCKET" = "$ATOMICS_MAC_SOCKET"
  test "$(stat -c '%U %a' "${ATOMICS_MAC_SOCKET%/*}")" = "$(id -un) 700"
  grep -qix 'streamlocalbindunlink yes' <<<"$sshd_config"
  command -v atomics-vm-exec
  if [ -S "$ATOMICS_MAC_SOCKET" ]; then
    atomics ctl '{"cmd":"list"}' >/dev/null
    printf 'forwarded socket: ATOMICS on the Mac answers\n'
  else
    printf 'forwarded socket: off (the default; docs/nixos-vm.md, The socket forward)\n'
  fi
fi

scratch=$(mktemp -d "$HOME/repos/.atomics-vm-check.XXXXXX")
cleanup() { rm -rf -- "$scratch"; }
trap cleanup EXIT
head -c 1048576 /dev/urandom > "$scratch/source"
cp --reflink=always "$scratch/source" "$scratch/clone"
cmp "$scratch/source" "$scratch/clone"
printf 'changed\n' > "$scratch/source"
test "$(wc -c < "$scratch/clone")" -eq 1048576
printf 'btrfs reflink: passed\n'

# An amd64 container runs under Rosetta (binfmt's fix-binary flag reaches into containers). Without
# a translator its shell fails with "exec format error".
test "$(docker run --rm --platform linux/amd64 docker.io/library/alpine:3.22 sh -c 'echo ran')" = ran
printf 'amd64 container under Rosetta: passed\n'

# Checks a stack adds (atomics.vm.checks), in name order.
for check in /etc/atomics/check.d/*.sh; do
  [ -f "$check" ] || continue
  printf '\n── %s\n' "${check##*/}"
  bash "$check"
done

printf 'ATOMICS VM checks passed\n'
