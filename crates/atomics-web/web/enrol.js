// Makes the client's passkey at top level on its own origin, for a frame that may not (web/passkey.js), and tells
// the window that opened this one its credential ID, which is public. The passkey's RP ID is this origin's host.
// Nothing here sees a key: making a passkey returns none.

const button = document.getElementById('make');
const state = document.getElementById('state');
const random = n => crypto.getRandomValues(new Uint8Array(n));
const base64url = bytes => btoa(String.fromCharCode(...new Uint8Array(bytes))).replace(/=+$/, '').replaceAll('+', '-').replaceAll('/', '_');

function say(text) {
  state.textContent = text;
}

function tell(message) {
  if (!window.opener) return false;
  window.opener.postMessage({ type: 'atomics.enrolled', v: 1, ...message }, location.origin);
  return true;
}

button.addEventListener('click', () => {
  button.disabled = true;
  say('Waiting for the passkey…');
  navigator.credentials.create({
    publicKey: {
      rp: { id: location.hostname, name: 'ATOMICS' },
      user: { id: random(16), name: 'ATOMICS', displayName: 'ATOMICS on this browser' },
      challenge: random(32),
      pubKeyCredParams: [{ type: 'public-key', alg: -7 }, { type: 'public-key', alg: -257 }],
      authenticatorSelection: { residentKey: 'required', requireResidentKey: true, userVerification: 'required' },
      extensions: { prf: {} },
    },
  }).then(credential => {
    const enabled = credential.getClientExtensionResults().prf?.enabled;
    if (enabled === false) {
      tell({ prf: false });
      return say('This passkey cannot keep pairings: it has no PRF. You can close this window.');
    }
    const told = tell({ credential: base64url(credential.rawId), prf: enabled === true ? true : null });
    if (!told) return say('The passkey is made. Go back to ATOMICS and tap Keep.');
    say('The passkey is made.');
    setTimeout(() => window.close(), 600);
  }, error => {
    button.disabled = false;
    const name = typeof error?.name === 'string' ? error.name : 'Error';
    say(name === 'NotAllowedError' ? 'Cancelled. Try again, or close this window.' : `The passkey could not be made (${name}).`);
  });
});
