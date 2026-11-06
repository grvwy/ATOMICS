#!/usr/bin/env bash
# Boot-time Mac identity and shares. The first argument is a trusted Nix-generated configuration;
# cidata is data, never sourced as shell code. Lima's param.env uses literal KEY=value lines.
set -euo pipefail
. "$1"
valid_path() {
  [[ $1 =~ ^/[A-Za-z0-9._@+/-]+$ && $1 != */ && $1 != *'/../'* && $1 != */.. && $1 != *'/./'* ]]
}
if [ "$dynamic" = true ]; then
  mac_user=$(sed -n 's/^PARAM_ATOMICS_MAC_USER=//p' "$cidata/param.env")
  [[ $mac_user =~ ^[A-Za-z0-9_][A-Za-z0-9._-]*$ && $mac_user != *..* ]] || {
    echo 'Missing or invalid ATOMICS_MAC_USER in Lima cidata' >&2; exit 1;
  }
  mac_home=${mac_home//@ATOMICS_MAC_USER@/$mac_user}
  socket=${socket//@ATOMICS_MAC_USER@/$mac_user}
  executables=${executables//@ATOMICS_MAC_USER@/$mac_user}
fi
# These are the flow-style mount rows emitted by Lima, not arbitrary cloud-init instructions.
# Read only virtiofs rows; Rosetta is mounted by NixOS's own service.
mounts=$(awk '
  /^[[:space:]]*- \[/ {
    line=$0; sub(/^[[:space:]]*- \[/, "", line); sub(/\][[:space:]]*$/, "", line)
    n=split(line, f, /, /)
    for (i=1; i<=n; i++) { gsub(/^"|"$/, "", f[i]) }
    if (n>=4 && f[3]=="virtiofs" && f[1]!="vz-rosetta") {
      mode="rw"; if ("," f[4] "," ~ /,ro,/) mode="ro"
      print f[1] " " f[2] " " mode
    }
  }' "$cidata/user-data")
if [ "$dynamic" = true ]; then
  shares=
  while read -r tag path mode; do
    [ -n "$tag" ] || continue
    valid_path "$path" || { echo 'Invalid cidata mount path' >&2; exit 1; }
    case $path in "$mac_home"/*|/Volumes/*) ;; *) echo 'Unexpected Mac share path' >&2; exit 1;; esac
    shares="$shares $path:$mode"
  done <<< "$mounts"
  [ -n "$shares" ] || { echo 'Lima supplies no Mac shares' >&2; exit 1; }
fi
# Validate all inputs before changing anything. Never place executable links inside a Mac share.
for path in "$mac_home" "$socket" $executables; do
  valid_path "$path" || { echo 'Invalid Mac path' >&2; exit 1; }
done
for share in $shares; do
  path=${share%:*}
  for exe in "$socket" $executables; do
    case $exe in "$path"|"$path"/*) echo 'Executable or socket is inside a share' >&2; exit 1;; esac
  done
done
install -d -m 0755 -o "$owner" -g "$group" "$mac_home"
install -d -m 0700 -o "$owner" -g "$group" "${socket%/*}"
for exe in $executables; do
  mkdir -p "${exe%/*}"
  ln -sfn "$binary" "$exe"
done
host_folder=$guest_home
for share in $shares; do
  path=${share%:*} mode=${share##*:}
  tag=$(awk -v want="$path" '$2==want {print $1}' <<< "$mounts")
  [[ $tag =~ ^[A-Za-z0-9._-]+$ ]] || { echo "Lima does not share $path" >&2; exit 1; }
  if ! mountpoint -q "$path"; then
    if [ ! -d "$path" ]; then
      install -d -m 0755 -o "$owner" -g "$group" "$path"
    fi
    mount -t virtiofs -o "$mode,nosuid,nodev" "$tag" "$path"
  fi
  if [ "$mode" = rw ] && [ "$host_folder" = "$guest_home" ]; then host_folder=$path; fi
done
mkdir -p "${output%/*}"
# All paths above use the restricted alphabet: safe for both shell sourcing and word lists.
printf 'ATOMICS_MAC_SOCKET=%s\nATOMICS_MAC_SHARES="%s"\nATOMICS_MAC_EXECUTABLES="%s"\nATOMICS_HOST_PORT=%s\nATOMICS_HOST_FOLDER=%s\n' \
  "$socket" "$shares" "$executables" "$host_port" "$host_folder" > "$output"
