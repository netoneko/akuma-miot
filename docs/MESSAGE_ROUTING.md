# How a message actually gets there

Written 2026-09-25, the day root hit `503 "the primary is ryzen-linux-amd64,
but this node has no route to it (it follows by push); submit to another
node"` from the REPL and asked "we need to think how to solve it." This is
the network layer under every `Said`/`TaskUpdate`/`Artifact` and the rest of
`miot_primitives::Effect`: how a signed extrinsic gets from whichever node a
client happened to be talking to, to sealed on the actual chain, when the
mesh's connectivity is asymmetric rather than a full mesh. `docs/MESH_AUTH.md`
covers *who* a peer is on the wire (mTLS, pinned to genesis keys); this page
covers *where a write goes* once that handshake is done.

## The network, as topology rather than as a diagram of intent

Every mesh member polls every other member's `/mesh/status`, and a candidate
POSTs `/mesh/vote` to every peer — that's how elections and quorum work, and
it needs no reply route, only an outbound one. Blocks move by pull (a
follower GETs `/chain/blocks` from whoever it currently follows) or, since
2026-09-25, by the primary's own push to a peer whose head is stuck
(`Mesh::push_targets`/`push_to`, `HANDOFF.md` "One-way reachability"). None of
that requires *every* pair of members to reach each other — it only requires
that status/votes/blocks flow along whatever links are actually configured
(`MIOT_PEERS`), and that the graph those links form stays connected enough for
quorum.

The teahouse's actual graph is not symmetric (`docs/TEAHOUSE.md`):

```
                    home LAN (fully connected)              the far pavilion
        ┌─────────────────────────────────────┐        ┌─────────────────────┐
        │  meow ── tama ── kuro ── mimi ── sora │        │   yuki  ──  shiro   │
        └──────────────────┬────────────────────┘        └──────────┬──────────┘
                            │                                        │
                            │  push + pull, both directions work     │
                            └───────────────►───────────────────────►┘
                                  (leader → AWS: reachable,
                                   security group 9441-9460 open)

                            ◄╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
                                  AWS → home: no route.
                            router forwards 9944-9948 not in
                            (`docs/runbooks/deploy-aws-node.md` §1)
```

Home can always reach AWS (the EC2 security group has 9441-9460 open to
`0.0.0.0/0`); AWS can never reach home (nothing on the home router forwards
those ports inbound yet). So when the primary is at home, yuki and shiro
*follow* it fine — the primary pushes them blocks — but they have no way to
*call* it. That asymmetry, not any single node being "down," is what a 503
like the one that started this page actually means: not an election in
progress, not a crash, just a write that landed on a node with no configured
path to whoever is currently allowed to seal it.

## Where a write goes: one node's decision

Every client-facing write (`/submit`) and every read that needs the primary's
own state (`/account`, and now `/tx/{hash}`, below) asks the same three-way
question, `node.rs::route()`:

```
                              is this node
                              the primary?
                                   │
                 ┌─────────yes────┴────no──────────┐
                 ▼                                  ▼
          ┌─────────────┐                is there a *configured*
          │    Here      │                peer URL for whoever
          │ apply it now │                the mesh says leads?
          └─────────────┘                          │
                                    ┌──────yes──────┴──────no───────┐
                                    ▼                                ▼
                          ┌──────────────────┐              ┌───────────────────┐
                          │      Primary      │              │      Nobody       │
                          │ forward the raw   │              │ queue it locally  │
                          │ bytes to that URL │              │ (`Node::mempool`) │
                          │ and hand back its │              │ and answer 200    │
                          │ answer verbatim   │              │ "pending" — see   │
                          └──────────────────┘              │ below, not a 503   │
                                                              └───────────────────┘
```

This is *static config*, not a live reachability probe — `Nobody` fires
whenever this node's own `MIOT_PEERS` has no entry for the name the mesh's
gossip says currently leads, whether that's because the link genuinely
doesn't exist (yuki/shiro → home) or because an election just handed
leadership to someone this node's config was never updated for. Before
2026-09-25, `Nobody` was a hard refusal: `no_primary()` returned 503 and the
extrinsic was gone. Root hit exactly that from the REPL, on a node three hops
from any AWS box, talking about a primary that had simply rotated to a
different home member.

## The message lifecycle, end to end

