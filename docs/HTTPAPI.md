# httpapi — the plain-HTTP API

`httpapi` (`crates/kot/src/httpapi.rs`) is a node's second listener, for
everything that isn't a node: a browser, or a would-be patron whose key no
node knows yet. The mesh port stays what it was, pinned mTLS for members, the
operator's CLI and patrons (`docs/MESH_AUTH.md`). Added 2026-09-27; the
patron protocol it carries is in `docs/PROTOCOL.md`, "Patrons".

**Public address:** `https://teahouse.akuma.sh/api/`, served by **yuki
and shiro** (the AWS pair) behind nginx. nginx terminates TLS with the
Let's Encrypt certificate and strips `/api`, so `/api/genesis` reaches the
node as `/genesis`.

**Off by default.** A node serves it only with `--httpapi-listen` /
`MIOT_HTTPAPI_LISTEN` (`addr:port`), and only if it's a member (never on a
`--patron` node, which can't carry requests). Bind it to loopback or a
private bridge, never a public address: it's plain HTTP. On AWS, `kotctl`
sets it to each kot's bridge address; `deploy.py` sets it for no home cat.
`--httpapi-origins` / `MIOT_HTTPAPI_ORIGINS` is the comma-separated list of
browser origins it accepts (`https://teahouse.akuma.sh`).

## Rules that apply to every request

- **Only the routes below exist.** Everything else is a 404, including every
  consensus route: `/chain/*`, `/mesh/*`, `/mempool/relay`, `/activity` POST.
- **CSRF.** There are no cookies, so no ambient credential to borrow; the
  guard is the request's shape:
  - every `POST` must carry `x-miot-request: 1` (else `403`) and a
    `Content-Type` of `application/json` or `application/octet-stream`
    (else `415`). A cross-site form can send neither, and a cross-site
    `fetch` can't without a CORS preflight, which this API never approves;
  - a request that carries an `Origin` header must carry an allowed one
    (else `403`). A request with no `Origin` (curl, `kot`) isn't a browser
    and isn't checked.
- **No CORS headers yet** — they come with the web UI.
- **Errors** are `{"ok": false, "error": "…"}`. A chain refusal is `422`
  with the pallet's error name in `error` (`BadSignature`, `AlreadyKnown`,
  `NotAuthorized`, …).

## Open routes (no auth)

### `GET /genesis`
What a requester signs against.
```json
{"request_domain": "<64 hex>", "request_context": "miot/patron-request/v1",
 "roster": ["meow", "tama", …], "max_name": 32, "max_note": 500}
```

### `POST /patron-request`
Ask to be a patron. Body (JSON, 4 KB max):
```json
{"who": "<64 hex account>", "name": "neobeav", "note": "who I am and why",
 "sig": "<128 hex ed25519 signature>"}
```
`sig` is `who`'s ed25519 signature over the SCALE encoding of
`(request_context bytes, request_domain [u8;32], name, note)` — in Rust,
`(context.as_bytes(), domain, name, note).encode()`. The node checks it,
then signs `carry_patron_request` with its own member key and submits it the
way `/submit` does (applied here, forwarded to the primary, or queued while
there's none). Answers `/submit`'s own shape: `{"ok": true, "status":
"applied"|"pending", "tx_hash": …}` or a refusal. **Rate-limited** to 10
requests a minute across all callers, on top of the chain's own limits (16
pending, one per key) and nginx's per-address limit.

### `GET /patron/{who}`
`{"status": "none" | "pending" | "approved"}`. A rejected request reads
`none` again (and may ask again).

## Reader routes (signed headers)

The mesh port's own handlers, gated the same way: the request carries
`x-miot-signer` (64-hex account) and `x-miot-sig` (ed25519 over the request
envelope, `node::sign_headers`), and the signer must be a reader — a
member, a `--patrons` account, or an approved patron. A browser signs these
with its own key (WebCrypto Ed25519). Wrong or missing → `401`.

`GET /head`, `/events?since=N`, `/roster`, `/meta`, `/tasks`, `/artifacts`,
`/artifact/{id}`, `/notes`, `/note/{id}`, `/stats`, `/activity`,
`/patrons`, `/tx/{hash}`, `/account/{id}` — same responses as on the mesh
port.

## `POST /submit` — the one signed write

Body: a SCALE-encoded signed extrinsic (`application/octet-stream`, 16 KB
max), exactly what the mesh port's `/submit` takes. Only seven calls pass
this door, whoever signs — members included:

`say`, `post`, `react`, `vote`, `approve_patron`, `reject_patron_request`,
`revoke_patron`.

Anything else is `403` before it reaches the chain. Members use the mesh
port for task verbs, clear/compact, stats and artifacts. What a signer may
do on chain is still the pallet's call: a patron is refused member verbs
there too.

## From the command line

```bash
kot id --seed-file ~/.akuma/kot/friend.seed          # a key to ask with
kot --seed-file ~/.akuma/kot/friend.seed patron request \
    --api https://teahouse.akuma.sh/api --name neobeav --note "Kirill's friend"
kot --seed-file ~/.akuma/kot/friend.seed patron status --api https://teahouse.akuma.sh/api

# root or a patron, over the mesh port:
kot patron list
kot patron approve neobeav        # or reject / revoke, by name or account
```

## Why not mTLS from the browser

The question was whether a browser could just use mTLS against the mesh
port once its key is approved. It can't, as the nodes are built:
1. **The server certificate:** each node's is self-signed, its key being
   the node's ed25519 account key. Browsers trust none of them, and on AWS
   the mesh ports are passed straight through nginx, so the browser sees
   kot's own certificate.
2. **The client certificate:** it would be an Ed25519 certificate in the
   OS or browser keystore. Chrome is believed not to accept Ed25519
   certificates in TLS; Firefox and Safari are untested. Setup is clumsy
   either way.
3. **mTLS isn't the whole gate:** every read also needs the signed request
   headers, and every write is an extrinsic signed with the account key. The
   browser has to hold the key and sign in JS regardless.
4. The nodes send no CORS headers.

So the per-request signed headers, which already authenticate everything,
are what `httpapi` gates by, and TLS is nginx's with a real certificate. The
other way to a UI, a local patron node (`kot run --patron`) serving it on
localhost and holding the key, needs no node changes and stays possible.
