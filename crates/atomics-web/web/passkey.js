// The device key's passkey copy (docs/decisions/2026-11-07-passkey-sealed-device-key.md; `src/keep.rs`). Where this
// frame's own storage ends with the browser, the key is also sealed under a passkey's PRF output and the sealed
// record kept by the hosting page; a cold start opens it again with one tap.
//
// One button on the page, which the app places over the screen that asks for it. A passkey needs the tap's own
// activation, which only a real element's click carries: egui draws its frame later. The app prepares what the next
// tap does (seal this device's key, or open a sealed record) and hears back what came of it. Every WebAuthn call
// starts inside the click, before anything waits.
//
// The passkey's RP ID is this origin's host, never its registrable domain, so the hosting page on another host can
// neither ask for it nor open what it seals. No key material is shown or logged: a PRF output becomes a
// non-extractable AES key at once and its bytes are zeroed, and the device key handed in for sealing is a copy,
// zeroed when the app clears the button.

const LABEL = 'atomics-device-seal-v1';
const encode = text => new TextEncoder().encode(text);
// The browser hashes the PRF input with WebAuthn's own label, so the text can be the input: one output per host.
const prfInput = hostId => encode(`${LABEL}\n${hostId}`);
const aad = (hostId, hostKey, credential) => encode(`atomics-device-v2\n${hostId}\n${hostKey}\n${credential}`);
const random = n => crypto.getRandomValues(new Uint8Array(n));
const zero = buffer => { if (buffer) new Uint8Array(buffer.buffer ?? buffer).fill(0); };

const base64 = bytes => btoa(String.fromCharCode(...new Uint8Array(bytes))).replace(/=+$/, '');
const base64url = bytes => base64(bytes).replaceAll('+', '-').replaceAll('/', '_');
const unbase64 = text => {
  const plain = text.replaceAll('-', '+').replaceAll('_', '/');
  return Uint8Array.from(atob(plain + '='.repeat((4 - (plain.length % 4)) % 4)), c => c.charCodeAt(0));
};
const errorName = error => (typeof error?.name === 'string' && /^[A-Za-z]{1,40}$/.test(error.name) ? error.name : 'Error');

/** WebKit, whose frames on another origin get storage that lasts only while the browser runs, and may not make a
 * passkey. Every browser on iOS is WebKit. */
