# Embedded ATOMICS through Relay

Design round, 2026-11-02. No code. Decided by the user on 2026-11-02 ("let us go for that, behind the application
switcher"): the full ATOMICS egui UI runs inside Relay, opened from Relay's application switcher. The ATOMICS
host on the developer's machine dials out to Relay, which pairs it with the user's session and forwards only
encrypted traffic. ATOMICS stays agnostic: Relay is one hosted option beside the tailnet and auth-proxy routes.

Reviewed the same day (section 13). The review corrected host authentication (5.3) and where the browser keys live
(4.2, 8). Relay's side of this design (its ingress, session cookie and headers, deployment, the citations into its
repositories, and its work items) is kept with Relay, not in this public repository. This record keeps what concerns
ATOMICS, and what ATOMICS expects of any relay (7.1).

**The user's decisions after the review (2026-11-02):**

1. A signed-in, paired session may **type in terminals**. ATOMICS needs a good mobile UX for it: touch, a key bar,
   a compact layout (4.4).
2. **Live, interactive terminals are in the first release**, with the board, plans and diffs (9).
3. **Full scope**, despite the larger estimate.
4. **One long-lived feature branch per repo** (`relay/<ticket>-embedded-atomics`). Nothing merges to `main` until the
   whole thing works without regression (10).

## 0. Sources

Every ATOMICS citation was read on `origin/main` (`49cc657`) after `git fetch` on 2026-11-02. **REG** marks crate
sources in the local cargo registry (`eframe-0.37.0`, `egui-wgpu-0.37.0`, `wgpu-30.0.1`), as locked in `Cargo.lock`.
Relay's side was read in Relay's repositories; those reads are kept with Relay.

What reading cannot settle needs a device or a build, and is marked **[SPIKE]**: the iOS soft keyboard, iOS pairing
storage, bundle size, `snow` on wasm32, and the relay's measured idle timeout. They are the remaining Phase 0 items
(section 9).

## 1. Where things stand

### 1.1 ATOMICS

**Phase 2 is complete and phase 3 has not started.** `host::Host` owns the runtime on the
`atomics-runtime` thread (`docs/daemon.md:238-241`, `crates/atomics/src/host.rs:13-42`). It is still in-process
and still takes an `egui::Context` (`host.rs:24`). The desktop borrows the whole model under one mutex
(`host.rs:44-48`; `daemon.md:252-253`), and exiting the app still joins the host and shuts the terminals down
(`daemon.md:317-319`). The "Remaining for phase 3" list (`daemon.md:306-319`) is untouched: no headless start, no
projections or snapshots, no WebSocket server, no journal. `Cargo.lock` has no WebSocket, AEAD or Noise crate;
`ring` is present (PKCE, `mcp_client/oauth.rs:1-8`), `tokio`, `wgpu` and `web-sys` only transitively through
eframe.

Phase 1 holds only for the core crate: `atomics-core` builds for wasm32 with `--no-default-features`, and a test
enforces it (`crates/atomics-core/src/platform_tests.rs:14-24`). The UI crate does not, for three reasons
measured on `origin/main`:

- **Size and coupling.** `app.rs`, `app/*.rs`, `terminal*` and `diff_view.rs` are 41,803 lines. The UI is
  written as methods on the host model: 37 files under `crates/atomics/src/` hold `impl State` blocks, and
  `State` carries the `egui::Context`, the wake handle and the control sender (`app.rs:438-445`).
- **The terminal painter reads the emulator.** `terminal.rs` paints from alacritty's `Term` directly
  (`terminal.rs:1-13`, `:133`, `:255` `renderable_content()`), so a browser painter needs a new cell-snapshot type,
  as `daemon.md:44-45` already says.
- **Desktop-only dependencies.** `syntect` uses `default-onig` (`crates/atomics/Cargo.toml:34`), a C regex
  library that does not build for `wasm32-unknown-unknown`; `alacritty_terminal`, `cpal`, `rfd`, `notify`,
  `ureq` and `fontique` with fontconfig (`Cargo.toml:11-33`) are desktop crates. The client does not need any of
  them: diff highlight spans already travel as byte ranges with RGBA (`daemon.md:263-268`), and screens arrive as
  cells.

That extraction is the critical path of the whole project (section 9).

**The protocol is specified, not built.** `daemon.md:67-110` proposes `atomics.v1` over a same-origin WebSocket with
JSON envelopes, cursors, `ack`/`resync`, operation IDs and journaling. `daemon.md:112-145` covers terminal
snapshots and deltas, and `daemon.md:147-167` covers reconnect and control leases. This design adopts that
protocol and adds only a session layer under it (section 5).

**The Relay remote control is live and already covers the everyday phone surface** (chief conversation, steering,
`/approve`): a polled, ticket-hosted conversation through MCP tools (`docs/tickets.md:836-985`, `988-1073`). It is
not end-to-end private: the mirror posts the chief's prompts and answers to Relay (`tickets.md:864-866`). Embedded
ATOMICS adds what that cannot: terminals, diffs, the board, and a private channel.

**ATOMICS already uses the word "relay" for something else**: the host credential relay (`crates/atomics/src/relay.rs:1-11`).
New modules therefore use "tunnel" and "rendezvous", never "relay", which also serves requirement 5.

### 1.2 The hosting side

Relay hosts the web app the user signs in to, the user's ticket connection (an MCP server with OAuth, where ATOMICS
signs in as the user's personal agent, `docs/tickets.md:1040-1049`), and real-time services of its own, none of which
is a duplex byte pipe to a host. Its routes, cookies, headers, application switcher and service layout were checked
against Relay's repositories and shaped Relay's half of this design, which Relay keeps. ATOMICS depends on none of
them: only on the contract in 7.1.

## 2. Decisions at a glance

| # | Decision | Chosen | Main rejected |
| --- | --- | --- | --- |
| 1 | Protocol | `atomics.v1` from `daemon.md`, all of it in release 1 (state, diffs, mutations, terminals with input and leases), built in slices on the feature branch. Served first by an in-process protocol thread in the running desktop app | Waiting for the headless host; a Relay-specific protocol; raw ANSI streaming |
| 2 | Client | The egui UI as one wasm bundle in an iframe mounted by a route of the hosting app, with a mobile UX mode for touch and terminals | A canvas inside the hosting app's wasm; a Flat-native (Leptos) client |
| 3 | Tunnel | A rendezvous broker that pairs a host's outbound socket with browser sockets and relays opaque frames, run by the hosting product (where and how is that product's decision, 5.2) | A tunnel over the hosting product's own real-time hubs, which would make the rendezvous product-specific |
| 4 | Host auth | **Both**: the user's authenticated ticket connection (for Relay, the personal-agent OAuth connection) registers the host and fetches short-lived tunnel passes through profile operations; the host's device keys sign the pass request and are what browsers pin | Device-code claim only; a long-lived bearer |
| 5 | E2E | Noise IK over the tunnel, paired by QR or link whose secret is in the URL fragment, confirmed with a short code on both screens | TLS only; hand-rolled ECDH; a PAKE-only scheme |
| 6 | Agnostic | A `Link` abstraction with direct and rendezvous transports; same inner protocol; the rendezvous issuer is mapped by profile like the ticket operations | A Relay-aware ATOMICS |
| 7 | Security | Exact Origin, single-use connect ticket, typing for a signed-in paired device holding the terminal's lease, revocation on both sides, the client on a dedicated origin (recommended, open question 1) | Treating the hosting app's login alone as enough for typing; device keys in the hosting app's origin |
| 8 | Delivery | One long-lived feature branch per repo, `main` merged in often, one regression gate, then an ordered final merge (10) | Merging phases to `main` behind flags |

