// This browser's Noise static key for each host it paired with (docs/daemon.md, "The browser client
// and its embedding").
//
// IndexedDB keeps one record per host: the host's public key and the device key sealed with AES-GCM
// (the host id and key are its additional data). The sealing key is a WebCrypto key made
// non-extractable, so no script can read it out, and a copy of the database alone is ciphertext.
// Script on this origin can still use the sealing key, which it must to run a handshake: that is the
// limit of keeping a key in a browser (docs/decisions/2026-11-02-embedded-atomics.md §6.2), and the
// reason the client runs on an origin of its own. Nothing here logs a key.

const DB = 'atomics-devices';
const KEYS = 'keys';
const DEVICES = 'devices';
const SEAL = 'seal';

const done = request => new Promise((resolve, reject) => {
  request.onsuccess = () => resolve(request.result);
  request.onerror = () => reject(request.error);
});

let opened;
function open() {
  opened ??= new Promise((resolve, reject) => {
    const request = indexedDB.open(DB, 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore(KEYS);
      request.result.createObjectStore(DEVICES);
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  return opened;
}

const store = (db, name, mode) => db.transaction(name, mode).objectStore(name);
const aad = (hostId, hostKey) => new TextEncoder().encode(`atomics-device-v1\n${hostId}\n${hostKey}`);

async function sealingKey(db) {
  const kept = await done(store(db, KEYS, 'readonly').get(SEAL));
  if (kept) return kept;
  const key = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
  try {
    await done(store(db, KEYS, 'readwrite').add(key, SEAL));
    return key;
  } catch {
    // Another tab made one first: use that.
    return await done(store(db, KEYS, 'readonly').get(SEAL));
  }
}

export async function saveDevice(hostId, hostKey, secret) {
  // The caller's bytes are a view of wasm memory: copy them before anything waits.
  const plain = Uint8Array.from(secret);
  try {
    const db = await open();
    const key = await sealingKey(db);
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const sealed = await crypto.subtle.encrypt({ name: 'AES-GCM', iv, additionalData: aad(hostId, hostKey) }, key, plain);
    await done(store(db, DEVICES, 'readwrite').put({ v: 1, hostKey, iv, sealed, pairedAt: Date.now() }, hostId));
    // Ask the browser to keep this origin's storage when it clears unused sites' storage.
    await navigator.storage?.persist?.();
  } finally {
    plain.fill(0);
  }
}

export async function loadDevice(hostId) {
  const db = await open();
  const record = await done(store(db, DEVICES, 'readonly').get(hostId));
  if (!record || record.v !== 1) return null;
  const key = await done(store(db, KEYS, 'readonly').get(SEAL));
  if (!key) return null;
  const plain = await crypto.subtle.decrypt(
    { name: 'AES-GCM', iv: record.iv, additionalData: aad(hostId, record.hostKey) }, key, record.sealed);
  return { hostKey: record.hostKey, secret: new Uint8Array(plain) };
}

export async function forgetDevice(hostId) {
  const db = await open();
  await done(store(db, DEVICES, 'readwrite').delete(hostId));
}
