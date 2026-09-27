# The web UI — a key on a phone, locked by a passkey

`public/` is the teahouse's browser side: three static files, no build,
hosted at the site root with `httpapi` (`docs/HTTPAPI.md`) under `/api/` on
the same origin. Target platform is a current iPhone in Safari, then Android
Chrome; there are no fallbacks for a browser without passkey PRF or WebCrypto
Ed25519, because this is a private chain with a handful of people on it.
Written 2026-09-27, the day the first cut was built.

The one idea: **the passkey is the lock, the Ed25519 key is the account.**
A passkey can't be the account, because WebAuthn signs only its own
challenge, never the bytes the chain wants (`x-miot-sig` over a query
string, an extrinsic). So the phone holds a real Ed25519 seed, the same 32
bytes `kot --seed-file` reads, and the passkey's PRF secret is what that
seed is encrypted under.

## What the person sees

```
┌──────────────────────────────┐   tap: Make my key          ┌───────────────────────────────┐
│  START                       │ ──────────────────────────► │  YOUR KEY                     │
│  Make my key                 │   passkey created           │  ┌ seal ─────────┐            │
│  I already have a seed ──►   │   (Face ID / Touch ID;      │  │ 5d5c 7501 …   │ copy       │
│      paste 64 hex            │    PRF must be enabled,     │  └───────────────┘ export     │
│                              │    else refused), then the  │                    forget key │
│                              │   seed is made or imported  │                               │
│                              │   and wrapped under it      │  ACCESS, by GET /api/patron/  │
└──────────────────────────────┘                             │  {account}:                   │
                                                             │   none     "Request access"   │
      ● locked  [Unlock]  ⇄  ● unlocked  [Lock]              │            nickname + note ─► │
      Unlock = the passkey prompt = sign-in.                 │            POST /api/         │
      Lock drops the wrap key and the private                │            patron-request     │
      key from memory; the account (public) stays            │   pending  "Access requested" │
      visible, so the seal and the status check              │            Approval pending.  │
      never need a prompt.                                   │   approved "Signed in"        │
                                                             │            locked: Unlock to  │
                                                             │              sign in.         │
                                                             │            unlocked: You are  │
                                                             │              <name>. ◄── GET  │
                                                             │              /api/patrons,    │
                                                             │              signed; the name │
                                                             │              is the chain's   │
                                                             └───────────────────────────────┘
      forget key ──► back to Make a key / Import a seed (same passkey)
      forget passkey and key ──► START; a seed not exported is gone

  approved + unlocked ──► CHAT (once per unlock; the Key/Chat button in the
                          header goes back and forth without re-triggering)
┌──────────────────────────────────────────────────────────────────────────┐
│  CHAT: the page is the scrollback, newest at the bottom                  │
│    on entry:  GET /api/roster, /api/patrons  (signed) → account → name   │
│               GET /api/events?since=0        (signed over "since=0")     │
│               every event the node holds, oldest first                   │
│    then every 4 s:  GET /api/head → seq                                  │
│               seq > cursor  → GET /api/events?since=<cursor>, append,    │
│                               scroll down only if already at the bottom  │
│               seq < cursor  → the node rebuilt its log (a /clear, a      │
│                               rewind): clear and replay from 0           │
│    said / message → a speech block: name, time, body ("to <name>" for    │
│                     a DM; the phone's own account in seal red)           │
│    everything else → one dim line, the same sentence `kot log` prints    │
│    no composer yet: a `say` is a signed extrinsic, the next step         │
└──────────────────────────────────────────────────────────────────────────┘
```

## What the phone holds, and what only exists while unlocked

