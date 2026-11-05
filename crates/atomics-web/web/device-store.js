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

let opened;
function discard(connection) {
  if (opened === connection) opened = undefined;
}

function open() {
  if (!opened) {
    const connection = new Promise((resolve, reject) => {
      const request = indexedDB.open(DB, 1);
      request.onupgradeneeded = () => {
        request.result.createObjectStore(KEYS);
        request.result.createObjectStore(DEVICES);
      };
      request.onsuccess = () => {
        const db = request.result;
        // A backgrounded browser may close the connection. A schema change must also let it go.
        db.onclose = () => discard(connection);
        db.onversionchange = () => { db.close(); discard(connection); };
        resolve(db);
      };
      request.onerror = () => reject(request.error);
    });
    opened = connection;
    // An unsuccessful open must not poison every later attempt, including Try again.
    connection.catch(() => discard(connection));
  }
  return opened;
}

async function withDatabase(action) {
  for (let attempt = 0; ; attempt++) {
    const connection = open();
    let db;
    try {
      db = await connection;
      return await action(db);
    } catch (error) {
      // A close can precede its event, or interrupt a transaction. Retry once with a new connection.
      // Quota, permission, malformed records and decrypt errors stay errors, never "no device".
      const lost = ['InvalidStateError', 'TransactionInactiveError', 'AbortError', 'UnknownError'].includes(error?.name);
      if (!lost) throw error;
      db?.close();
      discard(connection);
      if (attempt) throw error;
    }
  }
}

function transact(db, name, mode, action) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(name, mode);
    const request = action(tx.objectStore(name));
    let result;
    request.onsuccess = () => { result = request.result; };
    request.onerror = () => reject(request.error);
    // A request's success is not a committed write: an interrupted transaction may still abort.
    tx.oncomplete = () => resolve(result);
    tx.onabort = () => reject(tx.error ?? new DOMException('Storage transaction interrupted', 'AbortError'));
  });
}

const aad = (hostId, hostKey) => new TextEncoder().encode(`atomics-device-v1\n${hostId}\n${hostKey}`);

async function sealingKey(db) {
  const kept = await transact(db, KEYS, 'readonly', s => s.get(SEAL));
  if (kept) return kept;
  const key = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
  try {
    await transact(db, KEYS, 'readwrite', s => s.add(key, SEAL));
    return key;
  } catch (error) {
    if (error?.name !== 'ConstraintError') throw error;
    // Another tab made one first: use that.
    const kept = await transact(db, KEYS, 'readonly', s => s.get(SEAL));
    if (!kept) throw new Error('The storage sealing key could not be read');
    return kept;
  }
}

// Best effort, at every boot as well as after saving. A denied request is shown to the user; it must
// neither block a stored key's use nor turn a committed save into a failure. null means unsupported.
export async function requestPersistence() {
  try {
    if (!navigator.storage?.persist) return null;
    return await navigator.storage.persist();
  } catch {
    return false;
  }
}

// Replaces what was kept for the host only once the host accepted a pairing. The driver reuses an
// existing device key and asks first when the pairing names a different host key (ConfirmRepair).
export async function saveDevice(hostId, hostKey, secret) {
  // The caller's bytes are a view of wasm memory: copy them before anything waits.
  const plain = Uint8Array.from(secret);
  try {
    await withDatabase(async db => {
      const key = await sealingKey(db);
      const iv = crypto.getRandomValues(new Uint8Array(12));
      const sealed = await crypto.subtle.encrypt({ name: 'AES-GCM', iv, additionalData: aad(hostId, hostKey) }, key, plain);
      await transact(db, DEVICES, 'readwrite', s => s.put({ v: 1, hostKey, iv, sealed, pairedAt: Date.now() }, hostId));
    });
  } finally {
    plain.fill(0);
  }
}

export async function loadDevice(hostId) {
  return withDatabase(async db => {
    const record = await transact(db, DEVICES, 'readonly', s => s.get(hostId));
    if (record === undefined) return null;
    if (record?.v !== 1) throw new Error('The stored device could not be read');
    const key = await transact(db, KEYS, 'readonly', s => s.get(SEAL));
    if (!key) throw new Error('The storage sealing key could not be read');
    const plain = await crypto.subtle.decrypt(
      { name: 'AES-GCM', iv: record.iv, additionalData: aad(hostId, record.hostKey) }, key, record.sealed);
    return { hostKey: record.hostKey, secret: new Uint8Array(plain) };
  });
}

export async function forgetDevice(hostId) {
  await withDatabase(db => transact(db, DEVICES, 'readwrite', s => s.delete(hostId)));
}