## 3. Protocol (decision 1)

### 3.1 Use `daemon.md`'s protocol; change its order, not its content

The wire (tags, cursors, epochs, operation IDs, bounded queues) is as proposed at `daemon.md:67-110`. All three
slices below ship together in release 1; the order is the build order on the feature branch, chosen so that each
slice can be tested before the next depends on it.

**P3a, read-only board, diffs and plans.** Messages: `hello`/`welcome`, `state_snapshot`, `session_upsert`/`removed`,
`plan_upsert`/`removed`, `task_report`, `approval_pending`/`resolved`, `diff_summary`/`diff_file`/`file_content`
with `diff_get`/`file_get`, and `ack`/`resync`/`ping`/`pong`/`error` (`daemon.md:76-95`). First because it is
nearly stateless (revision-addressed snapshots; reconnect is "send a snapshot at cursor N", `daemon.md:152`) and
uses types that already exist in the browser-safe core (`daemon.md:53-55`, spans `daemon.md:263-268`).

**P3c, terminals**, built in three steps:

1. `screen_snapshot` only, full snapshots on the existing 100/200/500 ms cadence (`daemon.md:127-128`,
   `260-261`). An 80×24 screen is a few KB as JSON before compression.
2. `screen_delta` with generation and base revision, `history_get`/`history_chunk` (`daemon.md:121-125`).
3. `terminal_input`/`terminal_resize` with the controller lease and fencing token (`daemon.md:109`, `139`,
   `161-164`).

**P3b, mutations:** `approval_decide` (first valid decision wins, `daemon.md:165-166`), `plan_command` approve,
`review_command` comments, and `session_command` steering, with operation IDs and the journal (`daemon.md:103-108`).

### 3.2 Serve it first from the running desktop app

`daemon.md:306-319` makes phase 3 a separate headless host. For Relay that is the wrong first move. The user's
host today is the desktop app, left running while the user is away; a headless host needs shutdown ownership
changed (`daemon.md:317-319`) and an egui-free service construction (`daemon.md:308-309`), both invasive.

**Decision:** the protocol ships as a server thread inside the desktop process. Projections (snapshot structs built
from plain data) are built on the runtime thread at its existing deadlines and published as immutable `Arc`
snapshots, so the protocol thread never takes the `State` mutex (`host.rs:44-48`) and encodes and sends with no
lock held, as `daemon.md:129` requires for terminals. Terminal snapshots are copied under the short emulator lock and
sent after it (`daemon.md:129`). The headless split (phase 3 proper) becomes moving that thread behind `Host::start`
without an `egui::Context`, not a redesign. Remote access is **off by default**: with no `[remote]` configuration
there is no thread and no socket, so the desktop behaves exactly as today (a regression-gate check, 10.3).

