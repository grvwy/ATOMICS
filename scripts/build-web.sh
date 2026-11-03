#!/usr/bin/env bash
# Builds the ATOMICS web client (docs/daemon.md, "The browser client and its embedding"):
#   crates/atomics-web/dist/                      the bundle, with manifest.json
#   target/web/atomics-web-<version>.tar.gz       the bundle packed, and its .sha256 beside it
# and prints the sizes. Needs the wasm32-unknown-unknown target and trunk 0.21; trunk uses the
# wasm-bindgen 0.2.129 on PATH or in its cache, else downloads it. JOBS (default 1) caps cargo's jobs;
# BUILT_FROM (default: this checkout's commit) goes into the manifest.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
crate=$root/crates/atomics-web
dist=$crate/dist
out=$root/target/web
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
built_from=${BUILT_FROM:-$(git -C "$root" rev-parse HEAD)}
export CARGO_BUILD_JOBS=${JOBS:-1}

rm -rf "$dist"
(cd "$crate" && trunk build --release --cargo-profile web --locked)
cargo run --quiet --locked -p atomics-web -- manifest "$dist" "$built_from" >/dev/null

mkdir -p "$out"
archive=$out/atomics-web-$version.tar.gz
tar -C "$dist" --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -cf - . | gzip -9n >"$archive"
(cd "$out" && sha256sum "$(basename "$archive")" >"$(basename "$archive").sha256")

gz() { gzip -9c "$1" | wc -c; }
total_raw=0 total_gz=0
while IFS= read -r -d '' file; do
  total_raw=$((total_raw + $(stat -c%s "$file")))
  total_gz=$((total_gz + $(gz "$file")))
done < <(find "$dist" -type f -print0)
wasm=$(find "$dist" -maxdepth 1 -name '*.wasm' | head -n 1)
echo "wasm:    $(stat -c%s "$wasm") bytes, $(gz "$wasm") gzipped"
echo "bundle:  $total_raw bytes, $total_gz gzipped file by file"
echo "archive: $(stat -c%s "$archive") bytes  $archive"
cat "$archive.sha256"
