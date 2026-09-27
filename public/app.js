// Teahouse web UI, step one: one Ed25519 key per phone, locked by a passkey.
//
// The account is the raw 32-byte Ed25519 public key, hex, the same thing
// `kot id` prints. The private half is the 32-byte seed, the same 64 hex
// characters a kot seed file holds. WebCrypto makes and signs with it.
//
// The passkey is the lock on the seeds, not the account: WebAuthn signs only
// its own challenge, never the bytes the API wants. Every unlock asks the
// passkey for its PRF secret, and that secret (through HKDF) is the AES key
// the seeds are wrapped with. No PRF, no keys: a passkey that can't produce
// one is refused at setup. Forgetting the passkey forgets the seeds.
//
// The API is /api on the same origin, so no CORS is involved.

'use strict';

const $ = (s) => document.querySelector(s);
const enc = new TextEncoder();
const hex = (b) => [...new Uint8Array(b)].map((x) => x.toString(16).padStart(2, '0')).join('');
const unhex = (s) => Uint8Array.from(s.match(/../g).map((h) => parseInt(h, 16)));
const rand = (n) => crypto.getRandomValues(new Uint8Array(n));
const PKCS8_PREFIX = unhex('302e020100300506032b657004220420');
const ED = { name: 'Ed25519' };
const API = '/api';

// ---------------------------------------------------------------- storage

function openDb() {
  return new Promise((res, rej) => {
    const r = indexedDB.open('teahouse', 1);
    r.onupgradeneeded = () => {
      r.result.createObjectStore('keys', { keyPath: 'account' });
      r.result.createObjectStore('vault', { keyPath: 'id' });
    };
    r.onsuccess = () => res(r.result);
    r.onerror = () => rej(r.error);
  });
}
async function store(name, mode, fn) {
  const db = await openDb();
  return new Promise((res, rej) => {
    const t = db.transaction(name, mode);
    const req = fn(t.objectStore(name));
    t.oncomplete = () => res(req && req.result);
    t.onerror = () => rej(t.error);
  });
}
const getKey = async () => (await store('keys', 'readonly', (s) => s.getAll()))[0] || null;
const putKey = (k) => store('keys', 'readwrite', (s) => s.put(k));
const clearKey = () => store('keys', 'readwrite', (s) => s.clear());
const getVault = () => store('vault', 'readonly', (s) => s.get('passkey'));
const putVault = (v) => store('vault', 'readwrite', (s) => s.put({ id: 'passkey', ...v }));
const wipe = async () => { await store('vault', 'readwrite', (s) => s.clear()); await store('keys', 'readwrite', (s) => s.clear()); };

// ---------------------------------------------------------------- crypto