Rejected: **wait for the headless host first** (adds weeks, and the desktop is where the user's agents run).
Rejected: **raw ANSI over the wire** (contradicts `daemon.md:114-115`, and the client would need a second emulator).

### 3.3 Carrier

JSON text frames, as `daemon.md:70` says to start. Measure before binary or compression (`daemon.md:134`); terminal
deltas are the first candidate if the phone's data use or latency is poor. Frames are capped at 256 KiB before
encryption; snapshots, diffs and history are chunked with transfer IDs (`daemon.md:133`).

## 4. Client (decision 2)

### 4.1 The egui UI on wasm32

The client is the existing egui UI compiled with eframe's web backend. Facts from the review:

- **Renderer: no spike needed.** eframe 0.37's default `wgpu` feature enables `egui-wgpu/default`, whose
  defaults enable `wgpu/default` (which includes `webgpu`) and `wgpu/webgl` (REG `eframe-0.37.0/Cargo.toml`
  `[features]`, `egui-wgpu-0.37.0/Cargo.toml` `default`, `wgpu-30.0.1/Cargo.toml` `default`). One bundle already
  falls back at runtime.
  ATOMICS uses wgpu only to raise the texture limit (`crates/atomics/src/main.rs:392-399`), so eframe's `glow`
  (WebGL2 only, smaller) is an alternative if the size spike says so. `daemon.md:5`'s "WebGPU" should read
  "wgpu (WebGPU or WebGL2)".
- **Extraction:** a `Backend` trait with two implementations (in-process for the desktop, protocol for the browser
  and a loopback native client), which is `daemon.md:36-46`'s client list. Because the draw code is `impl State` in
  37 files (1.1), this rewrites every reader; it is done in small mechanical slices (accessors first, then moving
  draw code onto the trait) so that `main` can be merged in between (10.2).
- **Dependencies:** the web crate must not depend on `syntect`/`onig`, `alacritty_terminal`, `cpal`, `rfd`,
  `notify`, `ureq` or fontconfig (1.1). A test like `platform_tests.rs:14-24` enforces it.
- **Terminal rendering:** one painter over a cell-snapshot type in `atomics-core` (wide and combining cells,
  colours, cursor, selection), used by the desktop too, so the browser and the desktop cannot drift.

Rejected: **a Flat-native (Leptos) client.** The review proposed one as a read-only stepping stone that could ship
weeks earlier. The user put interactive terminals in the first release and nothing merges before the whole thing
works, so a stepping stone would not reach anyone sooner and would duplicate the board, diff viewer and terminal grid.

### 4.2 Embedding and origin

| Option | Verdict |
| --- | --- |
| **An iframe mounted by a route of the hosting app** (`/atomics`, bundle under `/atomics/v/<major>/`) | **Chosen.** Two wasm apps never share a canvas, a panic domain, a keyboard or an IME. The iframe owns pointer capture, focus, gestures and the soft keyboard. The hosting app only decides when to mount it |
| A canvas inside the hosting app's page and wasm module | Rejected. Two event loops, shared focus and key handling, one bad panic takes down the shell; the hosting app would have to link ATOMICS's UI |

**Which origin the iframe runs on is open question 1; the recommendation is a dedicated origin in release 1.** In
a same-origin iframe the phone's Noise key and every decrypted frame sit in the hosting app's origin, and the key is
extractable (6.2). Any XSS anywhere in the hosting app could then read terminals and type into them, since typing
ships in release 1. A dedicated origin confines that to the ATOMICS bundle, which gets a strict CSP of its own (no
`'unsafe-inline'`). The objection that host-only cookies don't reach it does not apply: the hosting app mints the
single-use connect ticket with its own session (5.4) and hands it to the iframe by `postMessage` with an exact
`targetOrigin`. The ticket is single-use, 30 seconds and bound to user, host and session, so passing it is not
passing a session.

The iframe's pages need their own header block: `frame-ancestors` naming the hosting app's origin, `X-Frame-Options`
to match, `Cache-Control no-store` on the loader, and a CSP that allows `wasm-unsafe-eval` and `wss:` to the
broker. No threads, so no COOP/COEP.

The hosting app gives the iframe a boot object by `postMessage` after load (mode, host list, theme, locale, connect
ticket), never the session cookie. The exact contract (message types, fields, the parent-origin config) is
`docs/daemon.md`, "The browser client and its embedding".

### 4.3 Asset hosting and versioning

- ATOMICS CI builds `atomics-web-<version>` with content-hashed filenames and a manifest
  `{atomics_version, protocol_major, protocol_minor_max, wasm_sha256, built_from}`. It is a release artifact of
  the ATOMICS repo (MIT, public), with no Relay content in it. On the feature branch, CI builds it from the branch.
- A hosting product serves **one bundle per protocol major**, under `/atomics/v/<major>/`, immutable and
  long-cached, with the loader `no-cache`.
- **Versioning against the host:** minors are additive and capability-gated; a major mismatch fails before
  subscription (`daemon.md:97`). The broker's plaintext presence record carries the host's protocol major, so the
  hosting app picks the right `/atomics/v/<major>/` before connecting. A host on an unserved major shows "update
  ATOMICS" or "open on the desktop".
- **How the bundle gets into a hosting product** is that product's decision. ATOMICS asks only that it be pinned by
  version and SHA-256 in a reviewed change, so a compromised ATOMICS release does not reach users unreviewed.
- Rejected: **serve the client from the host through the tunnel.** Code would arrive before any key is
  established, from a peer the user has not yet authenticated.

### 4.4 Mobile UX, terminals first

As the user's ticket comment of 2026-10-31 says: no new screens, a mobile UX mode. Typing in terminals
from a phone is in release 1, so the terminal is the part of the mode that gets the most design:

- **Mode:** chosen from viewport width and `pointer: coarse`, with a manual override. The top bar was tested at
  480/550 points (`daemon.md:292-294`).
- **Compact layout:** one pane at a time (board, a plan, a diff, a terminal, the chief), switched from a bottom bar
  like Flat's phone shell. A terminal fills the width.
- **The grid is the desktop's.** Resizing comes only from the lease holder and viewers scale or letterbox
  (`daemon.md:139`). A phone that takes the lease does **not** resize the PTY by default, because that would rewrap
  the terminal on the desktop; it pans and zooms the canonical grid. "Fit to this screen" is an explicit action
  that resizes, visible on the desktop, and the size returns when the desktop takes the lease back.
- **Key bar** above the soft keyboard: Esc, Tab, sticky Ctrl and Alt, arrows, PgUp/PgDn, `|`, `~`, `/`, `-`, and
  paste. It follows `visualViewport` so it sits on the keyboard. Keys are encoded with the latest published
  terminal modes (`daemon.md:136`).
- **Touch:** tap focuses and places the cursor where the application accepts mouse reports (from cell coordinates,
  `daemon.md:137`); one-finger drag scrolls history (per client, `daemon.md:124-125`); long-press selects, with
  copy; pinch changes the font size; a hardware keyboard (iPad) works without the bar.
- **Typing state is visible:** the lease holder is shown on every client; "typing from iPhone" shows on the desktop,
  which takes the lease back with one click (`daemon.md:161-163`).
- **Soft keyboard and IME** in an egui canvas were the main risk **[SPIKE, decided]**. Terminal input goes through a
  native `<textarea>` over the terminal in the iframe, which forwards committed text as `terminal_input` and
  composition as preedit; eframe's hidden text input is not used for terminals ("Spike results" below).
- **Offline:** cached content shows as disconnected and input is disabled until synchronised (`daemon.md:150`);
  input is never replayed across a disconnect (`daemon.md:109`).

**Pairing on a phone happens inside the installed PWA [SPIKE].** On iOS a home-screen web app has storage separate
from Safari, and a QR code scanned with the Camera app opens Safari. A key paired there is not in the PWA, and Safari
may clear an unused site's storage after 7 days. So the pairing page offers, inside the PWA, a camera scanner (the
hosting app's `Permissions-Policy` must allow the camera on the pairing page) and "paste the link". The QR stays for
Android and desktop browsers, where the link opens the same profile. The spike also confirms that an iframe on a
same-site dedicated origin keeps its storage inside the standalone PWA.

**Spike results (2026-11-02).** The page is `spikes/wasm` (README, "iPhone test", with the
checklist). It feeds one egui terminal widget from three paths: **a** eframe's text agent as shipped (an `<input>`
with only `autocapitalize="off"`), **a+** the same input with `autocorrect`, `autocomplete` and `spellcheck` off,
set by the page, and **b** a transparent `<textarea>` over the terminal that forwards committed text, composition
(as preedit) and Backspace/Return. The key bar follows `visualViewport`. Event logs stay on the serving machine.

Device: an iPhone on iOS 26.7.2 (from the logged user agent).

| Run | Result |
| --- | --- |
| Safari, **b** | Pass, by the user's report ("works pretty well"). No per-check notes, no log uploaded |
| Chrome on iOS, **b** | Pass, by the same report. No per-check notes, no log uploaded |
| Installed PWA, **b** | Pass, by the user's report. No per-check notes, no log uploaded |
| **a** and **a+**, any browser | Not run against the checklist |

One Chrome session on the default page (**a**) was logged in passing: text, Return, `åäö` (precomposed) and five
emoji each arrived once, and the key bar's Tab and arrows sent their sequences, though it was pressed with the
keyboard closed. Chrome on iOS shrinks the layout viewport for the keyboard (`innerHeight` 681 → 382), and the key
bar's position followed it.

**Decision: C4 uses the `<textarea>` overlay (b).** The browser owns the native input, composition and dictation,
and the client sets the input's attributes itself instead of relying on eframe's text agent, which as shipped leaves
autocorrect on. **a** and **a+** are not pursued. Final: **b** passed in iOS Safari, iOS Chrome and the installed
PWA.

**As built (C4, 2026-11-03).** `docs/daemon.md`, "The browser client and its embedding", describes the mode as
built. Two things differ from the spike and need an iPhone check before M4. A touch on the overlay is now the client's:
`touchstart` is prevented, so iOS's loupe, callout and page zoom never start, and a tap focuses the field from its
own `touchend` handler; the spike let the browser focus it natively. And the key bar shows only on a coarse
pointer, so a desktop browser's terminal keeps its pane switcher. The rest was checked in headless Chrome
(`tests/browser.rs`): typing and Return, the echo, losing the lease to the desktop and taking it again.

### 4.5 The entry point

Where the hosting app shows ATOMICS is its decision; in Relay it is an item in the application switcher, shown on
phones too, since the phone is the main target. ATOMICS supplies what the item opens:

- No paired host: the pairing page; on a desktop "Open the ATOMICS app" (`atomics://open…`, `docs/tickets.md`) stays
  as the second action. That keeps the fallback the task requires.
