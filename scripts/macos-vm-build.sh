#!/usr/bin/env bash
# Builds the ATOMICS VM image and its Mac bundle on a Mac with Docker alone (no Nix), in an
# aarch64-linux Nix container (docs/nixos-vm.md, "Build on a Mac with Docker").
#
#   scripts/macos-vm-build.sh [--out DIR] [--instance NAME] [--mac-user NAME]
#                             [--cpus N] [--memory-gib N] [--disk-gib N]
#
# - The Nix store is a named Docker volume (ATOMICS_NIX_VOLUME, default atomics-nix-store), so a
#   second run rebuilds only what changed and a run with nothing changed copies nothing.
# - The checkout is mounted read-only. Only git's files (tracked, and untracked ones git does not
#   ignore) are copied into the container, so target/ and other build output never reach the store.
# - The image step runs systemd-repart under `unshare --map-root-user`, which Docker's default
#   seccomp profile refuses. The container runs with scripts/macos-vm-build.seccomp.json instead:
#   Docker's refusals except creating a user namespace. The build stays unprivileged (Nix's build
#   users, no added capabilities), and the image is the same derivation any Linux builder makes.
#   Building as the container's root without that `unshare` would need a patched image builder,
#   whose output no other builder shares.
# - It runs through `atomics queue --class test` when ATOMICS is on PATH, so it waits for a slot.
#
# It prints the image and bundle paths. DIR (default target/atomics-vm) gets atomics-vm.raw (a
# sparse disk), bundle/ (the Mac side, no /nix/store reference) and build.json.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "${BASH_SOURCE[0]}"; exit "${1:-0}"; }

out=$repo/target/atomics-vm
instance=atomics
mac_user=$(id -un)
cpus=4
memory=8
disk=100
while [ "$#" -gt 0 ]; do
  case $1 in
    --out) out=$2; shift 2 ;;
    --instance) instance=$2; shift 2 ;;
    --mac-user) mac_user=$2; shift 2 ;;
    --cpus) cpus=$2; shift 2 ;;
    --memory-gib) memory=$2; shift 2 ;;
    --disk-gib) disk=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "macos-vm-build: unknown argument $1" >&2; usage 64 ;;
  esac
done
[[ $instance =~ ^[A-Za-z0-9._-]+$ ]] || { echo "macos-vm-build: bad instance name $instance" >&2; exit 64; }
[[ $mac_user =~ ^[A-Za-z0-9._@+-]+$ ]] || { echo "macos-vm-build: bad Mac user $mac_user" >&2; exit 64; }
for n in "$cpus" "$memory" "$disk"; do
  [[ $n =~ ^[1-9][0-9]*$ ]] || { echo "macos-vm-build: sizes are positive whole numbers" >&2; exit 64; }
done

# Wait for a slot first, as other heavy builds on a shared machine do.
if [ -z "${ATOMICS_VM_BUILD_QUEUED-}" ] && command -v atomics >/dev/null 2>&1; then
  export ATOMICS_VM_BUILD_QUEUED=1
  exec atomics queue --class test -- "${BASH_SOURCE[0]}" --out "$out" --instance "$instance" \
    --mac-user "$mac_user" --cpus "$cpus" --memory-gib "$memory" --disk-gib "$disk"
fi

command -v docker >/dev/null || { echo "macos-vm-build: needs Docker" >&2; exit 69; }
volume=${ATOMICS_NIX_VOLUME:-atomics-nix-store}
# nixos/nix 2.35.4, arm64 and amd64, pinned by digest. An empty volume starts from its /nix.
image=${ATOMICS_NIX_IMAGE:-nixos/nix@sha256:f159a58f390677f41e8e900a74b6e2f9c7f27b364122ae6acf7a652ce6c551c0}
settings=$(printf '{"macUser":"%s","cpus":%s,"memoryGiB":%s,"diskGiB":%s,"instance":"%s"}' \
  "$mac_user" "$cpus" "$memory" "$disk" "$instance")