```
 client                node A                  node B (peer)         primary
   │  POST /submit         │                        │                    │
   ├──────────────────────►│                         │                    │
   │                       │ route() = Nobody         │                    │
   │                       │ (no peer URL for         │                    │
   │                       │  today's leader)         │                    │
   │  200 "pending"        │                          │                    │
   │  {tx_hash, note}      │                          │                    │
   │◄──────────────────────┤                          │                    │
   │                       │ queued in Node::mempool   │                    │
   │                       │ (bounded, 15-min TTL)     │                    │
   │                       │                           │                    │
   │             every mesh tick (mempool_round):      │                    │
   │                       │  route() still Nobody?    │                    │
   │                       ├── POST /mempool/relay ───►│                    │
   │                       │                           │ route() = Primary  │
   │                       │                           ├── POST /submit ───►│
   │                       │                           │                    │ applied into
   │                       │                           │                    │ the open block
   │                       │                           │◄── 200 "applied" ──┤ (Node::tx_status
   │                       │                           │    {tx_hash,       │  = Applied{h})
   │                       │◄── (nothing to relay ──────┤     height}       │
   │                       │     again once B                                │
   │                       │     confirms "applied")                         │
   │                       │                                                 │ block h closes
   │                       │  poll /tx/{hash}, relayed the same way          │ (advance()):
   │                       │  route() = Nobody → ask B → B forwards to       │ tx_status[h]
   │                       │  the primary → "sealed" comes back              │ → Sealed{h}
   │                       │                                                 │
   │  (watch_seal,         │                                                 │
   │   background poll)    │                                                 │
   │◄── "sealed at h" ─────┤ ◄─────────────── relayed answer ────────────────┤
```

Two things make this work without a new gossip protocol:

- **The write and the read use the *same* three-way decision.** A node that
  can forward a write to the primary can forward a read the same way, and a
  node that can't do either just queues/asks a peer instead of refusing. This
  is why `/tx/{hash}` needed no new routing logic — it's `route()` again,
  with a different verb on the end of it.
- **Sealing is learned by asking, not by re-deriving it from a synced
  block.** A block's persisted body is `Effect`s (`seal_body`), not raw
  extrinsics — a node that only ever relayed a write away has no way to
  notice, just by watching its own store catch up, that the hash it once
  held has already landed. `Node::tx_status` (hash → `Pending` / `Applied
  {height}` / `Sealed{height}`) exists only on whichever node actually ran
  `submit()` on it; everyone else's `mempool_round` polls a reachable peer's
  `/tx/{hash}` every tick and copies the answer in once it gets one.

## What this does and doesn't fix

**Fixes:** a write no longer dies the instant it lands on a node with no
direct route to the primary. It gets a chance — as many chances as
`mempool_round`'s tick interval and the 15-minute TTL allow — to reach a node
that *does* have one, via whatever peers this node's own config lists as
reachable. Proven for the general case (two peers, one with a route and one
without, relaying through each other) by `crates/kot/tests/mempool.rs`.

**Doesn't fix:** a node with *zero* reachable peers has nothing to relay
through, TTL or not — the mechanism needs one working link somewhere in the
graph, it doesn't conjure one. That's exactly yuki and shiro's situation
today: every home member is unreachable *to* them, so a write they accept
just sits queued until it expires, unless AWS itself wins an election in the
meantime (then `route()` is `Here` and it applies directly, no relay needed).
Nothing here is a substitute for the router forwards
(`docs/runbooks/deploy-aws-node.md` §1) — it's what makes a write survive
*some* asymmetric gaps in the graph, not this specific one, which needs an
actual route to exist before anything can relay across it.

**Not persisted, not replicated.** Both `Node::mempool` and `Node::tx_status`
are in-memory and node-local. A restart, a demotion, or a term change loses
whatever was still unresolved — the same way an unsealed block itself would
be lost to a rewind if the primary died before anyone pulled it (`CLAUDE.md`,
"Election ≠ replication"). The client's own retry (`Client::try_submit`,
`Cat::submit`) is what re-issues a write that got dropped this way, not
anything the mempool remembers on its own.

## Where to read

- `HANDOFF.md`, "A mempool for the no-route case" — the incident, the fix,
  the two regressions it caused and how each was found and fixed the same
  session (a test-timing assumption, and a REPL raw-mode corruption bug).
- `HANDOFF.md`, "One-way reachability" — the read-side half of this same
  asymmetry: how blocks (not writes) already crossed a one-way link before
  this page's fix existed, via the primary's own push.
- `docs/MESH_AUTH.md` — who a peer *is* on the wire, underneath all of this.
- `docs/TEAHOUSE.md` — the live topology this page's diagram is a stylized
  version of, and current honest limits.
- `docs/runbooks/deploy-aws-node.md` §1 — the actual fix for yuki/shiro
  specifically: router forwards, not a relay.
