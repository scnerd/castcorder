// This browser profile's device key (castcorder-client `identity`). This is a
// function body, not a module: the app compiles it with `new Function` and
// calls it once; it returns { load, sign }.
//
// The key is a WebCrypto Ed25519 key pair whose private half is generated
// non-extractable and kept, as a CryptoKey, in IndexedDB (database
// "castcorder", store "identity", record "device"). Page script can use it
// to sign while the page is open but can never read the private key bytes.
// The record also keeps `alias`: the profile's old device id (localStorage
// "castcorder.device", a UUID from before keys), captured when the key is
// first made, so the host can bind it to the key once.
//
// load() -> Promise<{ pub: Uint8Array(32), alias: string | null }>
// sign(bytes: Uint8Array) -> Promise<Uint8Array(64)>

const DB = 'castcorder';
const STORE = 'identity';
const RECORD = 'device';
let cached = null;

function request(r) {
  return new Promise((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

function open() {
  const r = indexedDB.open(DB, 1);
  r.onupgradeneeded = () => r.result.createObjectStore(STORE);
  return request(r);
}

async function read(db) {
  return request(db.transaction(STORE).objectStore(STORE).get(RECORD));
}

async function create(db) {
  const pair = await crypto.subtle.generateKey({ name: 'Ed25519' }, false, ['sign', 'verify']);
  const pub = new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey));
  let alias = null;
  try { alias = localStorage.getItem('castcorder.device'); } catch (e) { alias = null; }
  const record = { privateKey: pair.privateKey, publicKey: pair.publicKey, pub, alias };
  try {
    // add, not put: another tab may have made the key first; theirs wins.
    await request(db.transaction(STORE, 'readwrite').objectStore(STORE).add(record, RECORD));
    return record;
  } catch (e) {
    const theirs = await read(db);
    if (theirs) return theirs;
    throw e;
  }
}

async function load() {
  if (!cached) {
    cached = (async () => {
      const db = await open();
      const record = (await read(db)) || (await create(db));
      db.close();
      return record;
    })();
    cached.catch(() => { cached = null; });
  }
  const r = await cached;
  return { pub: r.pub, alias: r.alias || null };
}

async function sign(bytes) {
  await load();
  const r = await cached;
  return new Uint8Array(await crypto.subtle.sign('Ed25519', r.privateKey, bytes));
}

return { load, sign };
