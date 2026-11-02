# Embedded ATOMICS: extracting the web UI (RELAY-11973 C1)

Companion to `2026-11-02-embedded-atomics.md` (§4.1 client, §9 stream C, §10.2 slicing). Numbers below were counted on
this branch with `grep` and `wc`, not by a build. Line counts are approximate (test modules are mixed into the files).

## 1. What the UI is today

| Thing | Fact |
| --- | --- |
| Size | `app.rs` + `app/*.rs` + `diff_view.rs` + `terminal*` + `theme*` + `markdown.rs` + `panel_nav.rs` + `highlight.rs`: about 44,000 lines, tests included |
| Shape | `State` (`app.rs:441`) is the host model: 117 fields. `Client` (`app.rs:631`) holds view state: drafts, dialogs, `diff_views`, terminal views, expanded tasks. `Desktop<'a> { runtime: &mut State, client: &mut Client }` derefs to `State`. **The split the design asks for already exists** in the type system: draw code is `impl Desktop<'_>` (23 files) or `impl State` (35 files, mostly runtime logic) |
| Writes | Already a queue. `Desktop::post(FnOnce(&mut State) + Send)` (`app/commands.rs:19`) is used at 50 sites, plus `post_plan` and `post_session`, which address by id. A few views still mutate `runtime` directly (`selected_plan`, `plan_view`, `diff_open`, `panel`: view hints) |
| Reads | Direct: `self.runtime.<field>` |
| egui in the model | `State` carries `ctx: egui::Context`, `wake`, `palette: Arc<Palette>` (egui colors and fonts), and `ClientEvent::Viewport` |

### 1.1 What the draw code reads

`runtime.<field>` occurrences in all files (tests included), top of 70+ distinct fields:

| Domain | Fields (uses) | Where drawn |
| --- | --- | --- |
| Plans and board | `plans` 550, `selected_plan` 155, `plan_view` 27, `integration_checks` 22, `integrating`, `changed_plans` | `sidebar.rs` (tabs), `topbar.rs`, `board.rs` (`plan_panel`, kanban), `plan_view.rs`, `plan_cleanup.rs` |
| Sessions | `sessions` 475, `active` 43, `dismissed`, `events` | `sidebar.rs`, `fleet.rs`, `approvals.rs`, `chief.rs`, `input.rs` |
| Tickets | `tickets` 282, `ticket_config()` 24 | `tickets_panel.rs` (3,093 lines), `board.rs`, `sidebar.rs` |
| Config | `config` 252 | everywhere: fonts, keys, layout |
| Look | `palette` 93 | everywhere; egui `Color32` and `FontId` inside |
| GitHub | `github` 104 | `github.rs` (1,635 lines) |
| Diffs | `diffs` 30, `diff_target()`, `diff_open`, `panel` 71 | `editor.rs` (`diff_panel`), `diff_view.rs`, `github.rs` |
| Everything else | `launch_dir`, `sbx`, `relay`, `connections`, `store`, `repos`, `lane`, `heartbeat` | settings menus, trace lane, sandbox UI |

Files with the most `ui` code and `runtime` reads in production code: `board.rs` (89 reads), `github.rs` (71),
`sidebar.rs` (43), `tickets_panel.rs` (35), `editor.rs` (31), `chief.rs` (26). Files that read **no** `runtime` at
all and are therefore movable as they are: `diff_view.rs`, `panel_nav.rs`, `markdown.rs`, most of `topbar.rs` helpers.

### 1.2 Desktop-only dependencies, and where they are used

| Crate | Used in | Web answer |
| --- | --- | --- |
| `alacritty_terminal` | `terminal.rs`, `terminal/runtime.rs` | A cell-snapshot type in `atomics-core` (C3); the web crate never sees `Term` |
| `syntect`, `two-face` (onig, C) | `highlight.rs` only | Section 6: the host highlights; the browser receives spans |
| `cpal` | `voice.rs` | Stays desktop; the web client has no microphone in release 1 |
| `rfd` | `app.rs` | Native file dialog; the web crate asks the `Backend`, never opens a dialog |
| `notify` | `watcher.rs` | Host side (runtime), not UI |
| `ureq`, `ring` | `github.rs`, `relay.rs`, `mcp_client*`, `credentials.rs` | Host side (services); the UI sees `GithubView`-style snapshots |
| `fontique` + fontconfig | `theme/fonts.rs` | Desktop font lookup behind the `Palette` build; the web crate loads bundled fonts |
| `libc` | process and socket code, 19 files | Host side |
| `png`, `eframe` native, `clap` | `main.rs`, `app/editor.rs` (`save_png`) | Desktop shell |

