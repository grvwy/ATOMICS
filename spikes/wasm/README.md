# Phase 0 wasm spike (RELAY-11973, design §9)

Standalone crate, outside the ATOMICS workspace. Answers: bundle size per renderer, `snow` on wasm32, and the
iOS soft-keyboard question (egui text agent vs hidden `<textarea>`).

**Status: written, not yet built or measured.** Builds were held until the host cooled down. The results table
below is empty and the code has not been compiled, so expect small API fixes on the first build.

## Build (toolchain 1.99.0, `-j 2`)

Needs `rustup target add wasm32-unknown-unknown`, `cargo install wasm-bindgen-cli` (same version as the
`wasm-bindgen` in `Cargo.lock`), and optionally `wasm-opt` (binaryen) and `brotli`.

```
./build.sh wgpu          # eframe wgpu: WebGPU, WebGL2 fallback in one bundle
./build.sh glow          # eframe glow: WebGL2 only
./build.sh wgpu snow     # + Noise_XX handshake, to get the snow size delta (also: ./build.sh glow snow)
```

Each run prints `raw/gzip/brotli` bytes of the `.wasm` and copies the output to `www/pkg/`.
The page shows the `snow` handshake result in the egui window when built with `snow`.

## Results

| build | raw | gzip -9 | brotli -11 |
|---|---|---|---|
| wgpu | | | |
| glow | | | |
| wgpu + snow | | | |
| glow + snow | | | |

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