async function deriveWrapKey(secret, salt) {
  const ikm = await crypto.subtle.importKey('raw', secret, 'HKDF', false, ['deriveKey']);
  return crypto.subtle.deriveKey(
    { name: 'HKDF', hash: 'SHA-256', salt, info: enc.encode('teahouse seed vault v1') },
    ikm, { name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
}
async function wrapSeed(wrapKey, seed) {
  const iv = rand(12);
  const ct = await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, wrapKey, seed);
  return { iv, ct: new Uint8Array(ct) };
}
async function unwrapSeed(wrapKey, w) {
  return new Uint8Array(await crypto.subtle.decrypt({ name: 'AES-GCM', iv: w.iv }, wrapKey, w.ct));
}
async function seedToPrivate(seed) {
  const pkcs8 = new Uint8Array(48);
  pkcs8.set(PKCS8_PREFIX); pkcs8.set(seed, 16);
  return crypto.subtle.importKey('pkcs8', pkcs8, ED, true, ['sign']);
}
async function publicOf(priv) {
  const jwk = await crypto.subtle.exportKey('jwk', priv);
  return hex(Uint8Array.from(atob(jwk.x.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0)));
}
async function newSeed() {
  const kp = await crypto.subtle.generateKey(ED, true, ['sign', 'verify']);
  return new Uint8Array(await crypto.subtle.exportKey('pkcs8', kp.privateKey)).slice(16);
}

// ---------------------------------------------------------------- session

// `vault` is read once so an unlock reaches the authenticator without an
// IndexedDB round trip first; Safari is strict about the tap that starts a
// passkey prompt. `wrapKey` lives only here, and only while unlocked.
const state = { vault: null, wrapKey: null, priv: null, name: null };

async function registerPasskey() {
  const salt = rand(32);
  const cred = await navigator.credentials.create({
    publicKey: {
      challenge: rand(32),
      rp: { name: 'teahouse', id: location.hostname },
      user: { id: rand(16), name: 'teahouse keys', displayName: 'teahouse keys' },
      pubKeyCredParams: [{ type: 'public-key', alg: -8 }, { type: 'public-key', alg: -7 }, { type: 'public-key', alg: -257 }],
      authenticatorSelection: { residentKey: 'required', userVerification: 'required' },
      extensions: { prf: { eval: { first: salt } } },
    },
  });
  const ext = cred.getClientExtensionResults();
  if (!(ext.prf && ext.prf.enabled)) {
    throw new Error('this passkey can\'t make a PRF secret, so it can\'t lock keys. Use iCloud Keychain or Google Password Manager on a current phone.');
  }
  return { credId: new Uint8Array(cred.rawId), salt, createdAt: Date.now() };
}

async function unlock() {
  const v = state.vault;
  if (!v) throw new Error('set up a passkey first');
  const a = await navigator.credentials.get({
    publicKey: {
      challenge: rand(32),
      rpId: location.hostname,
      allowCredentials: [{ type: 'public-key', id: v.credId }],
      userVerification: 'required',
      extensions: { prf: { eval: { first: v.salt } } },
    },
  });
  const out = a.getClientExtensionResults().prf?.results?.first;
  if (!out) throw new Error('the passkey answered without its PRF secret; nothing can be unlocked with that');
  state.wrapKey = await deriveWrapKey(out, v.salt);
}

function lock() {
  state.wrapKey = null; state.priv = null; state.name = null;
}

async function ensureUnlocked() {
  if (!state.wrapKey) await unlock();
  return state.wrapKey;
}

async function privateKey() {
  if (state.priv) return state.priv;
  const wrapKey = await ensureUnlocked();
  const k = await getKey();
  if (!k) throw new Error('no key on this phone yet');
  state.priv = await seedToPrivate(await unwrapSeed(wrapKey, k.wrapped));
  return state.priv;
}

async function sign(bytes) {
  return hex(await crypto.subtle.sign(ED, await privateKey(), bytes));
}

// ---------------------------------------------------------------- actions

async function setKey(seed) {
  const wrapKey = await ensureUnlocked();
  if (await getKey()) throw new Error('this phone already has a key; forget it first');
  const priv = await seedToPrivate(seed);
  const account = await publicOf(priv);
  await putKey({ account, createdAt: Date.now(), wrapped: await wrapSeed(wrapKey, seed) });
  seed.fill(0);
  return account;
}

async function exportSeed() {
  const wrapKey = await ensureUnlocked();
  return hex(await unwrapSeed(wrapKey, (await getKey()).wrapped));
}

// SCALE, just enough for the patron request: a compact length, then bytes.
function compact(n) {
  if (n < 64) return Uint8Array.of(n << 2);
  if (n < 16384) return Uint8Array.of(((n << 2) | 1) & 0xff, (n << 2) >>> 8);
  const v = (n << 2) | 2;
  return Uint8Array.of(v & 0xff, (v >>> 8) & 0xff, (v >>> 16) & 0xff, (v >>> 24) & 0xff);
}
function scaleBytes(b) { return [compact(b.length), b]; }
function concat(parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0; for (const p of parts) { out.set(p, o); o += p.length; }
  return out;
}
// (context.as_bytes(), domain [u8;32], name, note).encode()
function requestMessage(g, name, note) {
  return concat([...scaleBytes(enc.encode(g.request_context)), unhex(g.request_domain), ...scaleBytes(enc.encode(name)), ...scaleBytes(enc.encode(note))]);
}

async function api(path, init) {
  const r = await fetch(API + path, init);
  const text = await r.text();
  let body; try { body = JSON.parse(text); } catch (_) { body = { ok: false, error: text || r.statusText }; }
  return { status: r.status, body };
}

async function requestAccess(name, note) {
  const { account } = await getKey();
  const g = (await api('/genesis')).body;
  if (!g.request_domain) throw new Error('the API didn\'t answer with its genesis; is /api up?');
  if (name.length > g.max_name) throw new Error(`a nickname is at most ${g.max_name} characters`);
  if (enc.encode(note).length > g.max_note) throw new Error(`a note is at most ${g.max_note} bytes`);
  const sig = await sign(requestMessage(g, name, note));
  return api('/patron-request', {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-miot-request': '1' },
    body: JSON.stringify({ who: account, name, note, sig }),
  });
}

async function patronStatus(account) {
  const { body } = await api(`/patron/${account}`);
  return body.status || 'none';
}

// A signed read: the headers the node checks, over the query string (none here).
async function signedGet(path) {
  const { account } = await getKey();
  return api(path, { headers: { 'x-miot-signer': account, 'x-miot-sig': await sign(new Uint8Array(0)) } });
}

// The nickname is the chain's, not ours: once approved, read it back.
async function myName() {
  if (state.name) return state.name;
  const { account } = await getKey();
  const { status, body } = await signedGet('/patrons');
  if (status !== 200) throw new Error(`the teahouse wouldn't say (${status}): ${body.error || ''}`);
  const me = (body.approved || []).find((p) => p.account === account);
  state.name = me ? me.name : null;
  return state.name;
}

// ---------------------------------------------------------------- view

let statusTimer = null;
function status(msg, err) {
  const el = $('#status');
  el.textContent = msg || '';
  el.classList.toggle('err', !!err);
  clearTimeout(statusTimer);
  if (msg && !err) statusTimer = setTimeout(() => { el.textContent = ''; }, 6000);
}
const failing = (fn) => async (...a) => {
  try { status(''); await fn(...a); } catch (e) { status(e.message || String(e), true); }
  render();
};

let askStatusSeq = 0;
async function showAskStatus(k) {
  const el = $('#askstatus');
  const seq = ++askStatusSeq;
  el.textContent = 'checking with the teahouse…';
  try {
    const st = await patronStatus(k.account);
    if (seq !== askStatusSeq) return;
    $('#askform').hidden = st !== 'none';
    if (st === 'none') el.textContent = 'The teahouse doesn\'t know this key yet.';
    else if (st === 'pending') el.textContent = 'Approval pending.';
    else if (!state.wrapKey) el.textContent = 'Approved. Unlock to sign in.';
    else {
      const name = await myName();
      if (seq !== askStatusSeq) return;
      el.textContent = name ? `Signed in as ${name}.` : 'Approved, but the teahouse lists no nickname for this key.';
      $('#keycard .name').textContent = name || '';
    }
  } catch (e) {
    if (seq === askStatusSeq) el.textContent = `couldn't reach the API: ${e.message}`;
  }
}

function sealText(account) {
  return [0, 16, 32, 48].map((i) => account.slice(i, i + 16).replace(/(.{4})/g, '$1 ').trim()).join('\n');
}

function keyCard(k) {
  const el = document.createElement('div');
  el.className = 'key';
  el.innerHTML = `
    <pre class="seal" title="your account, as kot id prints it">${sealText(k.account)}</pre>
    <div class="facts">
      <div class="name">${state.name || ''}</div>
      <div class="meta">made ${new Date(k.createdAt).toLocaleString()}</div>
    </div>
    <div class="row">
      <button data-copy>Copy account</button>
      <button data-export>Export seed</button>
      <button data-delete class="danger">Forget key</button>
    </div>`;
  el.querySelector('[data-copy]').onclick = failing(async () => {
    await navigator.clipboard.writeText(k.account); status('account copied');
  });
  el.querySelector('[data-export]').onclick = failing(async () => showExport(await exportSeed()));
  el.querySelector('[data-delete]').onclick = failing(async () => {
    if (!confirm('Forget this key? If the seed isn\'t exported anywhere, the account is gone.')) return;
    await clearKey(); state.priv = null; status('key forgotten');
  });
  return el;
}

function showExport(seed) {
  $('#exportseed').textContent = seed;
  $('#export').hidden = false;
  $('#exportcopy').onclick = failing(async () => { await navigator.clipboard.writeText(seed); status('seed copied; paste it somewhere that isn\'t a chat'); });
  $('#exportsave').onclick = () => {
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([seed + '\n'], { type: 'text/plain' }));
    a.download = 'teahouse.seed'; a.click(); URL.revokeObjectURL(a.href);
  };
  $('#exportclose').onclick = () => { $('#export').hidden = true; $('#exportseed').textContent = ''; };
}