The UI proper uses none of these except through `State`. That is the extraction's good news: the dependencies hang off the
model and services, not off the drawing.

## 2. The Backend trait

`atomics-ui` (new, `crates/atomics-ui`) holds the trait and the plain data it returns. It depends on `atomics-core`
with `default-features = false` and nothing else, so it builds for wasm32; `tests/dependencies.rs` fails if a
desktop-only crate, `atomics`, or `atomics-core`'s `native` feature enters its tree on the host or the wasm target.

```rust
pub trait Backend {
    fn plan_list(&self) -> Vec<PlanRow>;
    fn plan_row(&self, index: usize) -> Option<PlanRow>;
    fn diff_roots(&self) -> Vec<PathBuf>;
    fn diff(&self, root: &Path) -> Option<DiffSnapshot>;
}
```

Rules that keep every slice small and mechanical:

1. **Reads first, owned or shared data out.** `&self` methods returning owned rows or `Arc`s. A protocol client implements
   them over its last snapshot; the in-process one (`app/backend.rs`, `Local`) over `&State`. No method takes a closure
   over the model, so nothing about the UI depends on a lock being held.
2. **A view type only for what the UI reads**, never `State` fields wholesale. Types that are already browser-safe in
   `atomics-core` (`Plan`, `Task`, `ApprovalRequest`, `ReviewComment`) are returned as they are. Types that were in the
   desktop crate move to `atomics-core` first (`diff_data.rs` this time), with a `pub use` left behind.
3. **Writes later, as `Command` values** (slice S8), replacing the 50 `post` closures one domain at a time. Until then
   the draw code keeps `post`.
4. **Indices are positions in the last list read**, as today. The protocol slice (H1) addresses by id; `post_plan` already
   converts.
5. No behaviour change in any slice: the slice's tests are the existing ones, run unchanged.

## 3. Slice plan

Each slice leaves the desktop working and builds alone; each is sized to merge `main` into the branch around. Estimates
are agent-days of focused work, review wait excluded. "C1"/"C2" are the design's streams (§9).

| # | Slice | What moves | Est. | Stream |
| --- | --- | --- | --- | --- |
| **S0** | **Done in this change.** `atomics-ui` crate with `Backend`, `PlanRow`, `PlanState`, `DiffSnapshot` and the dependency test; `Local` in-process impl; plan tabs, top bar plan button and `sync_diffs` read through it; `Prepared`, `FileView`, `Span` moved to `atomics-core::diff_data` | ~400 lines | 1.5 | C1 |
| S1 | Session list: `SessionRow` (id, title, status, kind, chief, branch, root, approval counts, worktree) read by `sidebar.rs` `session_row`, `fleet.rs`, `ungrouped_session`. Largest single read domain (475 uses): do it in three PRs by file | 3 PRs | 2 | C1 |
| S2 | Board: `plan_panel` (`board.rs`, 89 reads) onto `BoardView` { plan, ready ids, session titles, integration notes, integrating, ticket config, board counts }. Plan header first, kanban columns second | 2 PRs | 2 | C2 |
| S3 | Approvals and review comments: `approvals.rs` draw path onto `ApprovalRow`; comments on `DiffSnapshot` already | 1 PR | 1 | C2 |
| S4 | Ticket panels: `tickets_panel.rs` (3,093 lines) onto `TicketsView`. Split by panel (tree, find, sign-in) | 3 PRs | 2.5 | C2 |
| S5 | `Palette` (egui colors and fonts) out of `State`: the web crate owns `Palette`; `fontique` lookup stays a desktop hook that supplies the fonts (`theme::FontSource` already names a font in the config). `config.keys` and `layout` are already core | 1–2 PRs | 1.5 | C1 |
| S6 | Move the files that read nothing from `runtime`: `diff_view.rs`, `panel_nav.rs`, `markdown.rs`, `topbar` helpers into `atomics-ui` (adds `egui` with default features off). Needs S5 for `Palette` | 1 PR | 1.5 | C1 |
| S7 | Move `Client` and the draw functions: `fn draw_*(ui, &dyn Backend, &mut Client)` per file, `Desktop` becomes a shim holding `&mut State` for the writes not yet converted. Order: `topbar`, `sidebar`, `board`, `fleet`, `chief`, `editor` (diff panel), `github`, `tickets_panel`, settings menus | 8–10 PRs | 5 | C1 |
| S8 | Writes: `Command` enum and `Backend::send`, domain by domain, replacing `post` closures (50 sites) and the direct `runtime.*` view-hint writes | 5 PRs | 3 | H4 / C1 |
| S9 | `atomics-ui` builds for `wasm32-unknown-unknown` in CI (`cargo check --target`), eframe web entry point `atomics-web` | 1 PR | 1.5 | C5 |

