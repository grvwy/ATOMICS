// Real IndexedDB and WebCrypto, with faults at the browser API boundary. No bundle or broker needed.
import { saveDevice, loadDevice, forgetDevice, requestPersistence } from '/device-store.js';

const assert = (ok, message) => { if (!ok) throw new Error(message); };
const rejected = async action => {
  try { await action(); return false; } catch { return true; }
};
const transaction = (db, store, mode, action) => new Promise((resolve, reject) => {
  const tx = db.transaction(store, mode);
  const req = action(tx.objectStore(store));
  let value;
  req.onsuccess = () => { value = req.result; };
  tx.oncomplete = () => resolve(value);
  tx.onabort = () => reject(tx.error);
});

export async function check() {
  const checked = [];
  const plain = new Uint8Array(32).fill(23);
  let current;
  let opens = 0;
  const originalOpen = indexedDB.open;
  const originalTransaction = IDBDatabase.prototype.transaction;
  const originalPersist = navigator.storage.persist;
  indexedDB.open = function (...args) {
    opens++;
    const request = originalOpen.apply(this, args);
    request.addEventListener('success', () => { current = request.result; });
    return request;
  };
  const sameDevice = async () => {
    const device = await loadDevice('host');
    assert(device?.hostKey === 'public-host-key', 'The host key must survive');
    assert(device.secret.length === plain.length && device.secret.every((b, i) => b === plain[i]), 'The device key must survive');
    device.secret.fill(0);
  };
  try {
    assert(await loadDevice('host') === null, 'An absent record means no device');
    await saveDevice('host', 'public-host-key', plain);
    await sameDevice();
    const seal = await transaction(current, 'keys', 'readonly', s => s.get('seal'));
    assert(seal.extractable === false, 'The sealing key must remain non-extractable');
    assert(await rejected(() => crypto.subtle.exportKey('raw', seal)), 'The sealing key must not be exported');
    const record = await transaction(current, 'devices', 'readonly', s => s.get('host'));
    assert(record.sealed instanceof ArrayBuffer && !('secret' in record), 'Only ciphertext is stored');
    assert(localStorage.length === 0, 'No key belongs in localStorage');
    checked.push('sealed storage');

    let before = opens;
    current.close(); // Explicit close emits no close event: the next transaction must recover too.
    await sameDevice();
    assert(opens === before + 1, 'A stale connection must be reopened');
    checked.push('closed connection');

    before = opens;
    current.close();
    current.dispatchEvent(new Event('close')); // The browser reports an unexpected background close.
    await sameDevice();
    assert(opens === before + 1, 'A reported connection loss must discard the cached connection');
    checked.push('background close');

    before = opens;
    current.dispatchEvent(new Event('versionchange'));
    await sameDevice();
    assert(opens === before + 1, 'A version change must release and reopen the connection');
    checked.push('version change');

    let failures = 0;
    before = opens;
    IDBDatabase.prototype.transaction = function (...args) {
      if (failures++ === 0) throw new DOMException('Connection interrupted', 'UnknownError');
      return originalTransaction.apply(this, args);
    };
    await sameDevice();
    assert(opens === before + 1, 'A failed read on a lost connection must retry on a new one');
    IDBDatabase.prototype.transaction = originalTransaction;
    checked.push('interrupted read');

    // Success of an individual request is too early: abort before the transaction commits.
    let aborted = false;
    before = opens;
    IDBDatabase.prototype.transaction = function (...args) {
      const tx = originalTransaction.apply(this, args);
      if (args[0] === 'devices' && args[1] === 'readwrite' && !aborted) {
        const objectStore = tx.objectStore.bind(tx);
        tx.objectStore = name => {
          const store = objectStore(name);
          const put = store.put.bind(store);
          store.put = (...values) => {
            const request = put(...values);
            request.addEventListener('success', () => { aborted = true; tx.abort(); });
            return request;
          };
          return store;
        };
      }
      return tx;
    };
    await saveDevice('host', 'public-host-key', plain);
    IDBDatabase.prototype.transaction = originalTransaction;
    assert(aborted && opens === before + 1, 'An aborted save must reopen and commit on retry');
    await sameDevice();
    checked.push('aborted save');

    failures = 0;
    IDBDatabase.prototype.transaction = function () {
      failures++;
      throw new DOMException('Connection unavailable', 'InvalidStateError');
    };
    assert(await rejected(() => loadDevice('host')), 'Exhausted connection retries must stay a read error');
    assert(failures === 2, 'A connection retry must be bounded');
    IDBDatabase.prototype.transaction = originalTransaction;
    await sameDevice();
    checked.push('bounded retry');

    IDBDatabase.prototype.transaction = function () { throw new DOMException('Read denied', 'SecurityError'); };
    assert(await rejected(() => loadDevice('host')), 'A denied read must never mean no key');
    IDBDatabase.prototype.transaction = originalTransaction;
    await sameDevice();
    checked.push('read failure preserves key');

    current.close();
    current.dispatchEvent(new Event('close'));
    indexedDB.open = () => {
      const request = {};
      queueMicrotask(() => {
        request.error = new DOMException('Open denied', 'SecurityError');
        request.onerror();
      });
      return request;
    };
    assert(await rejected(() => loadDevice('host')), 'A failed open must stay a read error');
    indexedDB.open = function (...args) {
      opens++;
      const request = originalOpen.apply(this, args);
      request.addEventListener('success', () => { current = request.result; });
      return request;
    };
    await sameDevice();
    checked.push('failed open can retry');

    await transaction(current, 'keys', 'readwrite', s => s.delete('seal'));
    assert(await rejected(() => loadDevice('host')), 'A missing sealing key must never mean no device');
    await transaction(current, 'keys', 'readwrite', s => s.put(seal, 'seal'));
    await sameDevice();
    await transaction(current, 'devices', 'readwrite', s => s.put({ ...record, v: 2 }, 'host'));
    assert(await rejected(() => loadDevice('host')), 'An unsupported record must stay a read error');
    await transaction(current, 'devices', 'readwrite', s => s.put({ ...record, hostKey: 'changed' }, 'host'));
    assert(await rejected(() => loadDevice('host')), 'A decrypt failure must stay a read error');
    await transaction(current, 'devices', 'readwrite', s => s.put(record, 'host'));
    await sameDevice();
    checked.push('unreadable record preserves key');

    navigator.storage.persist = async () => false;
    assert(await requestPersistence() === false, 'Persistence denial must be reported');
    await sameDevice();
    navigator.storage.persist = async () => { throw new Error('Unavailable'); };
    assert(await requestPersistence() === false, 'Persistence failure must be best effort');
    await saveDevice('host', 'public-host-key', plain);
    await sameDevice();
    navigator.storage.persist = async () => true;
    assert(await requestPersistence() === true, 'Persistence success must be reported');
    checked.push('persistence is best effort');

    await forgetDevice('host');
    assert(await loadDevice('host') === null, 'Explicit forget removes the device');
    checked.push('explicit forget');
    return checked;
  } finally {
    indexedDB.open = originalOpen;
    IDBDatabase.prototype.transaction = originalTransaction;
    navigator.storage.persist = originalPersist;
    plain.fill(0);
  }
}
