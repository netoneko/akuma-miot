# public/ — the teahouse web UI

Static files, no build: `index.html`, `style.css`, `app.js`. Hosted at the
site root (`teahouse.akuma.sh`), with `httpapi` at `/api/` on the same
origin (`docs/HTTPAPI.md`), so the page never needs CORS.

Step one (2026-09-27): one Ed25519 key per phone, and a request to join. Target is
Safari on a current iPhone, then Chrome on Android; no fallbacks for
browsers without passkey PRF or WebCrypto Ed25519.

- The key is made by WebCrypto (`Ed25519`), or imported from a seed. The
  account is the raw public key as 64 hex, the same string `kot id` prints.
  The seed is the 32-byte private key, exportable as the 64 hex a
  `kot --seed-file` reads. One key per phone: it is who you are.
- The seed is stored in IndexedDB, AES-GCM-wrapped under a key derived (HKDF,
  per-vault salt) from the passkey's PRF output. A passkey must exist before
  the key can; one that can't produce a PRF secret is refused at setup.
  Forgetting the passkey forgets the seed.
- A passkey can never be the account: WebAuthn signs only its own
  challenge, and the API needs ed25519 over the raw query string
  (`x-miot-signer` / `x-miot-sig`) and over extrinsics.
- "Request access" is `POST /api/patron-request`: the page fetches
  `/api/genesis`, SCALE-encodes `(context, domain, name, note)` itself (a
  compact length before each byte string, the domain raw), signs it with
  the phone's key, and shows the chain's answer. `/api/patron/{who}` gives the
  key's standing: none shows the form, pending says so, approved reads the
  nickname back from a signed `/api/patrons` once the key is unlocked.

- Once approved and unlocked the page becomes the chat: a signed
  `/api/events?since=0` replays everything the node holds, `/api/head` is
  polled every 4 s to follow, and each effect is rendered with the same
  sentence `kot log` uses. The composer sends `say` as a signed extrinsic
  built in `app.js` (`blake2b.js` for the long-payload hash), checked byte
  for byte against `cargo run -p miot-runtime --example tx_vectors`.

Local dev: `python3 overlays/local/webdev.py` and open
`http://localhost:8080`. Passkeys work on localhost; `file://` has no WebAuthn and no IndexedDB worth trusting. Without an `/api` behind it, the
access form reports the 404. `overlays/local/webdev.py` serves the page and
proxies `/api` to a node started with `--httpapi-listen 127.0.0.1:9955`.
