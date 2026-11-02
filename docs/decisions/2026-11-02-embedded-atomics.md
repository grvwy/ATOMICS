# Embedded ATOMICS through Relay (RELAY-11973)

Design round, 2026-11-02. No code. Decided by Max on 2026-11-02 ("let us go for that, behind the application
switcher"): the full ATOMICS egui UI runs inside Relay, opened from Flat's application switcher. The ATOMICS
host on the developer's machine dials out to Relay, which pairs it with the user's session and forwards only
encrypted traffic. ATOMICS stays agnostic: Relay is one hosted option beside the tailnet and auth-proxy routes.

## 0. How this was researched, and what is not verified

**Only the ATOMICS repo was available in this sandbox.** relay-spatial, relay-backend, foundation-template and
platform-gitops were not checked out and could not be fetched (no remote, no GitHub credentials). So:

- **ATOMICS** citations (`docs/daemon.md:N`, `crates/...:N`) were read for this document, on the commit
  this branch is based on (`49cc657`).
- **Relay-side** citations marked **[R]** are carried over from the RELAY-11972 design and review
  (`docs/decisions/2026-11-01-relay-remote-control.md`, `...-review.md`). Those were read at foundation-template
  `44128b5e` (**FT**, the kernel), relay-backend `089e68c8d` (**RB**), relay-spatial `279e8d8` (**RS**, Flat in
  `next/`) and platform-gitops `origin/main` (**GO**). I did not re-read them, so they may have moved.
- Things the task asked for that **no document I could read covers** are marked **[UNVERIFIED]**: Flat's
  `apps.rs` and the switcher, how integration-web serves the MCP and OAuth endpoints beyond what [R] records, the
  relay ingress manifests in platform-gitops, and whether the kernel has any WebSocket or streaming host.
  Each has a Phase 0 check in section 8 that must pass before the dependent decision is built. The design is
  written so that a failed check changes one component, not the shape.

## 1. Where things stand

**ATOMICS: phase 2 is complete and phase 3 has not started.** `host::Host` owns the runtime on the
`atomics-runtime` thread (`docs/daemon.md:238-241`, `crates/atomics/src/host.rs:15-20`). It is still in-process
and still takes an `egui::Context` (`host.rs:25`). The desktop borrows the whole model under one mutex
(`host.rs:44-48`; `daemon.md:252-253`), and exiting the app still joins the host and shuts the terminals down
(`daemon.md:317-319`). The "Remaining for phase 3" list (`daemon.md:306-319`) is untouched: no headless start, no
projections or snapshots, no WebSocket server, no journal. `Cargo.lock` has no WebSocket, AEAD or Noise crate;
`ring` is present (PKCE, `mcp_client/oauth.rs:1-9`), `tokio`, `wgpu` and `web-sys` only transitively through
eframe. Phase 1 holds only for the core crate: `atomics-core` builds for wasm32 with `--no-default-features`, and a
test enforces it (`crates/atomics-core/src/platform_tests.rs:14-24`). The UI crate does not: `app.rs`, `app/*.rs`,
`terminal*` and `diff_view.rs` are about 41,800 lines that read `State` directly (`crates/atomics/src/app.rs:441`).
That coupling is why phase 4 is larger than the ticket's "1–2 weeks" (section 8).

**The protocol is specified, not built.** `daemon.md:67-110` proposes `atomics.v1` over a same-origin WebSocket with
JSON envelopes, cursors, `ack`/`resync`, operation IDs and journaling. `daemon.md:112-145` covers terminal
snapshots and deltas, and `daemon.md:147-167` covers reconnect and control leases. This design adopts that
protocol and adds only a session layer under it (section 5).

**RELAY-11972 is live and already covers the everyday phone surface** (chief conversation, steering, `/approve`):
a polled, ticket-hosted conversation through MCP tools (`docs/tickets.md:837-985`, `988-1073`). It is not
end-to-end private: the mirror posts the chief's turns to Relay (`tickets.md:864-866`). Embedded ATOMICS adds what
that cannot: terminals, diffs, the board, and a private channel.

**ATOMICS already uses the word "relay" for something else**: the host credential relay (`crates/atomics/src/relay.rs:1-11`).
New modules therefore use "tunnel" and "rendezvous", never "relay", which also serves requirement 5.

## 2. Decisions at a glance

| # | Decision | Chosen | Main rejected |
| --- | --- | --- | --- |
| 1 | Protocol | `atomics.v1` from `daemon.md`, built in slices: read-only state and diffs first, terminals as snapshots, input and leases last. Served first by an in-process protocol thread in the running desktop app | Waiting for the headless host; a Relay-specific protocol; raw ANSI streaming |
| 2 | Client | One wasm bundle per protocol major, in a same-origin iframe on a Flat route; wgpu with WebGPU and WebGL2 in one bundle | Canvas inside Flat's wasm; a Leptos-native client; a separate origin |
| 3 | Tunnel | A new broker service in relay-backend; hosts authenticate with a device key claimed by the user's PWA session | integration-web; the kernel; reusing the OAuth agent identity |
| 4 | E2E | Noise IK over the tunnel, paired by QR or link whose secret is in the URL fragment | TLS only; hand-rolled ECDH; a PAKE-only scheme |
| 5 | Agnostic | A `Link` abstraction with direct and rendezvous transports; same client, same inner protocol | A Relay-aware client |
| 6 | Security | Origin, single-use connect ticket, per-device roles, revocation on both sides, typing needs a local grant | Treating Relay login as sufficient for typing |

## 3. Protocol (decision 1)

### 3.1 Use `daemon.md`'s protocol; change its order, not its content

The wire (tags, cursors, epochs, operation IDs, bounded queues) is as proposed at `daemon.md:67-110`. The only
changes are the build order and one carrier decision.

**Smallest useful first slice: read-only board, diffs and plans (call it P3a).** Messages:
`hello`/`welcome`, `state_snapshot`, `session_upsert`/`removed`, `plan_upsert`/`removed`, `task_report`,
`approval_pending`/`resolved`, `diff_summary`/`diff_file`/`file_content` with `diff_get`/`file_get`, and
`ack`/`resync`/`ping`/`pong`/`error` (`daemon.md:76-95`). The role is `viewer` (`daemon.md:183`).

Why this slice first:

- **It is nearly stateless.** Everything is revision-addressed snapshots: no deltas, no leases, no input
  ordering. Reconnect is "send a snapshot at cursor N" (`daemon.md:152`). The hard parts the ticket names
  (terminal deltas, slow clients; `RELAY-11973` description) are not in it.
- **It uses code that exists.** `WorktreeDiff`, `Plan`, `AgentStatus` and `Hunk` are in the browser-safe core
  (`daemon.md:53-55`). Highlight spans are already byte ranges with plain RGBA (`daemon.md:263-268`).
- **It is the part 11972 cannot do.** Diffs and the plan board are what Max cannot review from the
  conversation today.

**P3b adds mutations that are small and race-safe:** `approval_decide` (first valid decision wins,
`daemon.md:165-166`), `plan_command` approve, `review_command` comments, and `session_command` steering. These
carry operation IDs and the journal (`daemon.md:103-108`). They ship before terminals because their semantics
are already written down and they are the actions Max most often needs from a phone.

**P3c adds terminals in three steps:**

1. `screen_snapshot` only, full snapshots on the existing 100/200/500 ms cadence (`daemon.md:127-128`,
   `260-261`), read-only. An 80×24 screen is a few KB as JSON before compression; snapshot-only is a sound
   first version and removes the base-revision bookkeeping.
2. `screen_delta` with generation and base revision, `history_get`/`history_chunk`
   (`daemon.md:121-125`) once measured.
3. `terminal_input`/`terminal_resize` with the controller lease and fencing token (`daemon.md:109`, `139`,
   `161-164`). This is last because it is the first slice that lets a stolen device act.

### 3.2 Serve it first from the running desktop app

`daemon.md:306-319` makes phase 3 a separate headless host. For Relay that is the wrong first move. Max's host
today is the desktop app, running while he is away; a headless host needs shutdown ownership changed
(`daemon.md:317-319`) and an egui-free service construction (`daemon.md:308-309`), both invasive.

**Decision:** P3a-P3c ship as a protocol server thread inside the desktop process. It takes the `State` mutex
(`host.rs:44-48`) only long enough to copy a projection (a snapshot struct built from plain data), then
encodes and sends outside the lock, as `daemon.md:129` requires for terminals. The protocol, projections, journal
and leases are the same ones the headless host needs, so the headless split (phase 3 proper) becomes moving that
thread behind `Host::start` without an `egui::Context`, not a redesign. The cost is lock contention with
rendering; the mitigation is that projections are built on the runtime thread at its existing deadlines and
published as immutable `Arc` snapshots, so the protocol thread never touches `State`.

Rejected: **wait for the headless host first** (adds weeks before anything is useful; the ticket's own estimate is
1-2 weeks for phase 3 with the hardest part being what we defer anyway). Rejected: **raw ANSI over the wire**
(contradicts `daemon.md:114-115`, and the client would need a second emulator).

### 3.3 Carrier

JSON text frames, as `daemon.md:70` says to start. Measure before binary or compression (`daemon.md:134`).
Frames are capped at 256 KiB before encryption; snapshots, diffs and history are chunked with transfer IDs
(`daemon.md:133`).

## 4. Client (decision 2)

### 4.1 One wgpu bundle, egui on wasm32

The client is the existing egui UI compiled to wasm32 via eframe's web backend with wgpu, enabling both WebGPU
and WebGL2 so one bundle falls back at runtime. `daemon.md:5` only promises WebGPU; iOS and older Android
browsers need the fallback, which is why this design requires it. **[UNVERIFIED in this sandbox]** wgpu's
`webgpu` plus `webgl` features coexisting in one eframe 0.37 build, and the resulting bundle size (expect several
MB before compression); both are Phase 0 spike items.

The UI must first be separated from `State`: a `Backend` trait with two implementations (in-process for the
desktop, protocol for the browser and for a native client over loopback). That is `daemon.md:36-46`'s client
list and the largest piece of work in the project (section 8, A4).

### 4.2 Embedded how: an iframe on a Flat route

Flat is Leptos compiled to wasm and served as a static site **[R]** (RS `next/crates/relay-flat/index.html:1-30`;
check command `cargo check -p relay-flat --target wasm32-unknown-unknown`, review doc `...-remote-control.md:1026`).
Options:

| Option | Verdict |
| --- | --- |
| **Same-origin iframe on a Flat route** (`/atomics` hosts `/atomics/v/<major>/index.html`) | **Chosen.** Two wasm apps never share a canvas, a panic domain, a keyboard or an IME. The iframe owns pointer capture, focus, gestures and the soft keyboard, which is where egui-on-mobile is weakest. Its own CSP and cache policy. Flat only decides when to mount it |
| A canvas inside Flat's page and wasm module | Rejected. Two event loops, shared focus and key handling, one bad panic takes down the shell; Flat would have to link ATOMICS's UI |
| A Leptos-native client over the same protocol | Rejected for now. A DOM client would get text selection, IME and accessibility for free, but it duplicates the terminal grid, diff viewer, board and chief docking that Max asked for as "the full ATOMICS UI", and doubles the UI to maintain. Revisit only if the Phase 0 mobile text-input spike fails (section 8) |
| A separate origin (`atomics.<relay domain>`) | Rejected for v1: host-only session cookies would not reach it, and Relay serves the code either way. It would isolate keys from a Flat XSS, so it stays a hardening option (open question 5) |

Flat gives the iframe a boot object by `postMessage` after load (mode, host list, theme, locale), never the
session cookie or a token: the iframe authenticates with the same-origin cookie by itself (section 6).

### 4.3 Asset hosting and versioning

- ATOMICS CI builds `atomics-web-<version>` with content-hashed filenames and a manifest
  `{atomics_version, protocol_major, protocol_minor_max, wasm_sha256, built_from}`. It is a release artifact of
  the ATOMICS repo (MIT, public), with no Relay content in it.
- Relay serves **one bundle per protocol major**, under `/atomics/v/<major>/`, immutable and long-cached,
  with the loader page `no-cache`. This follows the existing split of Caddy rules **[R]** (RS `deploy/Caddyfile`
  `@mutable` at `:79-80`). The wasm needs `script-src 'wasm-unsafe-eval'` for the iframe only. No threads, so no
  COOP/COEP.
- **Versioning against the host:** minors are additive and capability-gated; a major mismatch fails before
  subscription (`daemon.md:97`). So the client needs no per-host version matching other than the major. The
  broker's plaintext presence record carries the host's protocol major, so Flat picks the right
  `/atomics/v/<major>/` before connecting. A host on an unserved major shows "update ATOMICS" or "open on the
  desktop".
- How the bundle gets into Relay: a release workflow in relay-spatial pulls a pinned `atomics-web` artifact by
  version and hash and commits it (or gitops fetches it at deploy). Pinning is a reviewed PR, not an automatic pull
  from the ATOMICS repo, so a compromised ATOMICS release does not reach users unreviewed. **[UNVERIFIED]** which
  of the two fits relay-spatial's CI; see open question 4.
- Rejected: **serve the client from the host through the tunnel.** It keeps client and host versions identical
  by construction, but code would arrive before any key is established, from a peer the user has not yet
  authenticated. The shell must come from the origin the user trusts.

### 4.4 Phone layout

As Max's comment says: no new screens, a mobile UX mode (RELAY-11973, comment of 2026-10-31). Concretely:

- Mode choice from viewport width and `pointer: coarse`, with a manual override. The UI is already screen-aware
  (Max's comment; the top bar was tested at 480/550 points, `daemon.md:292-294`).
- Touch targets and gestures replacing hover and keyboard-only paths; compact single-pane navigation between
  board, a conversation and a diff.
- An on-screen key bar (Esc, Ctrl, Tab, arrows, `|`, `~`, `/`) for terminal control keys. Terminal mouse reports
  come from cell coordinates (`daemon.md:137-138`).
- Soft keyboard and IME in an egui canvas are the main risk. eframe's web backend uses a hidden text input for
  this; its behaviour on iOS is **[UNVERIFIED]** and is spiked first (section 8, Phase 0). A fallback is a
  native `<textarea>` in the iframe that forwards composed text as `terminal_input`.
- The PWA offline state: the client shows cached content as disconnected and disables mutations until
  synchronised (`daemon.md:150`). There is no offline shell; Flat's worker has no `fetch` handler by design
  **[R]** (`...-remote-control.md:655-665`), and the iframe does not change that.

### 4.5 The switcher

RELAY-11985 added the ATOMICS entry, which today opens `atomics://`. **[UNVERIFIED]**: Flat's `apps.rs` and
`chrome.rs` switcher were not readable here; only `chrome.rs:193` (the bell menu) is known **[R]**. Behaviour:

- No paired host: the entry opens the pairing page, with "Open the desktop app" (`atomics://`) as a second action on
  desktop. That keeps the fallback the task requires.
- One paired host: open the embedded client on it. Several: a host picker.
- Always available: "Open in desktop app" in the client menu on desktop.

11972's "Open in ATOMICS" button (Max's comment of 2026-10-31) deep-links to `/atomics?host=<id>&view=chief`.

## 5. Tunnel (decision 3)

### 5.1 Shape

```
ATOMICS host ──wss (dial out)──▶ broker ◀──wss── PWA iframe
     │  Noise-encrypted frames pass through unchanged  │
     └────────────── Noise IK end to end ──────────────┘
```

The broker pairs sockets, enforces auth and quotas, and relays opaque binary frames. Every frame has a small
plaintext header `{kind: data|ctrl, channel}`. The broker assigns each browser connection a `channel`; the host
sees `joined(channel)` and `left(channel)` control frames and runs one Noise handshake per channel. Those control
frames are untrusted hints: a forged `joined` yields a failed handshake, nothing more.

### 5.2 Which service owns the broker

| Option | Verdict |
| --- | --- |
| **A new broker service in relay-backend** | **Chosen.** Long-lived sockets should not share pods, rollouts and rate limits with the MCP and OAuth surface; a rollout there would drop every tunnel. The broker is small (pairing table, queues, auth) and Relay-product-specific, which suits relay-backend rather than the generic kernel. It needs no ATOM ticket unless token validation needs a kernel hook (below) |
| integration-web (MCP and OAuth host) | Rejected. `IntegrationWebApiModule.cs` is kernel-owned and byte-identical in FT and RB, locked by `kernel.lock` **[R]** (review `M1`); adding sockets means a kernel ticket and a shared rollout. MCP has no rate limiter today **[R]** (`IntegrationWebApiModule.cs:116-150, 401-402`) |
| The kernel (foundation-template) | Rejected. A generic kernel should not carry an ATOMICS-specific tunnel, and kernel work needs an ATOM ticket on an `atom/` branch **[R]** |

**[UNVERIFIED]** Whether the kernel already has a WebSocket or streaming host (the assistant streaming host the
task mentions) that the broker could reuse, and how relay ingress treats WebSocket upgrades and idle timeouts.
Phase 0 checks both. Replica routing is the main operational design question: a host on one pod and its client
on another need either ingress affinity keyed on `host_id` or a small shared bus. Start with one replica and
affinity, since a tunnel session is cheap to re-establish (the client reconnects and gets a snapshot).

### 5.3 Authentication: a device key, claimed by the human's session

Do **not** use the OAuth agent identity for the tunnel, although ATOMICS already holds one
(`mcp_client/oauth.rs:1-9`). The review fixed that the connection may be a product agent with no owner:
Max signs ATOMICS in as Max Claude, `kind: "agent"`, no `AgentOwnerUserId` **[R]** (review Q1, `whoami` at RB
`TicketIntegrationOperations.cs:1570-1595`; `docs/tickets.md:1047-1050`). That identity does not say which human
the host belongs to. The tunnel needs exactly that binding.

**Chosen: a device credential, claimed by a signed-in person (a device-code style claim).**

1. The host creates an Ed25519 device key and an X25519 static key, kept in the credential store
   (`crates/atomics/src/credentials.rs`; never on disk in plain text).
2. Settings → "Phone and browser access" (name provisional) registers the host with the broker: `host_id`, the
   device public key, the host's display name, and `H(claim_secret)`. The host shows a QR and link (section 6).
3. The PWA, with the person's Relay session, posts `claim(host_id, claim_secret)` with antiforgery. The broker
   binds `host_id` to that user and deletes the claim. A host is bound to exactly one user.
4. Afterwards the host authenticates by signing a broker challenge for a short-lived (5 minute) tunnel token, sent
   as an `Authorization` header on the upgrade, never in the URL (`daemon.md:180`).

Reasons: it needs no OAuth scope or consent change in the kernel, it works with a product agent, and the human
identity comes from the one thing that proves it, the person's own session at claim time. Revoking is deleting the
broker binding.

Rejected: **OAuth personal agent as the tunnel identity.** It would be the natural reuse and the ticket
suggests it, but personal-agent OAuth is a per-owner agent, and the owner list is separate from the connection
identity in Max's setup. It would also need a new scope minted by the kernel. Rejected: **a long-lived bearer token
pasted into the host.** Theft is silent and it breaks `daemon.md:180-181`.

### 5.4 Pairing a PWA session with the host; many hosts

- Browser side: the iframe calls `POST /api/atomics/connect {host_id}` (cookie, antiforgery) and receives a
  **single-use connect ticket** valid for 30 seconds and bound to the user, host and Relay session id. It opens the
  WebSocket and presents the ticket in the `Sec-WebSocket-Protocol` header (not the URL), so the upgrade itself is
  authenticated, as `daemon.md:171-172` requires. The cookie is `Secure; HttpOnly; SameSite=Strict`.
- Presence: `GET /api/atomics/hosts` lists the user's hosts with name, online state, protocol major and last seen
  (plaintext, user-chosen display name).
- **Several hosts per user:** a table `(user, host_id, name, created, revoked)`; limit 5 hosts per user to start. Each host
  allows several browser channels (`daemon.md:161`; limit 4 at first). A host is not shared between users in v1.

### 5.5 Limits and backpressure

The broker cannot read frames, so it cannot discard superseded deltas the way `daemon.md:130-132` does. Policy is
split:

- **The host applies the semantic policy before encrypting**, per channel: bounded queue, supersede deltas with
  a snapshot, disconnect a persistently slow client, never delay approvals (`daemon.md:130-132`).
- **The broker applies byte and time limits:** frame ≤ 256 KiB; per-channel send buffer ≤ 1 MiB, and a channel
  whose buffer stays full for 10 s is closed (the client reconnects and the host sends a fresh snapshot);
  per-host sustained ≤ 2 MiB/s with a burst of 8 MiB; per-user 5 hosts and 4 channels per host; a pending-claim limit; ping every
  25 s with a 60 s idle close (ingress idle timeouts, **[UNVERIFIED]**); unauthenticated handshakes limited per IP.
- Quotas count bytes only, which suits E2E: the broker never needs content to enforce them.
- Close codes are typed (`unauthorised`, `revoked`, `slow`, `replaced`, `quota`), and the client maps them to
  messages and to "reconnect" or "re-pair" actions.
- Reconnect uses jittered backoff and the epoch/cursor rules of `daemon.md:73`, `151-159`.

## 6. End-to-end encryption and pairing (decision 4)

### 6.1 Pairing

Settings shows a QR code and a text link:

```
https://<relay origin>/atomics/pair#<v>.<host_id>.<host_static_pubkey>.<claim_secret>.<psk>
```

The pairing data sits in the **URL fragment**, which a browser never sends to a server. `claim_secret` goes to the
broker in step 3 of section 5.3 (so the broker can bind the user); `psk` and `host_static_pubkey` never go to the
broker. The secrets are single-use, expire in 5 minutes, and the host refuses a second pairing until the first
completes or expires. A phone without a camera (an iPad, a desktop browser) uses the link: copy it, AirDrop or
message it to yourself, and open it. Typing a short code is deferred: a short code cannot carry a public key without a PAKE and a
numeric comparison, and that is more design than v1 deserves (rejected for now; open question 6).

After the handshake, the host shows "Pair 'iPhone Safari'? Viewer / Controller" **on the desktop**, so a pairing
code that leaked cannot add a device silently. The new device defaults to `viewer` (`daemon.md:183`); typing in
terminals needs `controller`, granted at that prompt or later in Settings.

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
- Libraries: a maintained Noise implementation with pure-Rust primitives that builds for wasm32 (candidate:
  `snow` with RustCrypto), shared between host and client. `ring` is already in the tree for PKCE
  (`mcp_client/oauth.rs`) but its wasm32 support needs a C toolchain, so it is not the default. **[UNVERIFIED]**
  both crates' wasm builds: Phase 0.

Rejected: **TLS terminated at the broker** (Relay would see everything, the opposite of the requirement).
**Hand-rolled ECDH plus AES-GCM** (nonces, rekeying and replay are exactly where hand-rolled schemes fail).
**Per-message HPKE or `age`** (no ordering or replay protection, no sessions). **Signal's double ratchet** (two
parties, one session at a time, no offline queue; too much for the benefit). **PAKE only** (no durable device
identity for revocation).

