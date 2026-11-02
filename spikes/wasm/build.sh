#!/usr/bin/env bash
# Usage: ./build.sh <wgpu|glow> [snow]   -> www/pkg/ (wasm-bindgen output) and sizes on stdout.
set -euo pipefail
cd "$(dirname "$0")"
renderer=${1:?wgpu or glow}; extra=${2:-}
features=$renderer${extra:+,$extra}
env -u CARGO_MAKEFLAGS cargo build --release --target wasm32-unknown-unknown --no-default-features --features "$features" -j 2
raw=target/wasm32-unknown-unknown/release/atomics_wasm_spike.wasm
out=target/out-$renderer${extra:+-$extra}
rm -rf "$out"
wasm-bindgen --target web --no-typescript --out-dir "$out" "$raw"
command -v wasm-opt >/dev/null && wasm-opt -Oz -o "$out/atomics_wasm_spike_bg.wasm" "$out/atomics_wasm_spike_bg.wasm"
w=$out/atomics_wasm_spike_bg.wasm
echo "$features raw=$(stat -c%s "$w") gzip=$(gzip -9c "$w" | wc -c) brotli=$(brotli -c -q 11 "$w" | wc -c)"
rm -rf www/pkg && cp -r "$out" www/pkg
