# Phase 0 wasm spike (embedded ATOMICS design §9)

Standalone crate, outside the ATOMICS workspace. Answers: bundle size per renderer, `snow` on wasm32, and the
iOS soft-keyboard question (egui text agent vs hidden `<textarea>`).

**Status:** all four variants build for wasm32 (rustc 1.99.0, eframe 0.37.0, wasm-bindgen 0.2.130, binaryen 133, snow 0.10.0).
Nothing has been run in a browser yet: the page and the `snow` handshake are unverified at runtime.

## Build (toolchain 1.99.0, `-j 2`)

Needs the wasm32 target, `cargo install wasm-bindgen-cli --version 0.2.130 --locked` (same version as the
`wasm-bindgen` in `Cargo.lock`), and optionally `wasm-opt` (binaryen) and `brotli`. `.cargo/config.toml` sets the
`getrandom_backend="wasm_js"` cfg that `snow` needs.

```
./build.sh wgpu          # eframe wgpu: WebGPU, WebGL2 fallback in one bundle
./build.sh glow          # eframe glow: WebGL2 only
./build.sh wgpu snow     # + Noise_XX handshake, to get the snow size delta (also: ./build.sh glow snow)
```

Each run prints `raw/gzip/brotli` bytes of the `.wasm` and copies the output to `www/pkg/`.
The page shows the `snow` handshake result in the egui window when built with `snow`.

## Results

`.wasm` size in bytes, `opt-level = "s"`, LTO, `panic = "abort"`. `wasm-opt -Oz --all-features` is applied after
wasm-bindgen. Brotli was not available in the VM (not measured).

| build | raw | raw + wasm-opt | gzip -9 | gzip -9 + wasm-opt |
|---|---|---|---|---|
| wgpu (WebGPU + WebGL2) | 5,423,029 | 4,975,276 | 2,035,906 | 2,039,606 |
| glow (WebGL2) | 3,340,268 | 3,063,804 | 1,284,384 | 1,281,741 |
| wgpu + snow | 5,481,187 | 5,029,889 | 2,056,573 | 2,062,436 |
| glow + snow | 3,398,461 | 3,118,417 | 1,305,483 | 1,304,465 |

- **snow delta** (Noise_XX, X25519, ChaChaPoly, BLAKE2s, RustCrypto, no `ring`): +58 KB raw (+1.1% on wgpu, +1.7% on glow), about +20 KB gzip.
- **wgpu vs glow:** glow is 38% smaller raw and 37% smaller gzipped (about 750 KB less over the wire).
- **wasm-opt** saves 8-9% raw but nothing once gzipped (gzip +/- 0.2%), so it matters little for transfer.

## iPhone test

Serve on the desktop (or the sandbox host): `PORT=8080 ./serve.sh`, then open `http://<tailnet-ip>:8080/` in
Safari on the iPhone. Over plain http `navigator.gpu` is absent (not a secure context), so the page uses
WebGL2 even in the wgpu build. For WebGPU use `tailscale serve --https=443 localhost:8080` and open the
`https://<host>.<tailnet>.ts.net/` URL (iOS 26+ for WebGPU).

Build `./build.sh glow` or `wgpu` first. The page has two modes (top links). The debug log at the bottom
records every event, with the user agent on its first line. In each mode try the same steps and note the result:

1. **Tap the black terminal box.** Does the keyboard come up? Does the page jump or zoom?
2. **Type** `hello world` on the soft keyboard. Is each letter received once? Is the first letter capitalised
   (it should not be), and are there autocorrect or smart-punctuation changes (`"`, `--`)?
3. **Backspace** twice, then **Return**.
4. **Predictive text:** tap a suggestion in the bar above the keyboard. Does the word arrive once, with no doubled space?
5. **IME:** switch to the Japanese (Kana) or Chinese (Pinyin) keyboard, type `nihon` / `ni`, watch the
   underlined composition, then pick a candidate. The log should show preedit then one commit.
6. **Dictation:** tap the mic key and say a sentence. Does the text arrive, and in what events?
7. **Hold a key** for the accented-letter popup (hold `e`, pick `é`).
8. **Paste** (long-press the box if it offers it, or paste from the keyboard's clipboard bar).
9. **Dismiss and reopen** the keyboard (tap elsewhere, then the box). Rotate the phone with the keyboard up.
10. In the egui mode, also try the **TextEdit baseline** tab (a stock egui text field) and compare with the terminal widget.

Send back: the iOS version, the mode that worked better, and anything from steps 1-10 that failed. The go/no-go
(egui text agent vs `<textarea>`) is pending that result.