async function render() {
  state.vault = (await getVault()) || null;
  const has = !!state.vault;
  const k = has ? await getKey() : null;

  $('#setup').hidden = has;
  $('#keys').hidden = !has;
  $('#access').hidden = !k;
  $('#passkey').hidden = !has;

  if (has) {
    $('#keycard').replaceChildren(...(k ? [keyCard(k)] : []));
    $('#keymake').hidden = !!k;
    if (!k) $('#importform').hidden = true;
    $('#passkeystate').textContent = `Set up ${new Date(state.vault.createdAt).toLocaleString()} on this phone. Every unlock asks it for the secret the seed is encrypted with.`;
  }
  if (k) showAskStatus(k);

  const ll = $('#lockline');
  ll.classList.toggle('open', !!state.wrapKey);
  ll.replaceChildren();
  if (has) {
    const dot = document.createElement('span'); dot.className = 'dot';
    const txt = document.createElement('span'); txt.textContent = state.wrapKey ? 'unlocked' : 'locked';
    const btn = document.createElement('button'); btn.textContent = state.wrapKey ? 'Lock' : 'Unlock';
    btn.onclick = failing(async () => { state.wrapKey ? lock() : await unlock(); });
    ll.append(dot, txt, btn);
  }
}

// ---------------------------------------------------------------- wiring

