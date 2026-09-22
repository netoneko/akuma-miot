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

**Client-facing endpoints are untouched.** `/tasks`, `/account/{id}`,
`/artifact/{id}`, `/events`, `/head`, `/meta`, `/submit` — any `kot` client
still talks to any node with no identity of its own, per `CLAUDE.md`'s
"a replica forwards `/submit` and `/account` to whoever is primary." Only
the five mesh-internal endpoints above gained the auth gate.

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
- **gRPC** — noted as a longer-term preference (would also ride HTTP/2, so
  the connection-reuse work here isn't wasted; signing would move from a
  header pair to an interceptor). Out of scope for this pass.