- One paired host: open it. Several: a host picker.
- The entry is gated per account, so an account without the feature does not see it.
- The remote control's "Open in ATOMICS" button (the user's comment of 2026-10-31) deep-links to
  `/atomics?host=<id>&view=chief`.

## 5. Tunnel (decision 3)

### 5.1 Shape

```
ATOMICS host ──wss (dial out)──▶ broker ◀──wss── ATOMICS iframe in the web app
     │  Noise-encrypted frames pass through unchanged  │
     └────────────── Noise IK end to end ──────────────┘
```

The broker pairs sockets, enforces auth and quotas, and relays opaque binary frames. Every frame has a small
plaintext header `{kind: data|ctrl, channel}`. The broker assigns each browser connection a `channel`; the host
sees `joined(channel)` and `left(channel)` control frames and runs one Noise handshake per channel. Those control
frames are untrusted hints: a forged `joined` yields a failed handshake, nothing more.

### 5.2 The broker

The broker belongs to the hosting product: where it runs, how it is deployed, routed and scaled are that product's
decisions, and Relay keeps its own. What ATOMICS needs from any broker is 7.1: it pairs a host's socket with browser
sockets, admits each only with a single-use pass or connect ticket minted elsewhere (single use enforced by
recording the pass id until it expires), relays opaque frames, enforces the byte and time limits of 5.5 and closes
with typed codes. It needs no browser session or OAuth of its own.

A broker restart shows as a short freeze and a reconnect: the client gets a snapshot, and leases fence any input
sent across it (`daemon.md:162`). The 25 s ping stays well inside common proxy idle timeouts; a deployment measures
its own **[SPIKE]**.

Rejected: **a tunnel over the hosting product's own real-time hubs.** It would make the rendezvous protocol
product-specific (section 7).

### 5.3 Host authentication: the personal agent registers, the device key proves (corrected)

**The draft's premise was wrong.** It rejected the OAuth identity because "the user's connection is a product agent
with no owner". That came from the remote-control review's Q1
(`docs/decisions/2026-11-01-relay-remote-control-review.md`), which assumed ATOMICS signs in as a product agent.
It does not. ATOMICS's ticket connection signs in by OAuth as the user's personal agent: whoami returns
`kind = "personalAgent"` and `ownerUserId` = the human (live on 2026-11-02 as the user's personal agent, owned by
the user), and the issuer creates that agent only for the consenting user. ATOMICS documents this setup
(`docs/tickets.md:1040-1049`) and already relies on it: the default steering list is whoami's owner
(`tickets.md:843-848`). So the OAuth connection is exactly the binding the tunnel needs: a credential the human
consented to, whose agent names that human.

What OAuth cannot do alone: the access token's audience is the MCP resource, so a broker that accepted it would be
accepting a token issued for someone else (token passthrough, which the MCP authorization spec forbids). And an
OAuth token does not give browsers a key to pin for end-to-end encryption.

**Chosen: both.**

1. The host creates an Ed25519 signing key and an X25519 Noise static key, kept in the OS keychain through
   `credentials::Store` (`crates/atomics/src/credentials.rs:1-4`; no plaintext fallback).
2. Settings → "Phone and browser access" calls the profile's **register** operation, a tool on the ticket
   connection's MCP server, with the two public keys, a display name and the protocol major. It runs as the
   personal agent; the issuer binds `host_id` to the agent's owner. A connection whose whoami has no owner
   (`kind = "agent"`) is refused, the same rule remote steering uses (`tickets.md:1047-1049`).
3. To dial, the host calls the **pass** operation with `host_id` and a signature over a fresh server nonce. The
   issuer returns a 5-minute single-use tunnel pass. The host presents it in the upgrade's `Authorization` header,
   never the URL (`daemon.md:180`).
4. Revocation is layered: revoking the OAuth client stops new passes at once, because the issuer re-derives the
   grant on every MCP call; the broker closes the host's sockets within the pass lifetime; deleting the host on the
   hosting side or rotating the device key in ATOMICS cuts it too.

Why both: OAuth proves which human, with no new consent and no new scope. The device key keeps tunnel auth
independent of token refresh, means a leaked access token alone cannot occupy the host's slot, and is the identity
phones pin. The pairing QR then carries only E2E material (6.1); the draft's `claim_secret` and claim endpoint go
away.

Fallback, deferred: a host without an OAuth connection (a self-hosted rendezvous, or a hosting product without the
ticket integration) uses a device-code claim approved in the signed-in browser, the RFC 8628 pattern. Not in
release 1.

Rejected: **device-code claim as the only path** (a second login ceremony for something the OAuth consent already
proved). **A long-lived bearer pasted into the host** (theft is silent and it breaks `daemon.md:180-181`).
**Accepting the MCP access token at the broker** (wrong audience).

### 5.4 Pairing a browser session with the host; many hosts

- Browser side: the hosting app's signed-in page asks its own backend, with CSRF protection, for a **single-use
  connect ticket** valid for 30 seconds and bound to the user, host and session. The client opens the WebSocket and
  presents the ticket in `Sec-WebSocket-Protocol` (not the URL), so the upgrade itself is authenticated, as
  `daemon.md:171-172` requires. The broker never reads the session cookie.
- The exact `Origin` check and the ticket are the cross-site defences, not the session cookie, whatever its
  settings.