**Sum:** C1 (S0, S1, S5, S6, S7) ≈ 11.5 days, inside the design's 10–15. C2 reads (S2–S4) ≈ 5.5, inside 5–8. S8 and S9 belong
to H4 and C5. The terminal painter (C3) is independent of this list: it needs only the cell type in `atomics-core`.

Risks: S7 is where merge conflicts with `main` concentrate (draw functions change daily); do it file by file, merging
`main` first each time, and announce large UI changes on `main` early. S4 and `github.rs` carry the most
stateful view code; they may need a `ViewState` struct per panel before they move.

## 4. What S0 changed, exactly

- `atomics-core/src/diff_data.rs`: `Span`, `Spans`, `FileHighlights`, `Prepared`, `FileView` (were in `highlight.rs` and
  `diff_worker.rs`, which re-export them, so nothing else changed).
- `atomics-ui`: `Backend`, `PlanRow`, `PlanState` (moved from `plan_view.rs`; `color()` stays in the desktop as the
  `PlanColor` trait because it needs the palette), `DiffSnapshot`.
- `app/backend.rs`: `Local`, the in-process implementation. `sidebar.rs` (`urgent_tabs_label`, `tabs_summary`,
  `urgent_navigation_width`, `view_tabs`, `view_tab`), `topbar.rs` (plan button) and `sync_diffs` read through it.
  `board.rs` is **not** moved yet: it is S2. The plan list is read once per call, and per tab for `view_tab`.

## 5. The dependency test

`crates/atomics-ui/tests/dependencies.rs` runs `cargo tree --locked -p atomics-ui` for the host target and for
`wasm32-unknown-unknown` (metadata only, so no wasm toolchain is needed) and fails if the tree contains `atomics`,
`syntect`, `onig`, `two-face`, `alacritty_terminal`, `cpal`, `rfd`, `notify`, `ureq`, `fontique`, `ring`, `tempfile`,
`dirs` or `libc`, or if `atomics-core` is present with its `native` feature. A second test checks the list against
`atomics`'s own tree so a rename of a crate cannot make the first test vacuous. Add a crate to the list when the web
crate's reviewers find another one the browser cannot use.

## 6. The highlighter for wasm

**Decision: the web client does not highlight.** The host already colors lines (`Highlighter::file` and `lines` in
`highlight.rs`) and `Prepared` / `FileView` carry the spans (byte ranges and RGBA, `daemon.md:263-268`); the protocol sends
those. The web crate therefore needs no grammar set, and syntect stays on the host with onig, where
`native_highlighting_uses_only_onig` keeps checking it. That also keeps the grammar set out of the wasm bundle (its size is not yet measured).

**Fallback if a host-less path ever needs highlighting in the browser** (no such path is planned): syntect with
`regex-fancy` (`default-fancy`, via two-face `syntect-fancy`). It is pure Rust, uses the same grammars and themes, so its
spans are identical to the host's, and it needs no new dependency family. Pure-Rust alternatives (`synoptic`,
tree-sitter) change the grammars and so the colors, and tree-sitter is C. This choice is a recommendation until it is measured.

**Measurements** (2026-11-02, `scripts/highlight-bench/measure.sh`, release, `opt-level = "z"`, 3 shared CPUs, five
sample files: `app.rs` 1,851 lines, `config.rs` 1,301, `README.md` 749, two small; times are the mean of five runs after a
warm-up):

| Engine | Startup (grammars + theme) | ms per 1,000 lines | Native binary | wasm32 |
| --- | --- | --- | --- | --- |
| onig (desktop today) | 11.6 ms | 36.3 | 2.09 MB | does not build (C) |
| fancy-regex | 11.0 ms | 168.3 (4.6× slower) | 3.14 MB | 2.31 MB raw, 1.34 MB gzip |

Per file, fancy / onig: `app.rs` 270 / 75 ms, `config.rs` 150 / 39 ms, `README.md` 241 / 27 ms (Markdown is 9× slower).
The wasm time was **not** measured (no browser here); wasm usually runs slower than native, so expect more than the native
fancy column. The wasm size is for the highlighter alone, without egui.

**What this settles:** fancy-regex is a working pure-Rust engine with identical grammars, but it costs about 1.3 MB gzip
and several times the CPU, in a browser on a phone. That confirms the decision above: the web client receives spans from the
host and carries no highlighter. Keep `syntect` with `regex-fancy` only as a documented fallback, to be measured in a
browser if a host-less path is ever planned.
