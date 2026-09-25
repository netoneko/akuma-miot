# Mesh auth — who a peer is, on the wire

Decision and implementation, 2026-09-23. Closes a gap that had nothing to do
with `/submit`'s signing: mesh-internal HTTP (election, chain sync) had none
of its own.

## Two signed things, not one

It's easy to conflate these because both are "a signature on the wire" — they
authorize different questions:

| | `/submit`'s `UncheckedExtrinsic` | mesh auth (this doc) |
|---|---|---|
| Question | *may this account cause this state change?* | *is this HTTP call really from a mesh peer?* |
| Checked by | `pallet-litter`'s dispatch, via `CheckNonce`/`CheckMortality`/etc | `node.rs`, at the HTTP boundary |
| Covers | `kot task open`, `say`, `plan`, ... | `/mesh/status`, `/mesh/vote`, `/chain/{head,blocks,checkpoint}` |
| Existed before | yes, and verified (`HANDOFF.md`) | **no** |

Before this change, `mesh_vote`'s handler (`crates/kot/src/node.rs`) trusted
`VoteRequest.candidate` as a plain string — not checked against
`MIOT_MEMBERS`, not tied to whoever actually sent the HTTP request. Anyone
who could reach the port could POST a vote request with a term higher than a
node's own and `on_vote_request`'s `adopt_term` would bump that node's
*persisted* election term from the unauthenticated input. `/chain/blocks`
and `/chain/checkpoint` served full block/state data to anyone who asked, and
a syncing replica had no way to confirm a response actually came from the
peer it thought it was pulling from.

## What changed

**Every mesh node now has its own keypair.** Before, `NodeConfig` carried no
identity at all unless the process also ran an agent loop (`--llm`/`--glm`),
in which case *that* identity was derived from the roster in `main.rs` and
used only for the agent's own `/submit` calls — never for the node's own
mesh traffic. Now `kot run --as <name>` derives an `Identity` from the
roster unconditionally (`main.rs::run`) and threads it into `NodeConfig` →
`Node`, whether or not an agent loop is attached.

**A signed envelope wraps mesh-internal HTTP, both directions.** Two custom
headers, `x-miot-signer` (hex account) and `x-miot-sig` (hex ed25519
signature), added to:

- `POST /mesh/vote` — request (the `VoteRequest` body) and response (the
  `VoteReply` body)
- `GET /mesh/status` — response (empty request body, since it takes no
  parameters — see below)
- `POST /mesh/status` (2026-09-25) — request (the caller's own `Status`,
  which must name the signer's account, or it's ignored) and response
- `POST /chain/push` (2026-09-25) — request (a `Push`: the leader's
  `Status`, which must name the signer, plus one op) and response (a
  `PushReply`)
- `GET /chain/head`, `GET /chain/checkpoint` — response
- `GET /chain/blocks` — response, and the request too, signed over the raw
  query string

The signature covers **exactly the bytes sent** — the raw request/response
body, or the raw query string for a parameterless GET — never a
re-serialized value. `Signed<T>`-as-a-nested-JSON-field was considered and
rejected: verifying it would mean re-serializing the parsed body and
checking byte-equality with what was signed, which only holds if JSON
round-trips deterministically (true for these particular structs — no maps,
fixed field order — but a fragile invariant to lean on going forward, and
untrue in general). Headers-plus-raw-bytes sidesteps the question entirely.

**Verification set:** `Node::is_trusted_signer` — genesis `members` plus
root and leader, the same set `genesis()` gives chain standing to `catnip`.
Not the same set `/submit` accepts (`CheckNonce` allows any account with
standing, which is broader); this is specifically "is this a mesh
participant," which is narrower and is what was missing.

