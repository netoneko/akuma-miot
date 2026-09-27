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
// `autoChat`: one automatic jump into the chat per unlock; the Key button
// then stays put until the next unlock.
const state = { vault: null, wrapKey: null, priv: null, name: null, view: 'key', autoChat: false };

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
  // Some authenticators evaluate PRF at creation; then the first unlock is free.
  const first = ext.prf.results && ext.prf.results.first;
  return { vault: { credId: new Uint8Array(cred.rawId), salt, createdAt: Date.now() }, secret: first || null };
}

// The one setup step: a passkey, then the key locked under it.
async function setup(seed) {
  const { vault, secret } = await registerPasskey();
  await putVault(vault);
  state.vault = vault;
  if (secret) { state.wrapKey = await deriveWrapKey(secret, vault.salt); state.autoChat = true; }
  return setKey(seed);
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
  state.autoChat = true;
}

function lock() {
  state.wrapKey = null; state.priv = null; state.name = null;
  leaveChat();
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
async function signedGet(path, query = '') {
  const { account } = await getKey();
  return api(path, { headers: { 'x-miot-signer': account, 'x-miot-sig': await sign(enc.encode(query)) } });
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

// ---------------------------------------------------------------- chat

// account hex → name: the roster, then approved patrons, then root (a Said
// from root carries `root: true`, and that account is remembered, so root's
// approvals read as root's too). Anything else shows as its first 8 hex.
const names = new Map();
async function loadNames() {
  const r = await signedGet('/roster');
  for (const m of r.body || []) names.set(m.account, m.name);
  const p = await signedGet('/patrons');
  for (const m of (p.body && p.body.approved) || []) names.set(m.account, m.name);
}
const nameOf = (a) => (a ? names.get(a) || a.slice(0, 8) : '?');

// One event → what to show. Same sentences as `kot log` (client.rs
// render_effect), so the phone and the terminal agree on what happened.
function describe(eff) {
  const t = eff.t || '';
  const n = (f) => nameOf(eff[f]);
  const task = () => eff.task || '?';
  const otr = eff.off_record ? ' (off the record)' : '';
  switch (t) {
    case 'said': return { speech: true, from: eff.from, who: n('from'), to: eff.to ? nameOf(eff.to) : null, body: eff.body + otr };
    case 'message': {
      let extra = eff.parent != null ? ` ↩#${eff.parent}` : '';
      for (const tag of eff.tags || []) extra += ` #${tag}`;
      return { speech: true, from: eff.from, who: n('from'), to: null, body: eff.body + otr + extra };
    }
    case 'reacted': return { text: `${n('who')} reacted ${eff.emoji} on #${eff.target}` };
    case 'voted': return { text: `${n('who')} voted ${eff.up ? 'up' : 'down'} on §${eff.artifact}` };
    case 'patron_requested': return { text: `${eff.name} asked to be a patron (carried by ${n('carrier')}): ${eff.note}` };
    case 'patron_approved': return { text: `${n('by')} approved ${eff.name} as a patron` };
    case 'patron_rejected': return { text: `${n('by')} turned down ${n('who')}'s patron request` };
    case 'patron_revoked': return { text: `${n('by')} revoked patron ${n('who')}` };
    case 'opened': return { text: `${n('who')} opened ${task()}: ${eff.text}` };
    case 'planned': return { text: `${n('who')} planned ${task()} into ${eff.count} subtask(s)` };
    case 'assigned': return { text: `${task()} assigned to ${n('to')}: ${eff.what}` };
    case 'directed': return { text: `${n('to')} directed on ${task()}: ${eff.directive}` };
    case 'nudge': return { text: `${n('to')} nudged on ${task()} (${eff.remaining} left${eff.last ? ', last' : ''})` };
    case 'record': return { text: `${n('who')} ${eff.act} on ${task()}${eff.text ? ': ' + eff.text : ''}` };
    case 'requeued': return { text: `${task()} requeued from ${n('from')}: ${eff.why}` };
    case 'budget_spent': return { text: `${n('holder')} spent its nudge budget on ${task()}` };
    case 'closed': return { text: `${task()} closed by ${n('author')}: ${eff.title}` };
    case 'failed': return { text: `${task()} failed` };
    case 'rehomed': return { text: `${task()} rehomed from ${n('from')} to ${n('to')}` };
    case 'standalone_artifact': return { text: `${n('author')} published artifact ${eff.id}: ${eff.title}` };
    case 'stats_reported': return { text: `${n('who')} reported its stats` };
    default: return { text: JSON.stringify(eff) };
  }
}

// `replaying` is true only during the first pull. A live entry is served
// before its block seals (node.rs pushes it with `at: None` and stamps it
// at seal), so a followed event without `at` is stamped with now.
const chat = { cursor: 0, timer: null, lastDay: null, me: null, hurry: 0, replaying: false };
const clock = (ms) => new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
const dayOf = (ms) => new Date(ms).toDateString();

function appendEvent(e) {
  const log = $('#log');
  if (!e.at && !chat.replaying) e = { ...e, at: Date.now() };
  if (e.at && dayOf(e.at) !== chat.lastDay) {
    chat.lastDay = dayOf(e.at);
    const d = document.createElement('div'); d.className = 'day';
    d.textContent = new Date(e.at).toLocaleDateString([], { weekday: 'long', day: 'numeric', month: 'long' });
    log.append(d);
  }
  const d = describe(e.effect || {});
  const el = document.createElement('div');
  if (d.speech) {
    el.className = 'msg' + (d.from === chat.me ? ' me' : '');
    const who = document.createElement('div'); who.className = 'who';
    who.textContent = d.who;
    if (d.to) { const to = document.createElement('span'); to.className = 'to'; to.textContent = ` to ${d.to}`; who.append(to); }
    const stamp = document.createElement('span'); stamp.className = 'stamp'; stamp.textContent = e.at ? clock(e.at) : '';
    who.append(stamp);
    const body = document.createElement('div'); body.className = 'body'; body.textContent = d.body;
    el.append(who, body);
  } else {
    el.className = 'line';
    const stamp = document.createElement('span'); stamp.className = 'stamp'; stamp.textContent = e.at ? clock(e.at) : '';
    el.append(stamp, document.createTextNode(d.text));
  }
  log.append(el);
}

// Replay from the start, then follow. `/events` is signed over its query
// string; a head seq below our cursor means the node rebuilt its log
// (a /clear or a rewind), so we start over: HANDOFF, "seq restarts".
async function pullEvents() {
  const q = `since=${chat.cursor}`;
  const { status, body } = await signedGet(`/events?${q}`, q);
  if (status !== 200) throw new Error(`events: ${status} ${body.error || ''}`);
  const nearBottom = window.innerHeight + window.scrollY >= document.body.scrollHeight - 120;
  // Learn root before rendering, so its approvals earlier in the log read as root's.
  for (const e of body) { const f = e.effect || {}; if (f.t === 'said' && f.root && f.from) names.set(f.from, 'root'); }
  for (const e of body) { appendEvent(e); chat.cursor = Math.max(chat.cursor, e.seq); }
  if (body.length && nearBottom) window.scrollTo(0, document.body.scrollHeight);
  return body.length;
}
async function followOnce() {
  const { status, body } = await signedGet('/head');
  if (status !== 200) throw new Error(`head: ${status}`);
  if (body.seq < chat.cursor) { chat.cursor = 0; chat.lastDay = null; $('#log').replaceChildren(); status('the node rebuilt its log; replaying'); }
  if (body.seq > chat.cursor) await pullEvents();
  $('#chatfoot').textContent = `block ${body.block}, ${chat.cursor} events`;
}
async function enterChat() {
  if (state.view === 'chat') return;
  state.view = 'chat';
  chat.me = (await getKey()).account;
  chat.cursor = 0; chat.lastDay = null; $('#log').replaceChildren();
  await loadNames();
  chat.replaying = true;
  try { await pullEvents(); } finally { chat.replaying = false; }
  window.scrollTo(0, document.body.scrollHeight);
  await followOnce();
  chat.timer = setInterval(() => {
    if (chat.hurry > 0) chat.hurry--; else if (Date.now() % 4000 >= 1000) return;
    followOnce().catch((e) => { $('#chatfoot').textContent = e.message; });
  }, 1000);
}
function leaveChat() {
  clearInterval(chat.timer); chat.timer = null;
  state.view = 'key';
}

// ---------------------------------------------------------------- sending

// A `say` as the chain takes it: a signed extrinsic, legacy v4 layout, the
// same bytes `miot_runtime::client::sign` produces (checked against
// `cargo run -p miot-runtime --example tx_vectors`).
//
//   extrinsic  = compact(len) ‖ 0x84 ‖ account(32) ‖ 0x00 ‖ sig(64) ‖ extra ‖ call
//   call       = 0x01 (Litter) ‖ 0x06 (say) ‖ Option<to> ‖ String body ‖ no_ack ‖ off_record
//   extra      = 0x00 (immortal era) ‖ compact(nonce)
//   signed     = call ‖ extra ‖ genesis(32) ‖ spec u32 LE ‖ tx u32 LE ‖ genesis(32)
//                (blake2-256 of that when it is longer than 256 bytes)
const u32le = (n) => Uint8Array.of(n & 0xff, (n >>> 8) & 0xff, (n >>> 16) & 0xff, (n >>> 24) & 0xff);
function encodeSay(to, body, offRecord = false) {
  return concat([Uint8Array.of(1, 6), to ? concat([Uint8Array.of(1), unhex(to)]) : Uint8Array.of(0), ...scaleBytes(enc.encode(body)), Uint8Array.of(0), Uint8Array.of(offRecord ? 1 : 0)]);
}
async function signExtrinsic(call, nonce, meta) {
  const { account } = await getKey();
  const extra = concat([Uint8Array.of(0), compact(nonce)]);
  const genesis = unhex(meta.genesis_hash.replace(/^0x/, ''));
  const payload = concat([call, extra, genesis, u32le(meta.spec_version), u32le(meta.tx_version), genesis]);
  const signedOver = payload.length > 256 ? blake2b(payload, 32) : payload;
  const sig = new Uint8Array(await crypto.subtle.sign(ED, await privateKey(), signedOver));
  const inner = concat([Uint8Array.of(0x84), unhex(account), Uint8Array.of(0), sig, extra, call]);
  return concat([compact(inner.length), inner]);
}
async function say(body, to) {
  const { account } = await getKey();
  const meta = (await signedGet('/meta')).body;
  if (!meta.genesis_hash) throw new Error('the API gave no chain meta');
  const acct = (await signedGet(`/account/${account}`)).body;
  if (typeof acct.nonce !== 'number') throw new Error(`no nonce for this key: ${acct.error || 'not on chain'}`);
  const xt = await signExtrinsic(encodeSay(to, body), acct.nonce, meta);
  const { status, body: r } = await api('/submit', {
    method: 'POST', headers: { 'content-type': 'application/octet-stream', 'x-miot-request': '1' }, body: xt,
  });
  if (!r.ok) throw new Error(`refused (${status}): ${r.error || JSON.stringify(r)}`);
  return r;
}

// "@name the rest" whispers to name; the reverse of the names map.
function parseAddress(text) {
  const m = /^@([a-z0-9_-]+)\s+([\s\S]+)$/i.exec(text);
  if (!m) return { to: null, body: text };
  const want = m[1].toLowerCase();
  for (const [account, name] of names) if (name.toLowerCase() === want) return { to: account, body: m[2] };
  throw new Error(`nobody here is called ${m[1]}`);
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
    $('#accesslede').hidden = st !== 'none';
    $('#accesstitle').textContent = { none: 'Request access', pending: 'Access requested', approved: 'Signed in' }[st] || 'Access';
    if (st === 'none') el.textContent = 'The teahouse doesn\'t know this key yet.';
    else if (st === 'pending') el.textContent = 'Approval pending.';
    else if (!state.wrapKey) el.textContent = 'Approved. Unlock to sign in.';
    else {
      const name = await myName();
      if (seq !== askStatusSeq) return;
      el.textContent = name ? `You are ${name}.` : 'Approved, but the teahouse lists no nickname for this key.';
      $('#keycard .name').textContent = name || '';
      if (name && state.autoChat && state.view !== 'chat') { state.autoChat = false; await enterChat(); render(); }
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

  const inChat = state.view === 'chat' && !!state.wrapKey;
  $('#setup').hidden = has;
  $('#keys').hidden = !has || inChat;
  $('#access').hidden = !k || inChat;
  $('#passkey').hidden = !has || inChat;
  $('#chat').hidden = !inChat;

  if (has) {
    $('#keycard').replaceChildren(...(k ? [keyCard(k)] : []));
    $('#keymake').hidden = !!k;
    if (!k) $('#importform').hidden = true;
    $('#keys .lede').hidden = !!k;
    $('#passkeystate').textContent = `Set up ${new Date(state.vault.createdAt).toLocaleString()} on this phone. Every unlock asks it for the secret the seed is encrypted with.`;
  }
  if (k && !inChat) showAskStatus(k);

  const ll = $('#lockline');
  ll.classList.toggle('open', !!state.wrapKey);
  ll.replaceChildren();
  if (k) {
    const dot = document.createElement('span'); dot.className = 'dot';
    const txt = document.createElement('span'); txt.className = 'state'; txt.textContent = state.wrapKey ? 'unlocked' : 'locked';
    const btn = document.createElement('button'); btn.textContent = state.wrapKey ? 'Lock' : 'Unlock';
    btn.onclick = failing(async () => { state.wrapKey ? lock() : await unlock(); });
    ll.append(dot, txt);
    if (state.wrapKey && state.name) {
      const view = document.createElement('button'); view.className = 'view';
      view.textContent = inChat ? 'Key' : 'Chat';
      view.onclick = failing(async () => { if (inChat) { leaveChat(); } else { await enterChat(); } });
      ll.append(view);
    }
    ll.append(btn);
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
    const account = await setup(await newSeed());
    status(`your key is ${account.slice(0, 8)}…; locked with the passkey`);
  });
  $('#setupimport').onclick = () => toggle($('#setupimportform'), true);
  $('#setupimportform').onsubmit = failing(async (e) => {
    e.preventDefault();
    const text = $('#setupseed').value.trim().replace(/^0x/, '');
    if (!/^[0-9a-fA-F]{64}$/.test(text)) throw new Error('a seed is exactly 64 hex characters');
    const account = await setup(unhex(text.toLowerCase()));
    $('#setupseed').value = '';
    status(`imported ${account.slice(0, 8)}…; locked with the passkey`);
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

  const composer = $('#composer');
  const bodyEl = $('#saybody');
  bodyEl.oninput = () => { bodyEl.style.height = 'auto'; bodyEl.style.height = Math.min(bodyEl.scrollHeight, 128) + 'px'; };
  bodyEl.onkeydown = (e) => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); composer.requestSubmit(); } };
  composer.onsubmit = failing(async (e) => {
    e.preventDefault();
    const text = bodyEl.value.trim();
    if (!text) return;
    const { to, body } = parseAddress(text);
    $('#saysend').disabled = true;
    try {
      const r = await say(body, to);
      bodyEl.value = ''; bodyEl.style.height = 'auto';
      chat.hurry = 12;
      status(r.status === 'applied' ? 'sent' : 'sent; queued until a primary takes it');
    } finally { $('#saysend').disabled = false; }
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
