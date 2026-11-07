# A device key that survives the browser closing: sealed with a passkey, kept by the hosting page

Status: accepted and built (`src/keep.rs`, `web/passkey.js`, `web/enrol.html`; docs/daemon.md, "The sealed copy").
Built without first running the passkey probe (`crates/atomics-web/probes/passkey/README.md`) on an iPhone: if the
route does not hold there, the pairing screens' `Key:` line says which step failed.

## Why

The web client keeps each host's device key in its own origin's IndexedDB (`web/device-store.js`), in a frame the
hosting page embeds from another origin. In Safari that storage lives only as long as the browser's processes, so
the pairing is lost when the app is closed or the phone restarts, and the client says *no key stored for this
host*:

- WebKit treats a frame as third party when its origin differs from the top-level origin, same site or not, and
  gives it in-memory IndexedDB and localStorage. `ScriptExecutionContext::canAccessResource` returns
  `DefaultForThirdParty` when `!storageTopOrigin()->isSameOriginAs(origin)` under `BlockThirdParty`, the default;
  `IDBFactory::openInternal` turns that into a transient database. WebKit's tracking-prevention page: "Third-party
  LocalStorage and IndexedDB are partitioned per first-party website and also made ephemeral." WebKit bug 231879
  (a frame on another subdomain) was closed as expected behaviour.
- `navigator.storage.persist()` is refused whenever the top-level and frame origins differ
  (`NetworkStorageManager::persist`), and WebKit's Storage Access API grants cookies only. Neither helps.
- Keeping the key on the hosting page's origin, or serving the client from it, would work but undo the client's
  own origin (embedded design §4.2, §6.2): every script on the hosting origin could use the key and type into the
  host's terminals.

## Decision

The frame keeps the key as today, and also seals it with a passkey's PRF output and hands the sealed record to
the hosting page, which keeps it in its own first-party storage. The hosting page cannot open it: the passkey's
RP ID is the client origin's host, which a page on another host cannot use. When the frame's own store comes up
empty, one tap and Face ID unseal the hosting page's copy.

## The passkey

- One discoverable passkey per browser profile for all hosts, user verification required, RP ID **the client
  origin's full host**, never its registrable domain: a registrable-domain RP ID would let the hosting page use the
  passkey too.
- Made at top level on the client origin: WebKit refuses `create()` in any frame whose origin differs from its
  ancestors' (`AuthenticatorCoordinator::create`). The frame opens the client's `enrol.html` as a popup (its
  sandbox allows popups that escape it), which makes the passkey and posts its credential ID, which is public, to
  its opener. Where a browser allows `create()` in the frame (with `publickey-credentials-create`), the frame makes
  it in place. With no opener, the frame goes on with a discoverable `get()`.
- `get()` runs in the frame: it needs the `publickey-credentials-get` delegation, document focus and a user tap.
  The tap is a real button `web/passkey.js` lays over the screen's place for it: egui handles a canvas tap in a
  later frame, outside the tap's activation, where a browser may refuse a passkey or a window.
- Which browsers need it: those whose frame storage ends with them. That is WebKit (every iOS browser, and Safari
  elsewhere), told from the user agent, since no API says a frame's storage is per session.

## The sealed record

- PRF input `first`: the text `atomics-device-seal-v1`, a newline and the host id. The browser hashes it with
  WebAuthn's own label, so one passkey gives one output per host.
- Sealing key: HKDF-SHA256 of the PRF output (zero salt, info `atomics-device-seal-v1`) to a non-extractable
  AES-GCM-256 key; the output's bytes are zeroed once imported.
- Plaintext: the 32-byte device secret. Additional data: `atomics-device-v2`, host ID, host key and credential ID.
- Record: `{v: 2, host_id, host_key, credential, iv, sealed, paired_at}`, under 1 KB, all of it public or
  ciphertext.

## The embed contract (version 1, optional fields)

| Message | Direction | Fields |
| --- | --- | --- |
| `atomics.embed.boot` | page to frame | adds `keeps: true` when the page keeps records, and `sealed`: its record for this host or null |
| `atomics.keep` | frame to page | `{host_id, record}`: replace this host's record |
| `atomics.embed.kept` | page to frame | `{host_id, ok}`: the record was stored and read back |
| `atomics.forget` | frame to page | `{host_id}`: only after the host's sealed refusal, as the frame forgets its own copy |
| `atomics.paired` | frame to page | adds `kept: true` or `false` |

The page validates shape and size and stores the record as opaque data keyed by host ID. Its frame gets
`allow="publickey-credentials-get; publickey-credentials-create"`; its Permissions-Policy allows both for the
client origin (WebKit reads only the attribute). Pages without `keeps` get today's behaviour.

## Pairing: a confirmed save before "paired"

After the host accepts the device, the driver enters a new **Saving** status instead of reporting paired:

1. Save to the frame's store and read it back (as `saveDevice` does now).
2. If the page keeps records, show *Keep this pairing on this browser* with one button. The tap opens the enrol
   popup when there is no passkey yet, then runs `get()` with PRF, seals and posts `atomics.keep`.
3. On `atomics.embed.kept ok`, post `atomics.paired {kept: true}` and open the board. A page that does not answer
   within 15 seconds counts as not keeping it.

A cancelled or failed step leaves the session working, posts `atomics.paired {kept: false}`, and keeps a notice
with **Keep it** to try again: *This browser keeps the pairing only until it closes.* The first pairing on a
browser takes two Face ID prompts (make, then keep), each later one takes one.

## Cold start: one tap

1. The boot arrives; the frame reads its own store. A record found: dial `Noise_IK` as today, no prompt. This is
   the case while the browser runs, and always in browsers that keep partitioned storage.
2. None found and the boot carries `sealed` for this host: a new **Locked** status shows *Unlock this pairing* with
   one button. The tap runs `get()` with `allowCredentials` set to the record's credential and the host's PRF input.
   Face ID, unseal, save to the frame's store, and dial with a fresh ticket: the boot's may have expired meanwhile.
3. None found and no `sealed`: `Stop::NotHere`, as today.

A cancelled unlock stays on Locked, its button ready again. A failed unseal or a missing PRF output never deletes the
record: it says so and offers pairing again. Only the host's sealed `not_paired` forgets both copies.

## What it costs

- One tap and Face ID per cold start in Safari; none while the browser runs, none in browsers that keep the
  frame's storage.
- The hosting page stores and returns a record, delegates `get()` to the frame, and should ask for persistent
  storage for itself (granted to installed apps).
- iCloud Keychain syncs the passkey to the user's other devices. The sealed record stays in this browser's
  hosting-page storage, so another device has the passkey but not the record; stealing that storage and using a
  synced passkey together would move this browser's device key elsewhere.
- Script on the client origin can still use the frame's in-memory key while it runs, as today, but can unseal
  the record only through a visible Face ID prompt.

## Later

The host could keep a third copy, fetched through the broker before the handshake by credential ID. It would
survive the hosting page's storage being cleared and disappear when the device is revoked, at the cost of a
protocol message and a host release.

## Open, until an iPhone has used it

- PRF output from `get()` inside the frame on iOS, with an iCloud Keychain passkey.
- Whether what the frame sealed opens again after Safari is quit and the frame's own storage is gone.
- Whether iCloud Keychain gives PRF output at `create()`; if it does, the first pairing could need one prompt.
- Whether the enrol popup has an opener in a Home Screen app.
