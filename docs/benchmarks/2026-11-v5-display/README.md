# The built display path, measured again (v5)

2026-11-02. Path C from [the display decision](../../decisions/2026-11-02-vm-display.md), as the flake now
builds it: ATOMICS native on the Mac (Metal), its agent tiles running in the NixOS VM through the wrappers
`programs.atomics.macDesktop` generates ([docs/nixos-vm.md](../../nixos-vm.md)). Same scene and
instrumentation as the spike, against its numbers.

## Results

| | v1 spike (path C, by hand) | v5 (built path) |
| --- | --- | --- |
| Display the window was on | external 2560×1440 at 60 Hz, 1 px/pt, window 2560×1378 px | built-in 3024×1834 px window at 120 Hz (ProMotion), 2 px/pt |
| GPU adapter wgpu picks (others offered) | `Apple M5 Max`, Metal (none) | `Apple M5 Max`, Metal (none) |
| Tiles' programs | Ubuntu Lima VM, hand-written ssh wrapper | NixOS VM, generated `bin/atomics-vm-run` |
| fps, diff scrolling, 8 streaming tiles, vsync | 59.8–60.0 | **119.2–120.0** |
| Frame interval p50 / p95 / p99 | 16.7 / 16.9–17.0 / 17.0–17.2 ms | **8.3 / 8.4–8.5 / 8.5–8.6 ms** |
| Frames over 20 ms / 34 ms, per 5 s | 0–2 / 0 | 0–1 / 0–1 |
| Same without vsync | 199–240 fps, p50 3.7–4.3 ms | 256–334 fps, p50 1.7–3.3 ms |
| CPU per frame p50 / p95, vsync | 1.6–4.5 / 1.8–5.2 ms | 1.2–1.3 / 1.4–1.6 ms |
| Keystroke → echo in a frame, tile in the VM | 5.2 ms p50, 16.4 ms p95 | **7.7 ms p50, 9.2 ms p95** (max 10.9) |
| Same, tile on the Mac | 4.7 ms p50, 12.3 ms p95 | 7.4 ms p50, 9.1 ms p95 (max 9.4) |
| PTY echo round trip through the wrapper, p50 / p90 / p99 | 0.65–0.68 / 0.83–0.85 / 1.0–1.2 ms | **0.51 / 0.61–0.62 / 0.84–0.91 ms** |
| Same, while 8 tiles stream through ssh | 0.59 / 0.77 / 0.96 ms | 0.43 / 0.67 / 2.12 ms |
| Plain ssh with OpenSSH's defaults (keystroke obfuscation on) | 20.4 / 22.3 / 22.8 ms | 19.4 / 28.4 / 32.1 ms |
| Local `cat` | 0.05 / 0.06 / 0.09 ms | 0.04 / 0.05 / 0.08–0.09 ms |

Steady state is the 5-second windows from 15 s to 40 s into each run, as in v1 ([steady.txt](steady.txt)).

**Reading it.** The UI is GPU-accelerated on Metal and holds the display's full refresh: 120 fps at
120 Hz, with more pixels than v1's 60 Hz screen and no frame over 20 ms in most windows. Without vsync
it renders 2–3× faster than 120 Hz. The VM costs a keystroke about 0.3 ms over a local tile, which is
the wrapper's 0.5 ms PTY round trip; at 120 Hz the echo lands within the next frame. The plain-ssh row
is why the wrapper passes `ObscureKeystrokeTiming=no`.

The keystroke p50 is higher than v1's because the frame is half as long but the measurement differs
(see below). The p95 is lower. Neither depends on the VM: local and VM tiles match within 0.3 ms.

## The machine and the scene

- MacBook Pro (Mac17,7), M5 Max, 64 GB, macOS 26.5, OpenSSH 10.2, Lima 2.2.0. Shared with other
  agents; runs went through `atomics queue --class qa`.
- The VM: a test variant of `nixosConfigurations.atomics-vm` (Lima instance `atomics-v5`, 14 vCPU,
  24 GiB instead of 48, the built raw image). It shared only scratch folders and named the QA build of
  ATOMICS as the Mac executable; otherwise the committed guest and the generated Mac bundle.
- ATOMICS from this branch with the spike's measurement patch in a scratch copy
  ([measure/](measure/): `spike.rs`, applied with v1's `patch_spike.py`; not part of the app).
- Scene as in v1: 8 tiles each streaming 200 coloured lines/s ([measure/scroll.pl](measure/scroll.pl)),
  one `cat` tile, and the diff panel open on a 70-file diff of ATOMICS's own sources
  (+11,814 −11,814 lines; v1 had +11,478 −11,478), all on screen. In the VM rows every tile ran
  through the generated wrapper; in the Mac rows the same programs ran locally.
- Isolated `ATOMICS_HOME`, `ATOMICS_SOCKET` and `ATOMICS_CONFIG`, with the bundle's `config.d.toml`
  as the host file.

## Method notes

- **Which display.** The window opened maximized on the built-in panel, so v5 ran at 120 Hz where v1
  ran at 60 Hz. Changing display settings was out of scope, as in v1.
- **Keystrokes.** ATOMICS repaints a terminal's output at once only for the focused tile typed into
  within the last second; otherwise every 16 ms (focused), 33 ms (visible) or 500 ms (hidden). The probe
  therefore clicks into the echo tile once and types each marker as egui keyboard input, the path a
  keystroke takes. Another app was in front during the runs, so the instrumentation reports ATOMICS's
  window as focused, as it is for someone typing into it. A probe that writes to the PTY without
  typing lands on the 33 ms cadence: 35.0 ms p50 for the VM tile and 35.3 ms locally
  ([runs/pty-write-*.jsonl](runs/)), again the same on both sides.
- **PTY echo** ([pty-echo.txt](pty-echo.txt)) is [measure/pty_rtt.py](measure/pty_rtt.py), unchanged from
  v1: one byte written, its echo read back, 300 samples, outside the app.
- Not measured, as in v1: photons (no screen capture), and frame rate above 120 Hz.

## Agents through hooks and MCP

[plumbing.txt](plumbing.txt) records the check of the generated path with the real CLIs and without a
model turn (Claude and Codex were not signed in to the test VM): both tiles start the real programs in
the VM; each agent's own hook commands, run in the VM, move its tile through idle, working and turn
done; each agent's own MCP server in the VM records an `atomics_report`; the chief runs there with its
own tool set. A live turn is still to be seen once the persistent VM is signed in.