mkdir -p -- "$out"
out=$(cd -- "$out" && pwd -P)
# The files a git flake of this checkout would see, plus new ones git does not ignore.
(cd -- "$repo" && git ls-files -z --cached --others --exclude-standard) |
  while IFS= read -r -d '' file; do
    [ -e "$repo/$file" ] && printf '%s\0' "$file"
  done > "$out/.files"

started=$(date +%s)
echo "Building the VM for $mac_user (Lima instance $instance, ${cpus} CPUs, ${memory} GiB, ${disk} GiB disk) in Docker; store volume $volume" >&2
docker run --rm -i --name "atomics-vm-build-$volume" \
  --platform linux/arm64 \
  --security-opt "seccomp=$repo/scripts/macos-vm-build.seccomp.json" \
  --mount "type=volume,src=$volume,dst=/nix" \
  --mount "type=bind,src=$repo,dst=/src,readonly" \
  --mount "type=bind,src=$out,dst=/out" \
  -e ATOMICS_VM_SETTINGS="$settings" \
  "$image" bash -euo pipefail -c '
    mkdir -p /work/src
    tar -C /src --null -T /out/.files -cf - | tar -C /work/src -xf -
    paths=$(nix --extra-experimental-features "nix-command flakes" build --impure --no-link \
      --print-out-paths --print-build-logs --max-jobs auto \
      -f /work/src/nix/vm/mac-build.nix --argstr settings "$ATOMICS_VM_SETTINGS" image bundle)
    image= bundle=
    for path in $paths; do
      if [ -f "$path/install.sh" ]; then bundle=$path; else image=$path; fi
    done
    raw=$(find "$image" -maxdepth 1 -name "*.raw" | head -n 1)
    [ -n "$bundle" ] && [ -n "$raw" ] || { echo "macos-vm-build: no image or bundle in: $paths" >&2; exit 1; }
    if grep -rIl /nix/store "$bundle"; then
      echo "macos-vm-build: the bundle names /nix/store paths, which do not exist on a Mac" >&2
      exit 1
    fi
    record=$(printf "{\"image\":\"%s\",\"bundle\":\"%s\",\"settings\":%s}" "$image" "$bundle" "$ATOMICS_VM_SETTINGS")
    if [ -f /out/build.json ] && [ "$(cat /out/build.json)" = "$record" ] &&
       [ -f /out/atomics-vm.raw ] && [ -d /out/bundle ]; then
      echo "Unchanged since the last build; nothing copied." >&2
      exit 0
    fi
    rm -rf /out/bundle /out/atomics-vm.raw /out/build.json
    cp --sparse=always "$raw" /out/atomics-vm.raw.new
    mv /out/atomics-vm.raw.new /out/atomics-vm.raw
    # The store copy is read-only, and Docker Desktop applies the Mac permissions to /out: a
    # read-only folder could not be filled there, so the copy is made writable first.
    cp -R "$bundle" /work/bundle
    chmod -R u+w /work/bundle
    cp -R /work/bundle /out/bundle.new
    mv /out/bundle.new /out/bundle
    printf "%s\n" "$record" > /out/build.json
  '
rm -f -- "$out/.files"

# No /nix/store reference may reach the Mac (the container checked the store copy; this is the copy).
[ -f "$out/bundle/install.sh" ] && [ -f "$out/build.json" ] || { echo "macos-vm-build: no bundle in $out" >&2; exit 1; }
if grep -rIl /nix/store "$out/bundle"; then
  echo "macos-vm-build: $out/bundle names /nix/store paths" >&2
  exit 1
fi
elapsed=$(( $(date +%s) - started ))
apparent=$(stat -f %z "$out/atomics-vm.raw" 2>/dev/null || stat -c %s "$out/atomics-vm.raw")
used=$(du -k "$out/atomics-vm.raw" | cut -f1)
printf 'image:  %s (%d GiB disk, %d MiB used)\n' "$out/atomics-vm.raw" $((apparent / 1073741824)) $((used / 1024))
printf 'bundle: %s (%s)\n' "$out/bundle" "$(du -sh "$out/bundle" | cut -f1 | tr -d ' ')"
printf 'time:   %dm%02ds\n' $((elapsed / 60)) $((elapsed % 60))
echo "Next: atomics vm setup --from $out" >&2