function webkit() {
  const agent = navigator.userAgent;
  const ios = /iPhone|iPad|iPod/.test(agent) || (/Macintosh/.test(agent) && navigator.maxTouchPoints > 1);
  return ios || (/AppleWebKit\//.test(agent) && !/(Chrome|Chromium)\//.test(agent));
}

/** What this browser offers: `src/keep.rs`'s `Passkeys`. */
export async function passkeyFacts() {
  const engine = webkit();
  let available = false;
  let prf = null;
  try {
    available = Boolean(window.PublicKeyCredential) && await PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable();
  } catch {
    available = false;
  }
  try {
    const capabilities = await window.PublicKeyCredential?.getClientCapabilities?.();
    if (typeof capabilities?.['extension:prf'] === 'boolean') prf = capabilities['extension:prf'];
  } catch {
    prf = null;
  }
  return { available, prf, per_session: window.top !== window && engine, create_in_frame: !engine };
}

let report = null;
let button = null;
let label = '';
let pending = null;
let busy = false;

/** The app's callback, which hears what came of each tap. */
export function installPasskey(callback) {
  report = callback;
}

function answer(result) {
  busy = false;
  show();
  report?.(result);
}

const failed = (step, error) => ({ kind: 'failed', step, name: errorName(error), detail: String(error?.message ?? '').slice(0, 120) });

function show() {
  if (!button) return;
  button.textContent = busy ? 'Waiting for the passkey…' : label;
  button.disabled = busy || !pending;
}

/** What the next tap does: `{kind: "keep", hostId, hostKey, credential, made, inFrame}` with the device key as
 * `secret`, or `{kind: "unlock", record}`. Replaces, and zeroes, whatever was prepared before. */
export function preparePasskey(op, secret) {
  clearPasskey();
  pending = { op: JSON.parse(op), secret: secret ? Uint8Array.from(secret) : null };
  show();
}

/** Nothing to do: the button goes and the prepared key is zeroed. */
export function clearPasskey() {
  if (pending?.secret) pending.secret.fill(0);
  pending = null;
  busy = false;
  if (button) button.hidden = true;
}

/** Lay the button over the screen's place for it, in CSS pixels. */
export function placePasskey(x, y, width, height, text) {
  if (!button) {
    button = document.createElement('button');
    button.id = 'atomics-passkey';
    button.type = 'button';
    button.addEventListener('click', tapped);
    document.body.append(button);
  }
  label = text;
  Object.assign(button.style, { left: x + 'px', top: y + 'px', width: width + 'px', height: height + 'px' });
  button.hidden = false;
  show();
}

export function hidePasskey() {
  if (button) button.hidden = true;
}

function tapped() {
  if (busy || !pending) return;
  const { op } = pending;
  // A copy for this tap: clearing the button while the passkey runs zeroes the prepared key, not this one.
  const secret = pending.secret ? Uint8Array.from(pending.secret) : null;
  let running;
  try {
    if (op.kind === 'unlock') running = unlock(op.record);
    else if (op.credential || op.made) running = seal(op, secret);
    else if (op.inFrame) running = makeHere(op, secret);
    else return enrolInWindow();
  } catch (error) {
    secret?.fill(0);
    return answer(failed('start', error));
  }
  busy = true;
  show();
  running.then(answer, error => answer(failed(error?.step ?? 'passkey', error?.error ?? error))).finally(() => secret?.fill(0));
}

const tagged = step => error => { throw { step, error }; };

// The PRF output as an AES-GCM key no script can read out; the output's bytes are zeroed either way.
async function sealingKey(output) {
  try {
    const base = await crypto.subtle.importKey('raw', output, 'HKDF', false, ['deriveKey']);
    return await crypto.subtle.deriveKey({ name: 'HKDF', hash: 'SHA-256', salt: new Uint8Array(32), info: encode(LABEL) },
      base, { name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
  } finally {
    zero(output);
  }
}

async function sealWith(credential, output, op, secret) {
  if (!output) return { kind: 'no-prf' };
  const key = await sealingKey(output);
  const id = base64url(credential.rawId);
  const iv = random(12);
  const sealed = await crypto.subtle.encrypt({ name: 'AES-GCM', iv, additionalData: aad(op.hostId, op.hostKey, id) }, key, secret);
  return {
    kind: 'sealed',
    record: { v: 2, host_id: op.hostId, host_key: op.hostKey, credential: id, iv: base64(iv), sealed: base64(sealed), paired_at: Date.now() },
  };
}

const prfOutput = credential => credential.getClientExtensionResults().prf?.results?.first;

// With the passkey the record or the app names, or whichever the browser offers when one was just made.
function seal(op, secret) {
  return navigator.credentials.get({
    publicKey: {
      rpId: location.hostname,
      challenge: random(32),
      userVerification: 'required',
      allowCredentials: op.credential ? [{ type: 'public-key', id: unbase64(op.credential) }] : [],
      extensions: { prf: { eval: { first: prfInput(op.hostId) } } },
    },
  }).then(credential => sealWith(credential, prfOutput(credential), op, secret), tagged('get'));
}

function creation(hostId) {
  return {
    rp: { id: location.hostname, name: 'ATOMICS' },
    user: { id: random(16), name: 'ATOMICS', displayName: 'ATOMICS on this browser' },
    challenge: random(32),
    pubKeyCredParams: [{ type: 'public-key', alg: -7 }, { type: 'public-key', alg: -257 }],
    authenticatorSelection: { residentKey: 'required', requireResidentKey: true, userVerification: 'required' },
    extensions: { prf: hostId ? { eval: { first: prfInput(hostId) } } : {} },
  };
}

// Where a frame may make a passkey: make it, and seal at once when the passkey gives its PRF output on creation.
function makeHere(op, secret) {
  return navigator.credentials.create({ publicKey: creation(op.hostId) }).then(credential => {
    const prf = credential.getClientExtensionResults().prf;
    if (prf?.enabled === false) return { kind: 'no-prf' };
    const output = prf?.results?.first;
    if (output) return sealWith(credential, output, op, secret);
    return { kind: 'enrolled', credential: base64url(credential.rawId) };
  }, tagged('create'));
}

// WebKit makes passkeys at top level only: `enrol.html`, in a window on this origin, makes it and says so. Where
// the window cannot say (no opener), its closing counts as made, and the next tap asks for whichever passkey the
// browser offers.
function enrolInWindow() {
  const popup = window.open(new URL('enrol.html', location.href).href, 'atomics-enrol', 'popup=yes,width=460,height=640');
  if (!popup) return answer(failed('window', { name: 'Blocked', message: 'the passkey window did not open' }));
  answer({ kind: 'enrolling' });
  let done = false;
  const finish = result => {
    if (done) return;
    done = true;
    clearInterval(timer);
    removeEventListener('message', heard);
    answer(result);
  };
  const heard = event => {
    if (event.origin !== location.origin || event.source !== popup) return;
    const data = event.data;
    if (data?.type !== 'atomics.enrolled' || data.v !== 1) return;
    const credential = typeof data.credential === 'string' && /^[A-Za-z0-9_-]{16,1400}$/.test(data.credential) ? data.credential : null;
    finish(data.prf === false ? { kind: 'no-prf' } : { kind: 'enrolled', credential });
  };
  const timer = setInterval(() => { if (popup.closed) finish({ kind: 'enrolled', credential: null }); }, 500);
  addEventListener('message', heard);
}

function unlock(record) {
  return navigator.credentials.get({
    publicKey: {
      rpId: location.hostname,
      challenge: random(32),
      userVerification: 'required',
      allowCredentials: [{ type: 'public-key', id: unbase64(record.credential) }],
      extensions: { prf: { eval: { first: prfInput(record.host_id) } } },
    },
  }).then(async credential => {
    const output = prfOutput(credential);
    if (!output) return { kind: 'no-prf' };
    const key = await sealingKey(output);
    const additionalData = aad(record.host_id, record.host_key, record.credential);
    const plain = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: unbase64(record.iv), additionalData }, key, unbase64(record.sealed))
      .catch(tagged('open'));
    return { kind: 'unlocked', secret: new Uint8Array(plain) };
  }, tagged('get'));
}
