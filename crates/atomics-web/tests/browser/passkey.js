// The passkey glue (`web/passkey.js`) in a frame on another origin, framed as the hosting page frames the client, with
// Chrome's virtual authenticator. The test taps the page's button through DevTools; this page keeps what each tap
// said, and never the key: an unlock reports only whether the key came back unchanged.
import { clearPasskey, installPasskey, passkeyFacts, placePasskey, preparePasskey } from '/passkey.js';

const HOST_KEY = 'A'.repeat(43);
const secret = new Uint8Array(32).fill(7);
const results = [];

installPasskey(result => {
  if (result.kind !== 'unlocked') return results.push(result);
  const same = result.secret.length === 32 && result.secret.every(b => b === 7);
  result.secret.fill(0);
  results.push({ kind: 'unlocked', same });
});

window.check = {
  results,
  ready: false,
  keep(op) {
    preparePasskey(JSON.stringify({ kind: 'keep', hostId: 'host-1', hostKey: HOST_KEY, credential: null, made: false, inFrame: true, ...op }), secret);
    placePasskey(16, 16, 240, 44, 'Keep with passkey');
  },
  unlock(record) {
    preparePasskey(JSON.stringify({ kind: 'unlock', record }), new Uint8Array());
    placePasskey(16, 16, 240, 44, 'Unlock with passkey');
  },
  clear: clearPasskey,
};

passkeyFacts().then(facts => {
  window.check.facts = facts;
  window.check.ready = true;
});