- **Signed in means signed in:** the broker re-checks the session behind each channel every 60 s and on sign-out,
  and closes the channel when it has ended. Typing therefore needs a live session on the hosting app as well as a
  paired device (the user's decision 1).
- Presence: the hosting app lists the user's hosts with name, online state, protocol major and last seen
  (plaintext, user-chosen display name).
- **Several hosts per user:** limit 5 hosts per user to start. Each host allows several browser channels
  (`daemon.md:161`; limit 4 at first). A host is not shared between users in v1.

### 5.5 Limits and backpressure

The broker cannot read frames, so it cannot discard superseded deltas the way `daemon.md:130-132` does. Policy is
split:

- **The host applies the semantic policy before encrypting**, per channel: bounded queue, supersede deltas with
  a snapshot, disconnect a persistently slow client, never delay approvals, and flush input echo at once
  (`daemon.md:127-132`).
- **The broker applies byte and time limits:** frame ≤ 256 KiB; per-channel send buffer ≤ 1 MiB, and a channel
  whose buffer stays full for 10 s is closed (the client reconnects and the host sends a fresh snapshot);
  per-host sustained ≤ 2 MiB/s with a burst of 8 MiB; per-user 5 hosts and 4 channels per host; pass and ticket
  issuance rate-limited per user; ping every 25 s with a 60 s idle close; unauthenticated upgrades rejected before
  any allocation. Small frames (keystrokes, echo) are forwarded without batching.
- Quotas count bytes only, which suits E2E: the broker never needs content to enforce them.
- Close codes are typed (`unauthorised`, `revoked`, `slow`, `replaced`, `quota`), and the client maps them to
  messages and to "reconnect" or "re-pair" actions.
- Reconnect uses jittered backoff and the epoch/cursor rules of `daemon.md:73`, `151-159`.

## 6. End-to-end encryption and pairing (decision 5)

### 6.1 Pairing

Settings shows a QR code and a text link (open question 3 covers short codes):

```
https://<relay origin>/atomics/pair#<v>.<host_id>.<host_noise_pubkey>.<psk>
```

The pairing data sits in the **URL fragment**, which a browser never sends to a server; `psk` and the host key never
go to the broker. The hosting app hands the fragment to the ATOMICS iframe and removes it from the address bar at once
(`history.replaceState`). The host-to-user binding comes from the OAuth registration (5.3), so the fragment carries no
claim secret. The `psk` is single-use, expires in 5 minutes, and the host refuses a second pairing until the first
completes or expires. On iOS the pairing happens inside the installed PWA (4.4).

**Both screens show a six-digit code** derived from the Noise handshake hash, and the desktop asks "Pair 'iPhone
Safari'? Codes match?". A link that leaked, or a script that read the fragment and raced the phone, produces a
different code, so a device cannot be added silently. A paired device may type (the user's decision 1); the prompt
also offers "view only" for a device that should not.

### 6.2 Handshake and transport

- First pairing: `Noise_IKpsk2_25519_ChaChaPoly_SHA256`. The phone is initiator, knows the host's static key from
  the fragment, and sends its own static key encrypted. The `psk` proves it saw the QR.
- Known devices: `Noise_IK_25519_ChaChaPoly_SHA256`. The host finds the phone's static key in its allowlist.
- The prologue binds `atomics.v1`, the protocol major and `host_id`, so a frame cannot be moved between hosts.
- Transport messages carry Noise's counter nonces: reordering, replay and truncation inside a session fail
  authentication. Rekey every 1 GiB or hour. A new session uses fresh ephemeral keys, so a recorded session
  replays nothing. The first IK message is not forward secret against a later leak of the host static key
  (known Noise IK property), which is acceptable because that message contains only the phone's public key.
- Everything the protocol carries (`hello` onward, `daemon.md:76`) is inside the encrypted channel. Before
  authentication the host parses only a bounded Noise message, never JSON.
- **The phone's static key lives in browser storage** (IndexedDB) and in wasm memory, extractable. That is in
  tension with the spirit of `daemon.md:180` and is the reason for 4.2's origin recommendation. A non-extractable
  WebCrypto X25519 key is a later hardening; `snow`'s resolver is synchronous, so it needs an async DH adapter.
- Libraries: a maintained Noise implementation with pure-Rust primitives that builds for wasm32 (candidate:
  `snow` with RustCrypto), shared between host and client **[SPIKE]**. `ring` (already in the tree for PKCE)
  needs a C toolchain for wasm32, so it is not the default.

Rejected: **TLS terminated at the broker** (Relay would see everything, the opposite of the requirement).
**Hand-rolled ECDH plus AES-GCM** (nonces, rekeying and replay are exactly where hand-rolled schemes fail).
**Per-message HPKE or `age`** (no ordering or replay protection, no sessions). **Signal's double ratchet** (two
parties, one session at a time, no offline queue; too much for the benefit). **PAKE only** (no durable device
identity for revocation).

### 6.3 What Relay can and cannot see

| Relay sees | Relay cannot see |
| --- | --- |
| The user, `host_id`, the host's display name, registration and pairing times | Terminal contents, prompts, keystrokes |
| Online/offline and connect/disconnect times, source IPs | Diffs, file contents, paths, repo and branch names |
| Protocol major (for bundle choice), channel numbers | Plans, tasks, approvals and their decisions |
| Frame sizes and timing, byte counts (typing rhythm and a terminal's activity pattern leak) | Which device holds a lease or is view-only |
| The host's public keys | The `psk`, session keys, private keys |

A malicious broker can drop, delay or cut frames, or refuse service. It cannot forge, replay or reorder them.

### 6.4 The honest limit: Relay serves the client code

An end-to-end protocol is only as strong as the code that runs it, and Relay serves that code. A compromised
Relay deploy, or an XSS in the origin the client runs in, can read what the user sees and type what a paired
device may type. So the claim is "Relay is only a pipe for the data path and a passive or logging-only compromise
reveals nothing", not "a fully compromised Relay cannot read anything". Mitigations, in order:

1. The client on a dedicated origin with a strict CSP (4.2, open question 1), so an XSS in the hosting app does not reach it.
2. The bundle is a reproducible build with a published hash; releases are signed by an ATOMICS release key, and
   bundles are pinned by a reviewed PR (4.3).
3. A loader checks the hash against the signed manifest and asks the user to confirm a major bump. The check lives
   in the loader page, not a service worker.
4. The strongest option is not to trust the hosting origin: a native client, or the direct route in section 7.
   Both remain supported.

## 7. Agnosticism (decision 6)

A hosted relay is one way to reach a host, beside the two direct routes: a tailnet ingress, or an auth proxy in
front of the host (`daemon.md`, "Remote authentication and isolation"). Relay is one hosted relay. Any service that
keeps the contract in 7.1 works, and nothing in ATOMICS names it.

- ATOMICS defines a `Link` abstraction (an ordered message duplex with `close(code)`) and two transports:
  **direct** (host serves assets and `wss` at its own origin behind a tailnet ingress or auth proxy, with the
  rules in `daemon.md:169-187`) and **rendezvous** (the host dials out to a configured URL). The wasm client takes
  the transport from its boot object: `direct` or `rendezvous {url, host_id}`.
- The **inner protocol is identical.** Noise wraps it on the rendezvous transport; on direct it is TLS plus the
  proxy's login, per `daemon.md:173-176`. The Noise layer is optional on direct transports so a tailnet user is not
  forced to pair keys; it is a flag in `welcome`.
- **The issuer is mapped, not named.** `register` and `pass` are profile operations on an MCP server, mapped the way
  `[tickets.operations]` maps ticket tools (`docs/tickets.md:995-1005`), or a static bearer for a simple
  self-hosted server. Rendezvous is a documented protocol (a one-page spec in the ATOMICS repo): any server that
  pairs two WebSockets and checks a pass works. Relay's broker is one implementation.
- **No Relay names in ATOMICS code.** Config is in 7.2; UI strings say "your account's rendezvous service" unless a
  profile supplies a label. A test greps the new modules for `relay` / `Relay`, the same discipline the
  remote-control work held. The Relay example profile in `atomics-core/src/config.rs` may gain the URL and
  operations as commented examples, nothing else.
- The bundle is an ATOMICS release artifact with no Relay content (4.3); Relay hosts it like any other server could.
- The **desktop `atomics://` fallback** stays (section 4.5).

### 7.1 What ATOMICS expects of any relay

The broker-facing half is specified in `docs/daemon.md`, "Rendezvous". In short:

- **The `atomics.v1` contract, untouched.** Everything from `hello` onward travels inside the Noise channel (6.2).
  The relay never parses, rewrites or reorders protocol messages; the host applies the protocol's own policy
  (bounded queues, superseded deltas, slow clients) before encrypting (5.5).
- **The tunnel framing.** Binary WebSocket messages with the plaintext header `{kind: data|ctrl, channel}`, one
  channel per browser connection, and `joined(channel)`/`left(channel)` hints that the host treats as untrusted
  (5.1).
- **Exact origins.** A browser upgrade is admitted only when its `Origin` equals the client's origin exactly: no
  suffix or pattern match, and a session cookie alone never admits a socket.
- **Tickets for browsers.** Single use, about 30 seconds, bound to user, host and session, presented in
  `Sec-WebSocket-Protocol`; the session behind each channel re-checked about every minute and on sign-out (5.4).
- **Passes for hosts.** Issued by the profile's `register` and `pass` operations to a host that signs a fresh
  nonce with its device key; single use, about five minutes, in the upgrade's `Authorization` header, never a URL
  (5.3).
- **Typed close codes:** `unauthorised`, `revoked`, `slow`, `replaced`, `quota`. The client maps each to a message
  and to "reconnect" or "re-pair" (5.5).
- **Limits by bytes and time only**, as in 5.5, with no content needed to enforce them; logs hold sizes and codes,
  never tokens or frames.
- **Serving the client:** one immutable `atomics-web` bundle per protocol major, pinned by version and SHA-256 in a
  reviewed change, on its own origin with a strict CSP (4.2, 4.3).

### 7.2 ATOMICS-side config

The protocol server is off unless configured (`docs/daemon.md`, "Protocol server"):

```toml
[remote]
enabled = true
allowed_origins = []     # exact Origin values for the direct routes

[remote.rendezvous]      # stream H5: designed, not built yet
url = "wss://rendezvous.example/atomics/tunnel"
```

`register` and `pass` are operations on the ticket profile's MCP server, mapped the way `[tickets.operations]`
maps ticket tools (`docs/tickets.md:995-1005`), or a static bearer for a simple self-hosted rendezvous. A profile
may give the service a label for the UI. Device keys live in the OS credential store, never in the config.

## 8. Security (decision 7)

### 8.1 Controls

| Threat | Control |
| --- | --- |
| Cross-site WebSocket hijack | Exact `Origin` check on every upgrade (broker and any direct host); a single-use connect ticket in `Sec-WebSocket-Protocol`; the broker never accepts the cookie, and its settings are not relied on. Repeats `daemon.md:178-179` |
| CSRF on connect, register, revoke | CSRF protection on the hosting app's browser endpoints; register and pass are MCP calls with an OAuth bearer, which a browser cannot send cross-site |
| Session theft or replay of a ticket | Tickets are single-use, 30 s, bound to user, host and session; the broker re-checks the session every 60 s and on sign-out, closing channels for ended sessions (`daemon.md:182`) |
| Stolen OAuth token | Cannot dial without the device signing key; cannot impersonate the host to phones without the Noise static key; revoked with the OAuth client, effective on the next MCP call |
| Leaked pairing link | Single-use `psk`, 5-minute expiry, one pairing at a time, and the six-digit code compared on both screens (6.1) |
| Replay of frames | Noise counter nonces, per-session ephemeral keys, single-use pairing secrets |
| XSS in the hosting app | With the dedicated origin: no access to the client, its keys or its frames. With a same-origin iframe: full access to what the device sees and may type (open question 1) |
| Stolen or lost phone | Typing also needs a live session on the hosting app (5.4), so signing out everywhere stops it; revoke from the hosting app (broker binding) **and** from the desktop (delete the device key, which is the real cut, since the broker is not trusted) |
| Unwanted typing | Typing needs a paired device, a live session and the terminal's explicit lease; the lease holder is shown on every client and the desktop takes it back with one click (`daemon.md:161-164`); late input is fenced (`daemon.md:162`); "view only" devices never get a lease |
| Tokens in URLs, logs | None: auth in headers and the Noise layer; logs hold sizes and codes only (`daemon.md:180`, `185`), at the relay too (7.1) |
| Malformed or hostile input | Typed errors, size caps and no JSON before authentication (6.2); parsing is fuzzed in the protocol gate |
| Cross-user routing | Broker checks `(user, host_id)` on every join; a misrouted client fails the handshake because the allowlist is by static key |
| Host exposure | The host only dials out; there is no inbound port and the Unix socket stays private (`daemon.md:186-187`). Off by default (3.2) |

### 8.2 If the relay is compromised

| What the attacker controls | What they get |
| --- | --- |
| The broker only, passive (logs, DB dump) | Account↔host bindings, names, public keys, times, IPs, traffic volume. No content |
| The broker, active | Denial of service; dropped or delayed frames; a failed or refused handshake; close codes, which are not authenticated, so it can stop a browser with a false `not_paired`. No content, no forged commands: the host drops any frame that is not valid Noise from an allowlisted key. No lost pairings: a browser forgets its key only on the host's refusal sealed into the channel, which the broker cannot make |
| The hosting app's session store | Attackers can mint connect tickets, but still lack the phone's static key, so the handshake fails |
| The hosting app's origin (XSS or a modified app) | With the dedicated origin: the boot object and connect tickets, which are useless without a device key. The boot also names the host and broker, so a pairing boot can point the frame at a host the attacker runs. What stops that is the user: the frame asks before it replaces the key of a host it already knows, and a pairing with any other host shows a code that no prompt on the user's own computer matches. A user who does not check that is paired with the attacker's host. Same-origin: everything the device sees and may type |
| The ATOMICS bundle | Everything that device views and anything it may type. This is 6.4's limit. Defence: signed, hash-pinned bundles and a native or direct client for the cautious |
| A phone or its browser profile | What the user can do from it, while its session on the hosting app lasts. The desktop can revoke it at once |

Not defended: a compromised ATOMICS host, and malware on the developer machine.

## 9. Phases: one release, three streams

The user's decisions make release 1 the whole product: board, plans, diffs, approvals and steering, and live
interactive terminals, with pairing, the broker and the switcher entry. All work happens on the feature branches
(section 10); the milestones below are internal and each has a gate on the branch. Efforts are agent-days of focused
work, excluding review wait.

**Not in release 1** (Phase F, later): the container profile, signed-bundle verification in the
loader (release 1 has hash pinning), non-extractable browser keys, short-code pairing, the device-code fallback for
hosts without OAuth. The headless host itself was brought forward on 2026-11-02 so the host can run in the NixOS
VM: `atomics host` (`docs/daemon.md`, "Headless host").

### Phase 0: spikes (about 2 days; blocks stream C's terminal input work)

Reads are done (sections 0, 1.2).

| Repo | Task | Acceptance |
| --- | --- | --- |
| ATOMICS | eframe wasm32 build of a minimal UI with `wgpu` and with `glow`; sizes; `snow` building for wasm32; **iOS Safari soft keyboard, IME and dictation into a terminal-like egui widget** | Sizes per renderer, a go/no-go on the egui keyboard vs the `<textarea>` fallback. **Done:** the `<textarea>` overlay (4.4, "Spike results") |
| The hosting app, on an iPhone | Camera-app QR opens Safari, not the PWA; in-PWA `getUserMedia` scanner; IndexedDB survival in the installed PWA, including a same-site iframe | Confirms or corrects 4.4's pairing rule |
| The relay | Measure WebSocket idle behaviour through its ingress with a test route (no product change) | The idle close in seconds, or "none" |

### Stream H: the ATOMICS host (about 30 days)

- **H1, protocol core (8):** `atomics-core` wire types and `v1` envelope (browser-safe); projections of sessions,
  plans, approvals and diffs published as `Arc` snapshots; the protocol server thread (off by default); `Link` with
  the loopback WebSocket transport; `hello`/`welcome`, cursors, `resync`, bounded queues, typed errors; fuzz and
  conformance tests; the rendezvous spec.
- **H2, terminal streaming (8):** screen snapshots under the short emulator lock, deltas with generation and base
  revision, history, eviction, slow-client handling (`daemon.md:112-134`).
- **H3, input (4):** `terminal_input`/`terminal_resize`, controller leases, takeover, fencing tokens
  (`daemon.md:109`, `136-139`, `161-164`).
- **H4, mutations (6):** `approval_decide`, `plan_command`, `review_command`, `session_command`; operation IDs and the
  journal with uncertain-crash outcomes (`daemon.md:103-108`).
- **H5, rendezvous (4):** the transport (blocking `tungstenite` on its own thread, consistent with `relay.rs:9-11`);
  device keys in the credential store; register and pass as profile operations; Noise; device allowlist; the pairing
  UI with QR, link and the six-digit code.

### Stream C: the ATOMICS client (about 30-40 days; the critical path)

Starts on day one, in parallel with H1, against an in-process `Backend` first.

- **C1, extraction (10-15):** the `Backend` trait in small mechanical slices (10.2); a web UI crate free of desktop
  dependencies, with its dependency test.
- **C2, board, plans, approvals and diffs on `Backend` (5-8).**
- **C3, the shared cell-snapshot terminal painter (6-8):** used by the desktop as well; selection, wide and
  combining cells, cursor shapes.
- **C4, mobile UX mode (6-8):** 4.4 in full: compact layout, key bar, touch, pan and zoom, lease indicator, the
  `<textarea>` overlay chosen in Phase 0 (4.4, "Spike results").
- **C5, the browser transport and the `atomics-web` artifact (2).**

### Stream R: the hosting side (about 13-16 days)

The broker (5.2) and the `register` and `pass` operations; connect-ticket, host-list and revoke endpoints; the
limits of 5.5 and session revalidation; an audit log without content; the pages that mount the client and pair a
device, with their headers, the entry point of 4.5 and the bundle pin; and the deployment, including the dedicated
origin if chosen. Relay plans and tracks that work in its own repositories.

### Milestones on the branches

| Milestone | Needs | Gate |
| --- | --- | --- |
| **M1 loopback** | H1, C1 started | A native loopback client over the protocol shows the board and a diff; malformed, version-mismatched and oversize messages rejected; reconnect replays or resets; a slow reader never blocks `Host` (`daemon.md:270-283` tests keep passing); no `State` lock across I/O |
| **M2 tunnel** | H5, the broker | A phone pairs through the broker in the test environment (10.5): unauthenticated and wrong-Origin upgrades fail; a replayed ticket or pass fails; an ended session or revoked OAuth client closes its channel; a connection without an owner cannot register; a wrong key or a mismatched six-digit code fails; the broker stores and logs only ciphertext; two hosts per user stay separate |
| **M3 web board** | C1, C2, C5, the hosting pages | The egui web client shows the board, plans, approvals and diffs on a phone and a desktop browser; two clients and reconnect restore state (`daemon.md:224-228`) |
| **M4 live terminals** | H2, H3, C3, C4 | Snapshots, deltas and typing from a phone: the terminal parity list (`daemon.md:224-227`: wide and combining cells, colours, wrapping, cursor, resize, alternate screen, scrollback and copy); a persistent slow client is disconnected; a dropped delta forces a snapshot; an expired lease fences late input; input is never replayed across a disconnect (`daemon.md:109`); the phone does not resize the desktop's PTY unless asked |
| **M5 mutations** | H4 | Two clients race an approval and exactly one wins; a stale approval cannot approve the next request (`daemon.md:165-166`); a retry returns the known outcome; a view-only device cannot mutate or type |
| **M6 release** | all | The regression gate and the ordered final merge (10.3, 10.4) |

### Totals

About 75-90 agent-days in all (H 30, C 30-40, R 13-16, Phase 0 and M6 about 7), which is the draft's 14-18
weeks that the user accepted. Running H, C and R in parallel, the calendar is set by stream C: about 8-11 weeks to
M5, then about a week for the regression gate and the merges, plus review wait.

## 10. Branching and the regression gate (decision 8)

### 10.1 Branches

| Repo | Feature branch | Notes |
| --- | --- | --- |
| ATOMICS | `relay/<ticket>-embedded-atomics` | Every plan's integration branch for this ticket is this branch, never `main` |
| The hosting product's repositories | `relay/<ticket>-embedded-atomics` in each | Relay's own plan; its deployment repository merges last, because it is what runs |

Sub-tickets get short work branches cut from the feature branch, with reviewed PRs **into the feature branch**; CI
runs on those PRs as on `main` PRs. To respect the build server's budget, an area's sub-tickets are batched per PR,
and long browser or visual suites run locally, not in CI. Fixes for other tickets keep going to `main` as usual.

### 10.2 Keeping the branches current

- **Merge `main` into each feature branch, never rebase.** The branches are long-lived and shared by many work
  branches; a rebase would rewrite what they are cut from, and force-pushes are not allowed. Each merge is a
  normal commit with a reviewed conflict resolution.
- **How often:** at least twice a week, before every milestone gate, and the same day for anything that changes a
  shared contract: an MCP tool or profile format change in ATOMICS, or a change on the hosting side to the contract
  of 7.1.
- **ATOMICS during the extraction (C1)** is the hot spot: `app.rs` and `app/*.rs` change on `main` almost daily, and
  the extraction touches every reader. So the extraction goes in small mechanical slices, each starting from a fresh
  merge of `main`; during C1 `main` is merged daily; and large UI changes on `main` are announced so they can be
  merged in right away rather than late.
- **After each merge** the feature branch's CI must be green before new work lands on it, so a break is always
  traceable to one merge.
- **Dogfooding:** the branch build runs with an isolated `ATOMICS_HOME`, `ATOMICS_SOCKET` and `ATOMICS_CONFIG`
  (`daemon.md:285-287`), never against the instance the user works in (`daemon.md:236`).

### 10.3 The regression gate before the final merge

The gate runs after a final merge of `main` into every feature branch, on the merged result. Everything must pass;
a failure goes back to the branch, never to `main`.

| Repo | Gate |
| --- | --- |
| ATOMICS | Full test suite, including the production-thread gates in `app::host_tests` and the terminal regressions (`daemon.md:270-283`); the browser-core platform test (`platform_tests.rs:14-24`) and the web crate's wasm32 build and dependency test; the no-Relay-names test; clippy. **With remote off (no `[remote]` config) the desktop opens no socket and starts no thread**, and the queued native QA (`daemon.md:285-287`) passes. With one remote client attached, runtime deadlines and frame times on the desktop are no worse than on `main` (measured). The Relay remote control still works (the operator steps, `docs/tickets.md:1060-1071`, and `docs/relay-device-checklist.md`) |
| The hosting side | Its own CI and checks; the contract of 7.1 exercised against the ATOMICS host of the feature branch |
| End to end, on devices | An installed iPhone PWA, an Android phone and a desktop browser: pair (QR, link, code check), board, plan, diff, approve, steer, type in a terminal, take the lease back on the desktop, revoke from each side, reconnect after a network switch, a slow client, a host restart (new epoch, reset), a broker restart |
| Security | The M2 gate list again, on the release candidate; the Relay-sees table of 6.3 checked against the broker's database and logs |

### 10.4 The final merge, in order

Each step is a PR from the feature branch to `main`, merged by the operator,
and each leaves production working if the next step is delayed:

1. **ATOMICS.** Inert without a `[remote]` configuration. Cut a release; it publishes the `atomics-web` artifact.
2. **The hosting side**, in its own order: the broker and the operations first (inert until routed), then the
   deployment, and last the pages and the entry point, which is the step users see.

Rollback goes in reverse: hiding the entry point removes the feature for users at once; the broker can then be
stopped; the ATOMICS feature is off by default.

### 10.5 Testing before anything merges

Since nothing reaches `main`, device testing needs an environment that runs the branches. Recommended: a local stack
of the hosting side's feature branches, reached from phones over a tailnet, for development and M1-M5; and, for the
gate's device runs, a preview deployment of the branch images, which is the hosting side's decision.

## 11. Rejected at the top level

- **Making the remote control's conversation the whole of this ticket**: it already exists, and it is not private.
- **Put the terminal bytes through a Relay-side MCP tool.** Wrong transport (polled, posted as text, cleaner-applied,
  non-private).
- **Tailnet or auth proxy only.** It remains supported, but it is not the "zero-setup" Relay route the user chose.
- **A Flat-native client** (4.1), and **merging phases to `main` behind flags** (the user's decision 4).

## 12. Open questions for the user

Answered on 2026-11-02 and folded in above: typing from a phone (yes), live terminals in release 1 (yes), full scope
(yes), one feature branch per repo with nothing on `main` until it all works (yes). Remaining:

- **1. Iframe origin.** Run the ATOMICS iframe on a dedicated origin, or on the hosting app's own origin?
   **Recommended: the dedicated origin.** Typing ships in release 1, and on a shared origin any XSS in the hosting
   app could read terminals and type into them. It costs the hosting side a name, a certificate and a route.
- **3. Pairing link or short code.** **Recommended: the link**, as a QR scanned inside the installed PWA or pasted, with
   the six-digit code compared on both screens. A typed short code needs a PAKE and is deferred to Phase F.
- **5. Host auth.** **Recommended: both.** The personal-agent OAuth connection registers the host and fetches passes
   through two profile operations; the device key signs and is what phones pin. This means release 1 needs the
   ticket connection on the host; the device-code fallback is deferred. Confirm?
- **8. Where the host runs.** The first host is the desktop app. Does the headless host (Phase F) need to come earlier
   for your use? **Answered 2026-11-02: yes**, for the NixOS VM. `atomics host` runs the runtime, the local socket
   and (with `[remote]`) the protocol thread with no window; the desktop attaching to it as a client is still phase 3.

Questions 2, 4, 6 and 7 concerned Relay's side (artifact pinning, broker placement, its switcher change and a preview
environment) and are kept with Relay.

## 13. Review (2026-11-02)

Each finding names where it is fixed in this document. The user's later decisions are noted where they
change a finding.

### HIGH

- **H1. Host auth rested on a factual error.** The draft said the user's connection is a product agent with no
  owner, citing the remote-control review's Q1. ATOMICS signs in as the user's personal agent (`kind: personalAgent`,
  `ownerUserId` = the user; `docs/tickets.md:1040-1049`). The rejected option was the right binding. Re-decided as
  "both" in 5.3: OAuth proves the human through profile operations (the token's audience is the MCP resource), and
  the device key authenticates the tunnel and is pinned by phones. The QR's claim secret is gone (6.1).
- **H2. The UI extraction is the critical path, and the draft started it late.** It is 41,803 lines whose draw code
  is `impl State` in 37 files, a terminal painter that reads alacritty's `Term`, and a C-based `syntect` that will not
  build for wasm32 (1.1). The review first proposed a Flat-native stepping stone. The user then put interactive
  terminals in release 1 and kept everything off `main` until it all works, so a stepping stone would ship nothing
  sooner. Fixed in 9: the extraction (stream C) starts on day one in parallel with the protocol, and its slicing is
  planned around the long-lived branch (10.2).
- **H3. Same-origin keys meant any XSS in the hosting app reads everything, and now types too.** The phone's Noise
  key and decrypted frames would live in the hosting app's origin, and the key is extractable (6.2). With typing in
  release 1 (the user's decision 1), that would be terminal access. Recommended fix: a dedicated origin in release 1
  (4.2, open question 1). The cookie objection falls away because the hosting app passes the single-use connect
  ticket by `postMessage`.

### MEDIUM

- **M2. iOS pairing by QR lands in the wrong storage.** The Camera app opens Safari, whose storage is separate from
  the installed PWA, and Safari may clear an unused site's storage after 7 days. Pair inside the PWA; the scanner
  needs the camera allowed on the pairing page. Fixed in 4.4, spiked in Phase 0.
- **M7. A leaked or read pairing link could add a device.** The draft relied on the desktop prompt alone, but a
  script that reads the fragment can race the phone and choose a believable device name. Fixed in 6.1: a six-digit
  code from the handshake hash on both screens, and the fragment removed from the address bar at once.
- **M8. The long-lived branch makes the extraction a merge problem.** ATOMICS's UI changes on `main` almost daily
  while stream C rewrites every reader. Fixed in 10.2: mechanical slices, a fresh merge of `main` before each, daily
  merges during C1.

### LOW

- **L1. The WebGPU+WebGL2 spike is moot.** eframe 0.37's default `wgpu` already compiles both backends into one bundle
  (REG feature lists, 4.1); `glow` is the size alternative. Fix `daemon.md:5`'s "WebGPU" wording when the web crate
  lands.
- **L4. A phone resizing a shared PTY rewraps the desktop's terminal.** `daemon.md:139` gives resizing to the lease
  holder. Fixed in 4.4: a phone pans and zooms by default and resizes only on request.

### NIT

- **N1.** Section references pointed at "section 8" for Phase 0 and at a nonexistent "A4". Fixed with the new
  section 9.
- **N3.** Pronouns for the user replaced with neutral wording (3.2).
- **N4.** `host.rs` line citations adjusted to the struct and `start` (`host.rs:13-42`).

Findings M1, M3, M4, M5, M6, L2, L3, L5 and N2 concerned Relay's side (its switcher entry, session cookie, broker
replicas and placement, bundle delivery, rate limits, headers and access rules, and a citation) and are kept with
Relay.