```
IndexedDB "teahouse"                       memory (state), gone on Lock or reload
┌──────────────────────────────┐           ┌──────────────────────────────┐
│ vault                        │           │ wrapKey   AES-GCM CryptoKey  │
│   credId   passkey id        │           │ priv      Ed25519 CryptoKey  │
│   salt     32 random bytes   │           │ name      from /patrons      │
│   createdAt                  │           └──────────────────────────────┘
│ keys (one record)            │
│   account  64 hex (public)   │  ← readable without unlocking: the seal,
│   createdAt                  │    the status check, "copy account"
│   wrapped  {iv, ct}          │  ← the seed, AES-GCM under wrapKey
└──────────────────────────────┘
```

## Unlock, and how a signature comes out of it

```
 tap Unlock / any action that needs the key
   │
   ▼
 navigator.credentials.get({ allowCredentials: [vault.credId],
                             userVerification: 'required',
                             extensions: { prf: { eval: { first: vault.salt } } } })
   │                                   Face ID / Touch ID
   ▼
 prf.results.first ── 32 bytes, stable for (this passkey, this salt),
   │                   computable by nothing else
   ▼
 HKDF-SHA256(secret, salt = vault.salt, info = "teahouse seed vault v1")
   │
   ▼
 wrapKey (AES-GCM-256) ──── decrypt keys.wrapped ────► seed (32 bytes)
                                                          │
                                       PKCS#8 prefix ‖ seed → importKey('pkcs8', Ed25519)
                                                          │
                                                          ▼
                                                   priv (CryptoKey)
                                                          │
          ┌───────────────────────────────────────────────┼──────────────────────────┐
          ▼                                               ▼                          ▼
  signed read                                    patron request                  (later) an extrinsic
  sig = sign(priv, query string or "")           sig = sign(priv, SCALE(          say / post / react …
  headers: x-miot-signer: account                  context, domain, name, note))   POST /api/submit
           x-miot-sig: sig                       POST /api/patron-request
  GET /api/patrons, /api/head, …                   {who, name, note, sig}
```

The SCALE encoding is done by hand in `app.js` (`compact`, `scaleBytes`,
`requestMessage`): a compact length before each byte string, the 32-byte
domain raw, which is exactly `(PATRON_REQUEST_CONTEXT, request_domain(),
name, note).encode()` in the pallet. Checked against a real node: the
request landed and read `pending`; the same body under a different signer
was refused `422 BadSignature`.

## What isn't true, so nobody assumes it

- **The server never sees the passkey.** No WebAuthn registration or
  assertion goes to the API; the challenge is random and local. The passkey
  authenticates the person *to the phone*, and the Ed25519 signature
  authenticates the phone *to the chain*. This is deliberate: it keeps
  `httpapi` free of a WebAuthn verifier, and the account stays what it is
  everywhere else, an ed25519 public key.
- **Forgetting the passkey forgets the seed.** There is no recovery other
  than a seed exported earlier. Export shows the 64 hex and saves
  `teahouse.seed`.
- **One key per phone.** It is who you are. The nickname is the chain's:
  sent in the request, read back from `/patrons` after approval.
- **The chat exists only while unlocked.** Every read and the `say` are
  signed with the phone's key; Lock leaves the chat. `/api/submit` lets
  seven calls through (`docs/HTTPAPI.md`); the page sends only `say`.
- **PRF and Ed25519 are required.** Setup refuses a passkey that comes back
  without `prf.enabled`; the page refuses to start without WebCrypto
  Ed25519 or a secure context.

## Running it locally

```
./target/debug/kot run --as solo --roster "solo=<64 hex>" --db /tmp/solo.db \
    --port 9966 --bind 127.0.0.1 --httpapi-listen 127.0.0.1:9955 \
    --httpapi-origins http://localhost:8080
python3 overlays/local/webdev.py           # public/ at :8080, /api → :9955
```

`localhost` is a secure context, so Touch ID on the Mac drives the passkey
with PRF in Safari and Chrome. Safari's Responsive Design Mode gives the
iPhone frame for layout. To approve a request on that one-node chain:
`kot --node http://127.0.0.1:9966 --seed 1 patron approve <nickname>`
(root is the dev seed there).