**Client-facing endpoints were untouched in this first pass** — `/tasks`,
`/account/{id}`, `/artifact/{id}`, `/events`, `/head`, `/meta`, `/submit`
still served (or accepted) anything, unsigned. Only the five mesh-internal
endpoints above got the gate. **Closed 2026-09-23, same day:** this is a
private chain — the account universe is closed genesis `members` ∪ {root,
leader}, same as mesh — so an unsigned read of `/tasks`/`/events`/
`/artifacts`/etc. from a stranger who merely reaches the port is exactly the
gap the mesh-auth work above was closing for peer traffic, just not yet
closed for read traffic. Every client-facing GET (`/head`, `/events`,
`/tasks`, `/meta`, `/account/{id}`, `/artifact/{id}`, `/note/{id}`,
`/notes`, `/artifacts`, `/stats`, `/mesh/peers`) now runs through the same
`require_client_auth` → `verify_headers` → `is_trusted_signer` gate as
`/mesh/status` et al. (`node.rs`), and `kot`'s own client (`client.rs`,
`agent.rs`) signs every read it makes — a bare `kot` client already resolves
*some* identity for every invocation (`--seed`/`--seed-file`/`--as`, or the
operator's persisted `~/.akuma/miot/id_ed25519.seed` as the fallback), so
this cost it nothing new to lean on.

`/submit` is the one endpoint that stays unsigned at the header level: its
authority was never the HTTP envelope, it's the `UncheckedExtrinsic`'s own
signature, and `CheckNonce` already refuses any account with no genesis
`providers` standing. Adding the envelope there would be a second lock on
a door that was never open.

A replica forwarding `/account/{id}` to the primary now re-signs the
forwarded request as *itself* rather than relaying the original caller's
headers — it already verified the caller against its own `members` set
before deciding to forward, and a replica is itself always a trusted
member, so this is simpler than threading someone else's signature through
a second hop (`Route::Primary` carries the replica's own `Identity` for
exactly this).

**What this doesn't change:** transport is still plain `http://`, no TLS —
the envelope proves *who signed a request*, not that the bytes are private
in flight. **Closed the same day, third pass:** see "mTLS pinned to the same
keys" below — transport is no longer plain after that pass.

**Scope not covered:** the signature proves *a trusted mesh member sent
this*, not *the specific candidate named in this `VoteRequest` sent it* —
`Node` has no name→account roster to cross-check `VoteRequest.candidate`
against the signer's account, only the flat `members: Vec<AccountId>` list.
Closing that would mean threading a `Roster` (name→account map, the same
type `common::Roster` already is for the client side) into `NodeConfig`.
Left out of this pass; the gap it leaves is narrower than the one closed
(a trusted member could theoretically claim to be a different candidate
name, but still cannot forge being a member at all, and `on_vote_request`'s
own stickiness/pre-vote logic already discounts most of what that could buy
an attacker).

## HTTP/2, prior knowledge

Alongside the auth gate: `reqwest::Client` in `Node` now builds with
`.http2_prior_knowledge()` rather than negotiating per-connection HTTP/1.1.
Mesh-internal traffic is a tight poll loop between the same small set of
peers (`/mesh/status` every `poll_ms`, `/chain/*` every `sync_ms`) — prior
knowledge means one multiplexed connection instead of a fresh handshake per
call. These are plain `http://` routes with no TLS, so there's no ALPN to
negotiate over anyway; prior knowledge just skips straight to the HTTP/2
preface.

Server side needed one change: axum's `http2` feature (off by default —
only `http1` is in axum's default feature set). With it on, `axum::serve`
uses `hyper_util::server::conn::auto::Builder`, which sniffs the connection
preface and handles h1 and h2c on the same listener — no separate port, no
config, nothing peer-specific. `reqwest`'s own `http2` feature gates
`.http2_prior_knowledge()` and needed enabling too.

Verified live (not just via the in-process election test), 2026-09-23:

```
$ kot run --as solo --seed 1 --db /tmp/solo-smoke.db --port 19944 &

$ curl --http2-prior-knowledge http://127.0.0.1:19944/mesh/status
missing signer header          # unsigned request, correctly rejected
HTTP 401 over 2

$ # signed as root (a trusted genesis account):
status: 200 OK  version: HTTP/2.0
body: {"name":"solo","term":1,"role":"leader","leader":"solo","head":3,"head_term":1}
x-miot-signer: 8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c
x-miot-sig: 274d098fde6b31695bf270072a2378985a16f9bb3d8daa0caa5c3c529ccb4809420b33658f551525ac9847db2d3f16ad1ec9b7c826e7447a4c87915ce485f608
```

`cargo test --workspace` (108+ tests, including the 3-node `election.rs`
integration test — real HTTP, real signatures, a killed-and-revived
primary) passes unchanged. Only host-native testing has been done this
pass; nothing has been redeployed to the fleet (`docs/FLEET.md`) yet —
that's separate, later work, not covered by this doc.

## A doc example this broke

`kot run --as <name>` now needs `<name>` to resolve to a keypair — in the
roster, or via `--seed`/`--seed-file` — because every node signs its own
mesh traffic now, not just one running an agent loop. The bare `--as solo`
invocation that both `HANDOFF.md` and this repo's `CLAUDE.md` used to
document (`solo` isn't in `DEV_ROSTER`) fails with "run needs a keypair to
sign mesh traffic" until `--seed` is added. Both docs' example commands were
updated to `--seed 1` alongside this change.

## mTLS pinned to the same keys, 2026-09-23 (third pass, same day)

Prompted by the same AWS-deploy conversation as the read-auth pass above:
the header envelope proves who signed a request, but the wire itself was
still plaintext, and mesh-internal traffic (election, replication) would
have crossed the open internet unencrypted if a LAN node and an AWS node
ever needed to reach each other directly. Considered and rejected: a
VPN/tunnel (Tailscale) between mesh members — works, zero code, but every
peer added is an operational dependency outside this repo. Chosen instead:
every node's TLS identity *is* its `miot_keys::Identity`. No CA, no cert
provisioning — `crates/kot/src/tls.rs` builds a self-signed cert from the
account's own raw ed25519 seed (RFC 8410's fixed PKCS8 prefix + the 32-byte
seed) fresh on every process start, and a custom `rustls` verifier accepts a
peer's cert exactly when its embedded public key is in `is_trusted_signer`'s
set — the identical question the header envelope already asks, now asked of
the TLS handshake instead of (well, in addition to; the header envelope
stays) one request's bytes.

**Mutual, not one-directional:** the server requires a client cert too
(`ClientCertVerifier`, `client_auth_mandatory() == true`) — a `kot`
CLI/agent connection is pinned exactly like a mesh peer connection, since
both already resolve an `Identity` for every invocation regardless.

**TLS1.3 only**, deliberately — this is a mesh we control both ends of, not
a browser-facing server needing TLS1.2 fallback, so there's no reason to
carry that extra code path (or verify_tls12_signature's extra risk surface)
along. ALPN offers `h2` then `http/1.1`; `.http2_prior_knowledge()` is gone
from every `reqwest::Client` builder — there's a real handshake to negotiate
h2 over now, prior-knowledge mode was specifically for skipping that when
there wasn't one.

**A hard cutover, not a migration path.** Peer/node URLs are `https://` now,
not `http://` — reqwest only runs the TLS connector for that scheme, so
there is no way to keep the old spelling and change the wire underneath it.
Every mesh member needs the new binary and the new URL scheme at the same
time or the mesh can't talk to itself. Confirmed acceptable for this
deployment: a fresh genesis is coming anyway (a coordinated restart, not a
live migration), so there's no fleet currently depending on the old scheme
surviving a rolling upgrade. **Not yet applied to the fleet's own configs**
(`overlays/deploy/*`, `docs/TOPOLOGY.md`, `docs/runbooks/run-the-mesh.md`
still say `http://...:9944`) — deliberately left for the redeploy pass
itself rather than hand-edited here, since `deploy.sh`/`deploy.py`'s env
templating is exactly the fragile area `CLAUDE.md` already warns about
touching casually.

**Correctness of the SPKI check specifically:** pulling a cert's embedded
public key back out uses `x509_parser` rather than a hand-rolled byte
search — a byte search for the fixed Ed25519 SPKI prefix would be a
spoofing risk (a crafted cert could plant a decoy trusted key elsewhere in
its DER while the field the handshake signature is actually bound to is
different). `x509_parser` walks the real ASN.1 grammar, so the field it
returns is structurally the same one `verify_tls12/13_signature` binds the
signature check to.

Verified: four unit tests in `tls.rs` run a real loopback TLS1.3 handshake
(mutual success; a client the server doesn't trust refused; a server the
client doesn't trust refused; a cert cannot claim an account it wasn't
built from) — not mocks, an actual `TcpListener`/`TlsAcceptor`/
`TlsConnector` round trip. `cargo test --workspace` passes unchanged
otherwise, `election.rs`'s 3-node mesh included (now connecting over real
mTLS, replica-forwarded `/account` included). Live smoke test against the
real binary: a plain HTTP request against the port gets nothing (TLS only
now); `openssl s_client` with no client cert gets a `certificate_required`
TLS1.3 alert; a real `kot` client, signed and trusted, works normally.

## The operator's client stopped pinning the node, 2026-09-23

A client needed a local roster only to decide, at the first handshake, which
node certs to accept. Now it accepts any node's (valid, Ed25519) cert and
reads the roster from that node's `/roster`: the operator runs every node of
a private chain, so the node is trusted by fiat (`tls::client_config_any_node`,
used by `client.rs` only). The node still pins the client (`server_config`),
so a non-member is refused exactly as before, and node↔node and the agent
loop's own client stay fully pinned. The cost is a client-side MITM: something
on the path can pose as a node to a client. It can't relay the client to a
real node, since that needs a member's key to pass the node's pin. Revisit
when a client ever runs somewhere the path isn't the operator's.

## Patrons: readers outside the roster, 2026-09-25

Every gate above used to be one set, the genesis accounts. A friend of the
operator who wants to *watch* the chain from another network would have
needed a roster entry, which is a new genesis: every node restarted onto a
fresh chain at once. Patrons are the smaller thing that was actually
wanted.

- **On the members: `--patrons` / `MIOT_PATRONS`**, `name=pub:<64 hex>,…`.
  Per-node config, not genesis, so it rolls out node by node and can be
  left off some (the GLM cats, 2026-09-25). A listed account is a *reader*
  (`Node::is_reader`): the TLS handshake (`server_config` gets
  `reader_accounts`), `/mesh/status` (GET and POST), `/chain/{head,blocks,
  checkpoint}`, and every client-facing GET via `require_client_auth`.
  Still members-only: `/mesh/vote`, `/chain/push`, `/activity` POST.
- **A patron's status never reaches the election.** `mesh_status_post`
  answers a patron like anyone (that's how it learns who leads), notes
  what it sent in `patrons_seen` for `kot peers` (a `patrons` list in
  `/mesh/peers`), and returns before `Mesh::on_inbound`. A patron
  claiming to lead changes nothing. It isn't in anyone's `MIOT_PEERS`, so
  it isn't in anyone's quorum either.
- **Writes are refused at the door.** `/submit` has no envelope gate (the
  extrinsic's signature is the authority), so a patron can reach it. Its
  account was never given `providers` at genesis, so the chain would say
  `Invalid(Payment)` anyway. But a node with no primary queues a write in
  its mempool and relays it, and a local sim showed a patron's write
  answered "pending" that way. So `accept_extrinsic` now checks the signer
  first: not a genesis account, same `Invalid(Payment)` on the spot, nothing
  forwarded or queued. Checked in `tests/patron.rs`, to a member directly
  and via the patron's own node.
- **On the patron: `--patron` / `MIOT_PATRON=true`** makes its
  `Mesh` a learner (`miot-mesh`, "Learners"): it never campaigns and never
  votes, so it never produces, not even with no peers. Its own key is
  always a reader on its own node, so its operator's `kot` works against it.
- **It pulls from whoever it can reach.** A member pulls only from the
  primary. A patron on another network usually can't reach the primary
  (home has no router forwards; the AWS pair is what faces the internet),
  and nobody pushes to it, so `Mesh::pull_sources` gives a learner every
  fresh member in the current term that follows a leader: the leader
  first, then the furthest along. It stays on one source while that source
  is still valid, rather than switching every time two replicas swap by a
  block. `tests/patron.rs` runs it with the primary unreachable. With
  leader-only pulling it sat at head 0.

The limit: a patron keeps up only while it can reach some member that
lists it. With the GLM cats left off the list, a patron that reaches only
meow or tama gets refused at the handshake.

Running one, for the friend. Same genesis as the mesh (`kot roster` or
`/roster` gives `MIOT_ROSTER`; `MIOT_ROOT_PUBKEY` and `MIOT_LEADER` from the
operator), and only the members they can reach as peers. **The key must be
one `kot` can sign with**: their node proves it in every handshake, and `kot`
can't read an OpenSSH private key (`CLAUDE.md`, "Known gaps"). An
`ssh-ed25519` public line is fine for the operator to list, but the friend's
node then has nothing to sign with. The first account added this way,
neobeav's, is exactly that. So the friend makes a `kot` seed and sends its
public half:

```bash
kot id --seed-file ~/.akuma/kot/friend.seed        # prints the hex the operator adds to PATRONS
MIOT_ROSTER=... MIOT_ROOT_PUBKEY="ssh-ed25519 ..." MIOT_LEADER=... \
kot run --as neobeav --seed-file ~/.akuma/kot/friend.seed --patron \
  --peers https://kot.akuma.sh:9441,https://kot.akuma.sh:9442 --db ~/kot-patron.db
kot --node https://127.0.0.1:9944 --seed-file ~/.akuma/kot/friend.seed log --follow
```

The operator adds an account in `overlays/deploy/deploy.py` (`PATRONS`,
applied to `PATRONS_ON` by `deploy.py up`) and, for the AWS pair, one line
in `/etc/kot/patrons` there, then `kotctl sync`.

## Rejected alternatives

- **`sc-network`/`rust-libp2p`** (what Polkadot actually uses) — Kademlia
  DHT, gossipsub, noise, yamux. Disproportionate: mesh membership here is
  static (`CLAUDE.md`: "no joint consensus — fine for one operator"), so
  discovery is dead weight, and `sc-network` expects to run inside
  `sc-service`'s executor scaffolding, which `kot` deliberately doesn't have
  (native execution, no `sc-executor`). Rough estimate before measuring:
  40–80 MB added to a binary that's 9–11 MB today, plus real integration
  work, not a Cargo.toml line.
- **`Signed<T>` as a nested JSON field** — see "what changed" above:
  rejected for needing JSON round-trip determinism as an invariant.
- **gRPC** — raised again 2026-09-23 alongside the mTLS pass above ("switch
  traffic to encrypted gRPC, we do know the target host public key").
  Decoupled on purpose: the pubkey-pinning idea is sound and is exactly what
  the mTLS pass above does, but gRPC itself would mean `tonic`/`prost` and
  redefining every mesh/chain/client endpoint as a `.proto` service — a
  framework migration, not a transport change, and not required to get
  encryption-via-pinned-keys. Still a longer-term preference (would also
  ride HTTP/2, so the connection-reuse work here isn't wasted; signing would
  move from a header pair to an interceptor); still out of scope.
