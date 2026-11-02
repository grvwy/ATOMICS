#!/usr/bin/env bash
# Usage: ./build.sh <wgpu|glow> [snow]   -> www/pkg/ (wasm-bindgen output) and sizes on stdout.
set -euo pipefail
cd "$(dirname "$0")"
renderer=${1:?wgpu or glow}; extra=${2:-}
features=$renderer${extra:+,$extra}
env -u CARGO_MAKEFLAGS cargo build --release --target wasm32-unknown-unknown --no-default-features --features "$features" -j "${JOBS:-2}"
raw=${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/release/atomics_wasm_spike.wasm
out=target/out-$renderer${extra:+-$extra}
rm -rf "$out"
wasm-bindgen --target web --no-typescript --out-dir "$out" "$raw"
sizes() { # label file
  local b=n/a
  command -v brotli >/dev/null && b=$(brotli -c -q 11 "$2" | wc -c)
  echo "$features $1 raw=$(stat -c%s "$2") gzip=$(gzip -9c "$2" | wc -c) brotli=$b"
}
w=$out/atomics_wasm_spike_bg.wasm
sizes bindgen "$w"
if command -v wasm-opt >/dev/null; then
  wasm-opt -Oz --all-features -o "$out/opt.wasm" "$w"
  sizes wasm-opt "$out/opt.wasm"
  mv "$out/opt.wasm" "$w"   # the served bundle is the optimised one
fi
rm -rf www/pkg && cp -r "$out" www/pkg