### 6.3 What Relay can and cannot see

| Relay sees | Relay cannot see |
| --- | --- |
| The user, `host_id`, the host's display name, device-claim times | Terminal contents, prompts, keystrokes |
| Online/offline and connect/disconnect times, source IPs | Diffs, file contents, paths, repo and branch names |
| Protocol major (for bundle choice), channel numbers | Plans, tasks, approvals and their decisions |
| Frame sizes and timing, byte counts (a terminal's activity pattern leaks) | Which device is a viewer or a controller |
| That a pairing happened | The `psk`, host static key use, session keys |

A malicious broker can drop, delay or cut frames, or refuse service. It cannot forge, replay or reorder them.

### 6.4 The honest limit: Relay serves the client code

An end-to-end protocol is only as strong as the code that runs it, and Relay serves that code. A compromised
Relay could serve a modified bundle that exfiltrates what the user sees. So the claim is "Relay is only a pipe for the data path
and a passive or logging-only compromise reveals nothing", not "a fully compromised Relay cannot read anything". Mitigations, in
order:

1. The bundle is a reproducible build with a published hash; releases are signed by an ATOMICS release key.
2. Bundles are pinned by PR (section 4.3), so a change is reviewed, never an automatic pull.
3. A service worker or loader checks the hash against the signed manifest, and asks the user to confirm a major
   bump. **[UNVERIFIED]** how far this holds in Flat's loader given its own worker (`next/web/sw.js`).
4. The strongest option is not to trust the Relay origin: a native client, or the direct route in section 7.
   Both remain supported.

## 7. Agnosticism (decision 5)

- ATOMICS defines a `Link` abstraction (an ordered message duplex with `close(code)`) and two transports:
  **direct** (host serves assets and `wss` at its own origin behind a tailnet ingress or auth proxy, with the
  rules in `daemon.md:169-187`) and **rendezvous** (the host dials out to a configured URL). The wasm client takes
  the transport from its boot object: `direct` or `rendezvous {url, host_id}`.
- The **inner protocol is identical.** Noise wraps it on the rendezvous transport; on direct it is TLS plus the
  proxy's login, per `daemon.md:173-176`. The Noise layer is optional on direct transports so a tailnet user is not
  forced to pair keys; it is a flag in `welcome`.
- Rendezvous is a documented protocol (a one-page spec in the ATOMICS repo): any server that pairs two
  WebSockets and checks a bearer works. Relay's broker is one implementation, and a self-hosted one is possible.
- **No Relay names in ATOMICS code.** Config is `[remote.rendezvous] url = ...`; UI strings say "your account's
  rendezvous service" unless a profile supplies a label. A test greps the new modules for `relay` / `Relay`, the same
  discipline the 11972 work held (names only in tests and the example profile, review `a1`). The Relay example
  profile in `config.rs` may gain the URL as a commented example, nothing else.
- The **desktop `atomics://` fallback** stays (section 4.5).

## 8. Security (decision 6)

### 8.1 Controls

| Threat | Control |
| --- | --- |
| Cross-site WebSocket hijack | Exact `Origin` check on every upgrade (broker and any direct host), cookie `SameSite=Strict`, single-use connect ticket in `Sec-WebSocket-Protocol`; this repeats `daemon.md:178-179` |
| CSRF on connect, claim, revoke | Antiforgery on all mutating endpoints, as the existing Web Push endpoints do **[R]** (FT `WebPushController.cs:38`) |
| Session theft or replay of a ticket | Tickets are single-use, 30 s, bound to user, host and Relay session; the broker re-checks the session periodically (60 s) and on sign-out, closing sockets for revoked sessions (`daemon.md:182`) |
| Replay of frames | Noise counter nonces, per-session ephemeral keys, single-use pairing secrets |
| Stolen or lost phone | Revoke from Flat (broker binding) **and** from the desktop (delete the device key, which is the real cut, since the broker is not trusted); roles; a controller grant expires after 12 h and needs a local re-grant; idle input lock |
| Unattended terminal control | Typing needs `controller`, which only the desktop prompt grants (`daemon.md:183`); leases are explicit, takeovers are visible on the desktop (`daemon.md:162-164`) |
| Tokens in URLs, logs | None: auth in headers and the Noise layer; logs hold sizes and codes only (`daemon.md:180`, `185`) |
| Malformed or hostile input | Typed errors, size caps and no JSON before authentication (section 6.2); parsing is fuzzed in the P3a gate |
| Cross-user routing | Broker checks `(user, host_id)` on every join; a misrouted client fails the handshake because the allowlist is by static key |
| Host exposure | The host only dials out; there is no inbound port and the Unix socket stays private (`daemon.md:186-187`) |

### 8.2 If Relay is compromised

| What the attacker controls | What they get |
| --- | --- |
| The broker only, passive (logs, DB dump) | Account↔host bindings, names, times, IPs, traffic volume. No content |
| The broker, active | Denial of service; dropped or delayed frames; a failed or refused handshake. No content, no forged commands: the host drops any frame that is not valid Noise from an allowlisted key |
| The Relay session store | Attackers can mint connect tickets, but still lack the phone's static key, so the handshake fails |
| Relay's static assets (the bundle) | Everything the user views in that session, and anything a controller device could type. This is section 6.4's limit. Defence: signed, hash-pinned bundles and a native or direct client for the cautious |
| A phone or its browser profile | Its role: viewer reads everything, controller can type. The desktop can revoke it at once |

Not defended: a compromised ATOMICS host, and malware on the developer machine.

## 9. Phases, gates and tasks per repo

Efforts are agent-days of focused work, as the ticket estimated them, unless noted; they exclude review wait. They are
revised upward from the ticket where the code says so: the UI crate is about 41,800 lines tied to `State`
(section 1).

### Phase 0: verification and spikes (about 4 days; blocks the dependent items)

| Repo | Task | Acceptance |
| --- | --- | --- |
| relay-spatial | Read `next/crates/relay-flat/src/apps.rs` and `chrome.rs` (switcher, RELAY-11985); the Caddyfile and CSP headers | The switcher entry, its route and its extension point are written into this doc's section 4.5 |
| relay-backend / foundation-template | Does the kernel have any WebSocket or streaming host? How does the integration host validate bearer tokens? | A paragraph in this doc; decide whether the broker reuses it or stays new (5.2); whether any ATOM ticket is needed |
| platform-gitops | The relay ingress: WebSocket upgrade, idle and body timeouts, affinity | A yes/no on WebSocket through ingress with a measured idle timeout |
| ATOMICS | Spike: eframe wasm32 with wgpu WebGPU+WebGL in one bundle; iOS Safari soft keyboard into a terminal-like widget; bundle size; `snow` and `ring` building for wasm32 | A spike report with sizes, an iOS check on a real device, and a go/no-go on the egui client vs the textarea fallback |

### Phase A: protocol P3a, read-only (ATOMICS, about 8 days, 2-3 weeks including review)

- **ATOMICS:** `atomics-core` wire types and `v1` envelope (browser-safe); projections of sessions, plans, approvals, diffs built on the runtime thread and published as `Arc` snapshots; the protocol server thread in the desktop process; `Link` abstraction with the loopback/direct WebSocket transport; `hello`/`welcome`, cursors, `resync`, bounded queues, typed errors; the `atomics.v1` fuzz and conformance tests; a rendezvous spec document.
- **Gate:** malformed, version-mismatched and oversize messages rejected; reconnect replays or resets; a slow reader is disconnected without blocking `Host` (the runtime thread's tests keep passing, `daemon.md:270-283`); no `State` lock is held across I/O.
- **Ships to Max:** nothing visible yet except a loopback test client.

### Phase B: broker and pairing (relay-backend, about 5 days; ATOMICS about 4 days)

- **relay-backend:** the broker service: host registration and claim, challenge-response tunnel token, connect tickets, presence list, frame relay, limits (5.5), session revalidation, audit log without content; a revoke endpoint.
- **ATOMICS:** the rendezvous transport (blocking `tungstenite` on its own thread, consistent with `relay.rs`'s threads, `relay.rs:9-11`), device keys in the credential store, the pairing UI with QR and link, the Noise layer, device allowlist and roles, the desktop confirmation prompt.
- **platform-gitops:** the broker deployment, ingress route and WebSocket timeouts and secrets.
- **relay-spatial:** `/atomics/pair`, the claim call, a minimal "Hosts" list.
- **Gate:** unauthenticated and wrong-Origin upgrades fail; a replayed ticket fails; a revoked session closes its socket; a wrong key fails the handshake; the broker sees only ciphertext (a test inspects the stored/log output); two hosts per user stay separate.
- **Acceptance check on the device:** pair a phone by QR and by link, see it connect and the desktop prompt.

### Phase C: web client, read-only (ATOMICS about 15-25 days; relay-spatial about 4 days)

- **ATOMICS:** extract the `Backend` trait and a UI crate that builds for wasm32; browser transport; read-only board, plans, approvals list and diffs; `atomics-web` CI artifact and manifest; mobile UX mode (single pane, touch targets).
- **relay-spatial:** the `/atomics` route, the iframe mount with the boot object, per-major bundle hosting, CSP headers, the switcher behaviour of 4.5, and the bundle pin workflow.
- **Gate:** the `daemon.md` phase 4 parity gate for the read-only subset, two clients and reconnect restore state (`daemon.md:224-228`); on a phone the board, a plan and a diff are usable with one thumb.
- **First useful release for Max:** **end of Phase C.** From the switcher he opens his desk's board, plans and diffs on his phone, privately. 11972 continues to carry steering and approvals.

### Phase D: mutations (ATOMICS about 6 days; relay-spatial about 1)

- `approval_decide`, `plan_command` approve, `review_command`, `session_command` steering; operation IDs and the journal with uncertain-crash outcomes (`daemon.md:103-108`).
- **Gate:** two clients race an approval and exactly one wins; a stale approval cannot approve the next request (`daemon.md:165-166`); a retry returns the known outcome; a viewer cannot mutate.

### Phase E: terminals (ATOMICS about 15-20 days; relay-spatial about 2)

- Snapshots first (read-only), then deltas, history and eviction, then input with leases and fencing, plus the on-screen key bar.
- **Gate:** the terminal parity list at `daemon.md:224-227` (wide and combining cells, colors, wrapping, cursor, resize, alternate screen, scrollback and copy); a persistent slow client is disconnected, a dropped delta forces a snapshot, an expired lease fences late input, input is never replayed across a disconnect (`daemon.md:109`).

### Phase F: headless host and hardening (ATOMICS about 10 days; later)

- Move the protocol thread behind a headless `Host::start` and change shutdown ownership (`daemon.md:306-319`); the container profile (`daemon.md:189-209`); signed-bundle verification in the loader; optional short-code pairing; revocation drills.

### Totals

Roughly 14-18 weeks of agent work end to end, versus the ticket's roughly 4-6 weeks. The difference is the UI crate
extraction (Phase C) and terminals with input (Phase E). Max's first useful release is Phase C, about 6-8 weeks
after Phase 0.

## 10. Rejected at the top level

- **A Leptos-native client** (section 4.2), and **making 11972's conversation the whole of this ticket**: it
  already exists.
- **Put the terminal bytes through a Relay-side MCP tool.** Wrong transport (polled, posted as text, cleaner-applied,
  non-private).
- **Tailnet or auth proxy only.** It remains supported, but it is not the "zero-setup" Relay route Max chose.

## 11. Open questions for Max

1. **Typing from a phone.** A controller device can run anything in a terminal, e.g. `git push`. Is that
   acceptable with a desktop-granted controller role and 12 h expiry, or do you want terminals read-only on the phone
   in v1 (Phase E input deferred), as 11972 kept irreversible actions desktop-only?
2. **Order.** Is "board, plans and diffs, read-only" the right first release (Phase C), or do you want live terminals
   in the first release, accepting about 3-4 more weeks?
3. **The effort.** This design puts the total at about 14-18 weeks against the ticket's 4-6, because the egui UI is
   about 41,800 lines tied to `State`. Do you want to proceed, or take a cheaper first cut (a Phase C that only
   shows diffs and the board)?
4. **Bundle supply.** Should relay-spatial pin a reviewed `atomics-web` artifact by PR (recommended), or should
   gitops fetch it at deploy time?
5. **Origin.** Is a same-origin iframe acceptable for v1, or do you want a separate origin (stronger isolation, a
   cookie and ingress change)?
6. **Pairing without a camera.** Is "copy the link" enough, or do you want a typed short code, which needs a PAKE?
7. **Who owns the broker.** A new relay-backend service is recommended. If the kernel already has a streaming host
   (Phase 0 checks), do you still want a separate service, or prefer reuse (needs an ATOM ticket)?
8. **Where the host runs.** The first host is the desktop app. Does the headless/cloud host (Phase F) need to come
   earlier for your use?