async function main() {
  const problems = [];
  if (!window.isSecureContext) problems.push('the page isn\'t on https or localhost');
  if (!window.PublicKeyCredential) problems.push('no passkeys in this browser');
  try { await crypto.subtle.generateKey(ED, true, ['sign', 'verify']); } catch (_) { problems.push('no Ed25519 in this browser\'s WebCrypto'); }
  if (problems.length) {
    $('#unsupported').textContent = `This page can't work here: ${problems.join('; ')}. Use Safari on a current iPhone or Chrome on a current Android.`;
    $('#unsupported').hidden = false;
    return;
  }

  const toggle = (form, on) => { form.hidden = !on; if (on) form.querySelector('input,textarea').focus(); };
  $('#importkey').onclick = () => toggle($('#importform'), true);
  document.querySelectorAll('[data-cancel]').forEach((b) => (b.onclick = () => toggle(b.closest('form'), false)));

  $('#setuppasskey').onclick = failing(async () => {
    const vault = await registerPasskey();
    await putVault(vault);
    state.vault = vault;
    status('passkey set up; now make your key');
  });

  $('#newkey').onclick = failing(async () => {
    await ensureUnlocked(); // the passkey prompt first, straight off the tap
    const account = await setKey(await newSeed());
    status(`made ${account.slice(0, 8)}…; export the seed if you want it anywhere else`);
  });
  $('#importform').onsubmit = failing(async (e) => {
    e.preventDefault();
    const text = $('#importseed').value.trim().replace(/^0x/, '');
    if (!/^[0-9a-fA-F]{64}$/.test(text)) throw new Error('a seed is exactly 64 hex characters');
    const account = await setKey(unhex(text.toLowerCase()));
    toggle($('#importform'), false); $('#importseed').value = '';
    status(`imported ${account.slice(0, 8)}…`);
  });

  $('#startover').onclick = failing(async () => {
    if (!confirm('Forget the passkey and the key with it? A seed not exported elsewhere is gone. The passkey itself stays in your keychain until you delete it there.')) return;
    await wipe(); lock(); status('everything forgotten');
  });

  $('#askform').onsubmit = failing(async (e) => {
    e.preventDefault();
    const name = $('#askname').value.trim();
    const note = $('#asknote').value.trim();
    if (!/^[a-z0-9_-]{1,32}$/.test(name)) throw new Error('a nickname is 1 to 32 of: lowercase letters, digits, - and _');
    if (!note) throw new Error('write a note; it\'s what the approver reads');
    const { status: code, body } = await requestAccess(name, note);
    const out = $('#askout');
    out.hidden = false;
    out.textContent = `${code}\n${JSON.stringify(body, null, 2)}`;
    if (!body.ok) throw new Error(`refused: ${body.error}`);
    status(body.status === 'applied' ? 'request sent and on the chain' : 'request sent; it\'s queued until a primary takes it');
    $('#askname').value = ''; $('#asknote').value = '';
  });

  await render();
}

main().catch((e) => status(e.message || String(e), true));
