# Handoff

State of Akuma Miot as of 2026-09-23 (late: `kot` merge, election, new mesh — `docs/CLEANUP.md`; mesh-internal HTTP now authenticated, then every client-facing read too, then transport itself moved to mTLS pinned to the same keys — `docs/MESH_AUTH.md`; later still: `kot chat`, `RequestCompaction`, a `SendMessage` routing bug found and fixed by actually running two local cats against each other — `docs/LOCAL_SIM.md`). What runs, what doesn't, what to do next,
and the things that will waste your time if you don't know them.

---

## What this is

A litter of LLM agents that coordinate through a blockchain instead of a
socket. Task state, results and final artifacts live on chain; the agents are
ordinary clients. The lineage is `akuma/userspace/meow`, whose own docs called
it *"a hand-rolled Tendermint with futures bolted onto it"* — **none of its code
is imported**, only its behavioural findings, each now a named test.

## Run it

```bash
cargo test --workspace                 # 108 tests, host-native, no docker (docker is gone)

overlays/local/build.sh all            # dist/aarch64/kot (9 MB), dist/x86_64/kot (11 MB), static musl
overlays/deploy/deploy.sh up all       # the mesh: docs/TOPOLOGY_TARGET.md (live agents only)

R="$(grep ^MIOT_ROSTER overlays/deploy/mesh.env | cut -d= -f2-)"
kot --node http://192.168.1.126:9944 --roster "$R" peers            # roster, primary, terms, heads
kot --node http://192.168.1.126:9944 --roster "$R" say --to mac-linux "hi"
kot --node http://192.168.1.126:9944 --roster "$R" task open "your question"
kot --node http://192.168.1.126:9944 --roster "$R"                  # the REPL
kot --node http://192.168.1.126:9944 --roster "$R" log --follow

cargo run -p kot -- run --as solo --seed 1 --db /tmp/solo.db         # a mesh of one: local dev
cargo run -p kot --bin kot -- chat --glm --model glm-5.3              # a model, in-process, no chain at all
```

Signing defaults to the operator's root identity (`~/.akuma/miot/id_ed25519.seed`);
any node will do (`--node` then `--nodes a,b,c`), because a replica forwards
`/submit` and `/account` to whoever is primary.

---

## The crates

| crate | what | tests |
|---|---|---|
| `miot-primitives` | vocabulary: `TaskId`, `Act`, `Effect`, `Limits`, `Timers`. `no_std`. | 5 |
| `miot-tasks` | **the lifecycle, as a pure state machine.** No clock, no I/O. Event-sourced: `TaskTable::apply` is the only place state is written, live or replayed. | 41 |
| `pallet-litter` | thin FRAME wrapper: `ensure_signed` → load → apply → store → emit; `Replaying` (fold without re-ticking) | 17 |
| `miot-runtime` | `construct_runtime!`; `AccountId32`/`MultiSignature`, real `UncheckedExtrinsic` + `Executive`, **executed natively — no wasm** | 2 |
| `miot-store` | block log on ParityDB, compaction-boundary rewind, leader-wins, `aux` (persisted vote) | 17 |
| `miot-mesh` | **leader election** — Raft's, election only, with pre-vote + check-quorum + stickiness. Pure state machine; tests are a simulated network with partitions and kills | 10 |
| `miot-keys` | ed25519 identity: seeds for cats, the operator's SSH *public* key → `AccountId32`, hex wire encoding | 14 |
| `miot-llm` | provider layer on `genai` (15 providers, GLM included) | — |
| `kot` | **the one binary** (`dist/<arch>/kot`), 2026-09-22: `kot run --as <name>` = a mesh node + that cat's agent loop in one process; every other verb is a stateless client of any node. Absorbed `crates/miot` (node + RPC client), which is deleted. `tests/election.rs` = three real nodes over localhost, kill the primary, revive it. `tests/agent_state_machine.rs` (2026-09-24) = the one agent loop against a scripted fake model server | 15 unit + 4 election + 30 agent loop |

**`miot-tasks` is the real thing.** Everything else hosts it. That is why the
pallet is thin and why the same machine runs with or without a chain.

---

## Decisions that took the longest to reach

**FRAME, executed natively. No wasm.** The blob exists so it can be swapped
on-chain for a forkless upgrade; we do not upgrade. FRAME's runtime side is an
ordinary Rust library, so `sc-executor`, the wasm toolchain and the state trie
all fall away. Cost: no runtime upgrades, no `sc-*` tooling. Benefit: a 5.1 MB
static binary that runs on `busybox` with nothing else in the image. The
upgrade path stays open — add `substrate-wasm-builder` and `impl_runtime_apis!`
and the same runtime compiles to a blob.

**Use polkadot-sdk for everything it has.** A hand-rolled signing envelope
(domain string, genesis, nonce, length-prefixing) was written, tested, and
deleted: `UncheckedExtrinsic` + the `frame-system` transaction extensions cover
all of it plus mortality and spec-version binding, and it is now actually
wired — `miot` runs `frame_executive::Executive::initialize_block` /
`apply_extrinsic` / `finalize_block` for real, which is also what makes
`CheckGenesis`/`CheckMortality` mean something (they bind to `BlockHash`
storage, which nothing populated before). What survived from the hand-rolled
version is `account_from_ssh` — reading an OpenSSH **public** key, which
polkadot-sdk does not do — so root is the operator's existing key, *"no new
secret to manage."* Signing as that key from a client is not built: `miot-keys`
has no OpenSSH **private**-key parser, only `Identity::from_seed`. Anything
that isn't the real operator (every cat, and `miot --rpc` for now) signs
with a deterministic seed instead — fine here, since a forged sender still
can't happen without the matching private key, and one operator's own swarm
already trusts every seed it configured.

**A no-fee chain still needs `frame_system::CheckNonce` to see an account,
which needs a balances-shaped hook nothing here provides.** `CheckNonce`
refuses any account whose `providers`/`sufficients` are both zero — a gate
`pallet-balances` normally trips when it credits an account. We have neither
fees nor balances, so nothing was ever going to trip it, and *every*
signature, however correct, was refused with `InvalidTransaction::Payment`
(read: not a fee error, a "this account doesn't exist yet" error). Fixed by
having `miot` call `inc_providers` for every account in `MIOT_MEMBERS`
(default `1,2,3,4,5`, the same seed convention as `MIOT_ROSTER`) at genesis —
named `catnip` in `crates/miot/src/main.rs`, because an account that hasn't had
any can't be nonce-checked. **Worth reconsidering later:** a minimal
`pallet-balances` (zero-fee weight, or just used for its provider bookkeeping)
might be a smaller surface than operator-curated membership lists, especially
once membership needs to grow without a redeploy.

**`DirectiveNag` was unbounded, and a live run found that the hard way.**
Watching a real litter close a task, mimi (leader) mis-fired `TaskUpdate`
(`WrongStatus`/`WrongKind`) a few times on the same directive, and nothing
would ever have stopped the chain re-nagging it forever — no cap existed,
unlike a worker's `MaxNudges`. Fixed: `Timers::max_directive_nudges` (default
3, same count as `MaxNudges`) bounds it, and exhausting the budget now fails
the parent outright (`TaskStatus::Failed`, `Effect::Failed`) rather than
going quiet — there's no "reassign the leader" act the way `ReassignNeeded`
exists for a stuck worker, so silence would mean nobody ever heard about it.
A resolving action (a result landing, a leadership change, a successful
`plan`) resets the budget; only the nag loop itself spends it.

**`clear_all` — a session boundary, not a task-lifecycle act.** Operator-only
extrinsic that fails every currently-open parent at once (reuses
`TaskStatus::Failed`/`Effect::Failed`, same terminal outcome as the
directive-nag exhaustion above — the log doesn't distinguish "leader never
resolved it" from "operator moved on," because downstream nothing needs to).
`/clear` in `miot --chat`; `--clear` on `miot --rpc`. Task ids keep
incrementing past a clear — a "session" is just old parents going quiet, not
a fresh genesis.

**`miot-tasks` is genuinely event-sourced now, not just observably so —
2026-09-22.** Every verb used to mutate `self.tasks`/`self.artifacts`/
`self.leader` directly and return the resulting `Effect`s as a description
of what it had already done. Now every verb *decides* its effects (pure, no
mutation) and calls the new `TaskTable::apply(effect, now)` once per effect
— the **only** place state is written. Live application and replaying a
persisted effect log after a restart go through the identical function, so
they cannot quietly drift apart the way two independently-maintained code
paths eventually do. Forced `Effect::Opened`/`Record`/`Closed` to carry
`text`/`body`/`author` — fields an agent's prompt never needed but `apply`
does, since it can only reconstruct a `Task`/`Artifact` from what the effect
itself carries. Verified by
`replaying_the_effect_log_reproduces_live_state_exactly`
(`crates/miot-tasks/src/tests.rs`): runs a full lifecycle live, replays only
the resulting effects into an untouched table, asserts field-for-field
equality. `docs/PROTOCOL.md`'s tx/state/event section is the write-up; this
is what makes wiring `miot-store` into `miot` (item 2, below) a matter
of persisting+replaying the effect log rather than something to re-derive
from raw transactions.

**Two stores, not one.** Chain write path is ParityDB (`miot-store`, +373 KB).
Agent-local tool output is planned for Turso, and is **per-cat and private** —
nothing reads another cat's store.

**Leader wins, back to the last compaction.** No fork choice, no voting. A cat
that diverges rewinds to the latest compaction at or below the fork point and
replays the leader's blocks. Right for one operator's trusted swarm; badly
wrong for a public chain.

---

**Election, 2026-09-22 (`crates/miot-mesh`).** `MIOT_ROLE`/`MIOT_PEER` are
gone; every `kot run` node is a mesh member and the mesh elects which one
produces blocks. Raft's election only: terms, one persisted vote per term,
majority quorum, randomized timeouts, **pre-vote** (a node cut off never
inflates its term, so it never deposes a healthy leader on return),
**check-quorum** (a leader that can't see a majority steps down by itself),
and **stickiness** (nobody votes while they can still hear a leader). That's
how "unreachable" is told apart from "lost the election". A vote goes only
to a log at least as far along, compared as `(head_term, head)`, so an
ex-leader that kept producing on a minority side, and so has the *higher*
head, can't win with blocks the majority never saw. Heartbeats are
**pulled** (everyone polls everyone's `/mesh/status`), so each node only
needs its own outbound routes, which is what the NAT'd guests allow.
Blocks still move the old way: pull-sync plus *leader wins, back to the
last compaction*. Since 2026-09-25 there is also a leader push, for a peer
that can't call out; see "One-way reachability" below. A node that changes role rebuilds from its store
(demotion) or runs the open block's missing tick (promotion); a replica
that gets a new primary reconciles its whole range against it once. A
replica forwards `/submit` and `/account` to the primary, so a cat's
co-located node is always a valid endpoint.

**Replay double-applied the tick, 2026-09-22 — found wiring election.**
Folding a block (a replica syncing, *or any node replaying its own store on
restart*) ran `on_initialize`'s `tick` locally **and** applied the
producer's recorded tick effects on top. `Directed` *increments*
`directive_nudges_used`, so every replica and every restarted primary burned
the leader's directive budget at double speed. `/tasks` never shows that
counter, which is why the "byte-identical" replica checks missed it. A
promoted replica would have failed parents early. Fixed with
`pallet_litter::Replaying` (tick skipped while folding; `gc` still runs,
since it drops rows without an effect). The test is
`a_folded_block_log_reproduces_the_producers_state_exactly`, whose control
arm shows the old behaviour diverging.

**Artifacts are budgeted in pages, 2026-09-22.** `MaxArtifact` was a flat
64 KiB, about 16k tokens: half a local model's whole 32k window. It is now
`ARTIFACT_PAGES` (4) × `ARTIFACT_PAGE_BYTES` (4 KiB ≈ 1k tokens) = 16 KiB, in
`miot-runtime`, and the leader's `ArtifactNeeded` prompt states the word
budget.

**Mesh-internal HTTP is authenticated and on HTTP/2, 2026-09-23
(`docs/MESH_AUTH.md`).** `/mesh/vote`, `/mesh/status`, `/chain/{head,blocks,
checkpoint}` used to trust whatever a reachable HTTP client claimed — no
relation to `/submit`'s signed extrinsics, which authorize a state change,
not a peer's identity. Every mesh node now carries its own keypair (`kot run
--as <name>` needs `<name>` in the roster or an explicit `--seed`/
`--seed-file`, unconditionally now, not just when an agent loop is
attached), and every mesh-internal request and response carries
`x-miot-signer`/`x-miot-sig` headers, verified against genesis `members` ∪
{root, leader}. Client-facing endpoints (`/tasks`, `/account`, `/submit`,
...) were untouched in this first pass (**closed the same day, see next
entry**). Alongside it: `reqwest`'s client uses
`.http2_prior_knowledge()`, and axum's `http2` feature turns on
`hyper-util`'s connection-preface sniffing so `axum::serve` accepts it —
mesh traffic is a tight poll loop between the same peers, so one multiplexed
connection beats a handshake per call. Verified live (`kot run --as solo`,
curl with and without a valid signature) and via `cargo test --workspace`
(unchanged, 108+ tests including the 3-node election integration test);
**not yet redeployed to the fleet** — host-native only this pass.

**Client-facing reads are authenticated too now, 2026-09-23**
(`docs/MESH_AUTH.md`). Prompted by planning an AWS deploy: `/tasks`,
`/events`, `/artifacts`, `/notes`, `/stats`, `/head`, `/meta`,
`/account/{id}`, `/mesh/peers` served anything to anyone who reached the
port, no auth at all — fine on a LAN, not fine on the open internet. Product
call: this is a private chain with a closed account universe (adding one is
a protocol update, not a runtime registration), so an unsigned request
should get nothing, full stop — same rule mesh-internal traffic already
follows, just not yet applied to reads. Every client-facing GET now runs
through the same `x-miot-signer`/`x-miot-sig` envelope and
`is_trusted_signer` check as `/mesh/status`; `kot`'s own client and agent
loop sign every read they make (they already resolve an `Identity` for
every invocation, so this was free). `/submit` is unchanged — its
authority was always the extrinsic's own signature, not the HTTP envelope.
A replica forwarding `/account/{id}` now re-signs as itself rather than
relaying the caller's headers, since it's a trusted member in its own
right. **This does not add TLS** — transport is still plain `http://`; the
envelope proves who signed a request, not that it's private on the wire
(**closed same day, see next entry**). Verified live (curl with and
without a signature against a `kot run --as solo` node) and
`cargo test --workspace` (unchanged, all passing, `election.rs`'s
replica-forwarded `/account` included); **not deployed anywhere** — this
pass is host-native only, same as the mesh-auth work above.

**Transport is mTLS now too, 2026-09-23, same day.** Every node's TLS
identity is its own `Identity` — a self-signed cert built from the same
ed25519 seed fresh on every start, no CA, pinned by a custom `rustls`
verifier (`crates/kot/src/tls.rs`) against exactly `is_trusted_signer`'s
set, mutual (the server requires a client cert too, since every caller
already resolves an `Identity` regardless). This is a hard cutover — peer
and node URLs are `https://` now, since reqwest only runs the TLS connector
for that scheme — accepted because a fresh genesis is coming anyway (see
"what's next"), so nothing needs the old scheme to survive a rolling
upgrade. `x-miot-signer`/`x-miot-sig` header signing stays (redundant for
auth now, still what binds a signature to one request's exact bytes rather
than "this connection"). Considered gRPC for this instead (the operator's
own suggestion, since the target host's pubkey is already known) — decoupled
on purpose: pinning ed25519 keys via mTLS gets the same guarantee without
the `tonic`/`prost` framework migration gRPC would actually require; see
`docs/MESH_AUTH.md`'s "Rejected alternatives" for the fuller reasoning.
Verified: 4 new `tls.rs` unit tests running a real loopback TLS1.3
handshake (mutual success; either direction's untrusted-peer refusal; a
cert can't claim an account it wasn't built from), `cargo test --workspace`
unchanged otherwise (`election.rs`'s 3-node mesh now running over real
mTLS), and a live smoke test against the real binary (plain HTTP gets
nothing; `openssl s_client` with no client cert gets a TLS1.3
`certificate_required` alert; a real trusted `kot` client works normally).
**Not applied to `overlays/deploy/*`, `docs/TOPOLOGY.md`, or
`docs/runbooks/run-the-mesh.md`** — those still say `http://...:9944` and
need their URLs (and `deploy.sh`/`deploy.py`'s templated env) updated as
part of the redeploy itself, left alone here on purpose rather than
hand-edited mid-conversation given how fragile that templating already is
(`CLAUDE.md`'s own warning). New dependencies: `rustls`, `tokio-rustls`
(both already pulled in transitively by `reqwest`'s `rustls-tls` feature,
now direct), `rcgen`, `x509-parser`.

**Key management written up separately, same day — `docs/KEY_MANAGEMENT.md`.**
Once an account's key started backing mTLS and not just chain writes, "which
key" stopped being a dev-convenience question: the doc covers what one key
now backs, the dev-seed footgun in `kot`'s own CLI defaults (`--root`
defaults to `"1"`, etc. — checked and confirmed the deployed fleet's own
`overlays/deploy/mesh.env` does *not* have this problem, real keys
throughout), and the existing genesis procedure (`deploy.py`/`deploy.sh
ids`) including what changes when a new node joins. Not yet acted on — the
operator is migrating later, alongside the new genesis and the mTLS
cutover above; this is the write-up to work from when that happens.

---

## The teahouse (2026-09-24)

The mesh and its chain are called **the teahouse** (茶馆). It has seven
members on one genesis: meow, tama, kuro, mimi and sora at home, yuki and
shiro on AWS. All links are mTLS, and the primary is elected across the WAN.
`docs/TEAHOUSE.md` has the diagram, the seat-by-seat table, what was shown
live (all seven on one chain, tool calls on bare-metal Akuma and Linux,
results fed back since the agent state machine) and the limits: the three
Akuma members are the fragile ones, and with them down the mesh runs at
exactly quorum (4 of 7).

## The agent state machine (2026-09-24)

`crates/kot/src/agent_state_machine.rs` is **the one agent loop**, hosted by
both a cat (`agent.rs`, `kot run`) and `kot chat` (`chat.rs`). Hosts differ
only in where input comes from (chain `/events` vs stdin), their own extra
tools, and how things are shown; the logic is shared. It is
`docs/MAPPING_REPORT.md` §2.3 actually built, after the first `agent.rs`
shortcut it: that loop's inbox held **only chain events** — every tool call
was awaited, `println!`ed and dropped, so no model ever saw a `Bash`/`Peers`/
`ArtifactRead` result. Live symptom, 2026-09-24: tama and sora answered every
message by calling `Peers`, auto-replying "(ran Peers — no further reply)",
then burning turns on 2-token nothing; yuki said "I don't have an AboutMe
tool" (true: cats were never offered it).

Now: queries (reads, `Bash`, `AboutMe`, ...) are spawned, not awaited; their
results enter the same inbox as wakes and are fed back labelled `[#id Tool]`.
Records (chain writes, a chat reply) are fire-and-forget and never fed back.
A wake assembles a turn at once; results alone wait for their batch (or
10 s); result-only turns are capped at 8 in a row. Both hosts keep a
conversation with the same budget warnings/compaction, reset by the chain
checkpoint moving. Trap already paid for: writing the model's own calls into
its history as text (`[called: Bash{...}]`) made qwen3-4b *type*
`[called: SendMessage{...}]` instead of calling the tool — calls stay out of
assistant turns; each result names its call instead.

**Stalls fixed, same day (meow's kernel build).** Asked to compile the
akuma kernel, meow did five turns of recon, hit the follow-up cap (then 4),
had that result *dropped* (not held, despite the note saying so), was fed
only the first 3000 chars of a 43 KB README, then messaged root "next I'm
checking a plain `cargo build`" and called nothing. Nothing woke it again,
so the build never started and the reports stopped. Also, `Bash` was a fixed
30 s, so a build would have been killed anyway. Now: results past the cap are
queued and fed with the next wake; the cap is 8 and its last turn says
"report now"; a turn that worked on results and only wrote things gets one
**check-in** before the loop idles (cats only, `Host::check_before_idle`;
an empty answer is erased from history); results are fed head and tail;
`Inspect` pages by `offset`; `Bash` takes `timeout` up to 3600 s.
`docs/AGENT_STATE_MACHINE.md` has the diagram and the whole account.
**Not yet deployed to any cat.**

## One-way reachability (2026-09-25)

**The AWS pair had never followed a home primary.** Everything in the mesh
was started by the node that wanted something: a follower GETs
`/mesh/status` and `/chain/*`, a candidate POSTs `/mesh/vote`, and the
primary never calls anyone. yuki and shiro reach home only through
87.71.28.157:9944-9948, whose router forwards were probably never set up
(closed from AWS on all five, including tama's unchanged address). Home
could call *them* the whole time, but a status GET teaches the callee
nothing, and a vote request says "I'm running", not "I won". So whenever a
home node was primary, they followed nobody and sat as pre-candidates. The
AWS journal since Sep 23 has them following only each other. TEAHOUSE's
"primary moves between home and AWS" hid this: they rejoined each time the
primary came back to AWS. On 2026-09-24 it stayed home (terms 9-12), and
they stayed stuck.

Two fixes, both leaving who decides what unchanged:

- **Status goes both ways.** `mesh_round` POSTs `/mesh/status` with its own
  `Status` as the signed body; the handler takes it in (`Mesh::on_inbound`)
  and answers as before. A status heard with no route back is keyed by
  name (`Mesh::heard`), and check-quorum counts distinct names, so a peer
  heard both ways isn't counted twice. A node from before this answers
  POST with 405, and the poller falls back to GET, so a mixed-version mesh
  keeps electing mid-rollout.
- **The leader pushes to a stuck peer.** `Mesh::push_targets` names every
  reachable peer whose head differs from the primary's and hasn't moved
  for an election window, plus, for the rest of that leadership, any peer
  that has needed a push once (or a push-only peer would trail by a window
  every time). For each one the primary runs a session (`push_to`): it
  reconciles *from its side*, reading the peer's `/chain/head` and
  `/chain/blocks` (readable from here) and running the same
  `Store::fork_point`, then POSTs `/chain/push` ops: checkpoint, rewind,
  or a page of blocks. The receiver takes them only from the leader it
  follows, in its current term, signed by that leader's account
  (`Mesh::accepts_push_from`), and applies them with the same store calls
  a pull uses. A pull and a push racing is harmless: each skips rows at or
  below its head.

Deployed everywhere 2026-09-25. Result, live: yuki and shiro went from
pre-candidates at 12945/12944 to followers of kuro at the head in about
80 s, and `kot --node https://kot.akuma.sh:9441 peers` lists every home
cat as "inbound only". Tests: `miot-mesh` (a node that can't call out
follows and keeps up; a healthy mesh never pushes; check-quorum dedupe;
only the stuck peer is targeted; the one-way test fails with the new
paths disabled), and `tests/election.rs`
`a_node_that_cannot_call_out_follows_by_push` over real HTTP, where the
mute node also starts with a fork of its own that the primary has
rewound.

**Was open, now has a fallback (2026-09-25, see below):** a push-only node
couldn't *write* — `/submit` had no route to forward to (`CLAUDE.md`, known
gaps), so AWS cats couldn't post while the primary was at home. The router
forwards (`docs/runbooks/deploy-aws-node.md` §1) are still the real fix and
still not done — what's below is a same-day fallback that doesn't need them.

## A mempool for the no-route case, and a REPL corruption fix (2026-09-25)

Prompted live: root broadcast to the litter from a node whose configured
peers had no route to that moment's primary and got `503 "the primary is
ryzen-linux-amd64, but this node has no route to it (it follows by push);
submit to another node"` (`node.rs::no_primary`) — exactly the gap the
paragraph above names, hit for real instead of in theory.

**The fix:** `/submit`'s `Route::Nobody` case no longer refuses. It queues
the raw extrinsic in a new in-memory `Node::mempool` (bounded, 15-minute
TTL) and answers `200 {"ok":true,"status":"pending","tx_hash":…}`
immediately. A new periodic `mempool_round` (piggybacked on the existing
`poll_ms` mesh tick) then, per queued hash: applies it directly if this
node is now the primary, forwards it if a route now exists, or — the actual
fix — POSTs it to every peer this node's own config *can* reach, at a new
`/mempool/relay` endpoint that runs the identical routing decision. A
relayed write crosses the gap by however many hops it takes to reach a node
with a real route, the same way `Mesh::push_targets` already gets *blocks*
across a one-way link, just for the write side instead of the read side.

**Knowing when it lands:** a block's persisted body is effects, not raw
extrinsics (`seal_body`), so a node can't tell whether a hash it relayed
away landed by re-reading a synced block. Fixed with a second small
in-memory table, `Node::tx_status` (hash → `Pending`/`Applied{height}`/
`Sealed{height}`), set only by whichever node actually calls `submit()` on
it, flipped to `Sealed` when `advance()` closes that block. A new `/tx/
{hash}` endpoint answers from that table if it has one, otherwise forwards
the read the same way `/submit`/`/account` forward a write — and
`mempool_round`'s `Route::Nobody` arm now also *polls* `/tx/{hash}` on its
reachable peers every tick, so a node that only ever relayed a write (never
applied it, never forwarded it to the actual primary) still learns and
reports the truth once a peer that does know it can proxy the answer
through. Neither table is persisted or replicated — a leadership change
loses an unsealed entry, same as the block riding it would be lost.
`Client::try_submit`/`Cat::submit` print the ack and then poll `/tx/{hash}`
in the background (`watch_seal`) until sealed or a ~30s timeout.

Proven with a new test, `crates/kot/tests/mempool.rs`
`a_node_with_no_route_to_the_primary_relays_through_a_peer_it_can_reach`:
three nodes, quorum 2, so alpha/beta settle a primary between themselves
first (observed, not assumed) — then gamma joins with a route to whichever
of the two lost and *no* route to the primary. A submit straight to gamma
queues, relays through the loser, applies on the real primary, converges on
all three via the primary's ordinary push back to gamma, and gamma itself
reports `"sealed"` when asked — proving the read-side relay, not just the
write-side one.

**A real regression this caused, and the fix:** `election.rs`'s
`a_mesh_of_one_produces_on_its_own` broke, because it had been relying on
the *old* single-attempt `/submit` to double as an accidental "block until
this node wins its own election" barrier (success required `Route::Here`
already). The new mempool ack returns immediately, before any election
concludes, which is strictly better behavior but removes that incidental
sync — fixed by polling for `is_producing()` explicitly instead of trusting
a fixed sleep to outlast a 600-1200ms election window.

**A second real regression, found live by root, mid-session:** routing
`try_submit`'s new retry/ack notices through a bare `eprintln!` corrupted
the REPL's display — garbled text mixing the status bar with a stray
"trying the next node" line. `repl()`'s terminal is in raw mode with
ratatui's `Viewport::Inline` owning the whole screen (`docs/CLI.md` §0/§1);
anything written straight to stdout/stderr from then on doesn't get
skipped, it desyncs ratatui's buffer from what's actually on screen. Two of
these (`reconnect`'s "switched to", `get_json`'s "unreachable… trying the
next node") were *pre-existing*, just rare enough before this session's new
retry loop made `get_json`/`meta` run far more often per submit. Fixed with
`Client::notice: Option<mpsc::UnboundedSender<String>>` — `None` (a bare
`eprintln!` is correct) for a one-shot verb, `Some(tx)` once `repl()` has
put the terminal in raw mode, feeding the same inline-viewport scrollback
channel every other REPL line already goes through. `watch_seal`, spawned
and long-outliving the call that started it, takes its own clone of the
sink so it keeps reporting correctly for as long as it keeps polling.

**Also today, small:** `ui::directed` fed a `Directive`'s PascalCase name
(`PlanNeeded`, `ClearanceNeeded`, `ReassignNeeded` — `miot_primitives::
Directive`) straight into `shout()` (an unconditional `.to_uppercase()`
under some themes), collapsing the word boundary into unreadable blobs like
`PLANNEEDED` — found live, root: "we need it to look better, hard to
read." Fixed with `ui::split_words`, inserting a space at each PascalCase
boundary before shouting. `ui::directed_tests`.

**Redeployed the same session**, `overlays/deploy/deploy.py up`, to
dumpster-akuma-amd64, ryzen-linux-amd64, mac-linux-aarch64 and
ryzen-akuma-amd64. **Not** mac-akuma-aarch64: its akuma-guest (nested in
Lima `fc`, a separate VM from `fc` itself) wasn't booted — port 4444
accepted TCP but nothing answered the SSH handshake on the other side.
Left on its old binary; another agent is taking the guest's kernel side.

**Not done, next:** a UI indicator for `Effect::Said`'s `no_ack` flag
(`miot_primitives`) in `ui::said`/`ui::render`'s `"said"` arm — asked live,
root: cats are "still doing crazy loops" despite `no_ack` existing
specifically to stop reply ping-pong (see that field's own doc comment,
2026-09-23), and there's currently no way to see from the REPL which
messages were actually marked `no_ack` to tell whether the flag itself is
the problem or the loop is happening for some other reason. Handed to
another agent rather than built this session.

## Watching the litter (2026-09-25)

What a cat is doing between chain events is visible now, none of it on
chain. Full description: `docs/AGENT_STATE_MACHINE.md`, "Watching it work".

- **Reasoning** (GLM's `reasoning_content`, qwen's `<think>`) is shown on
  stdout and kept in a per-cat JSONL transcript,
  `~/.akuma/kot/<name>.transcript.jsonl` (every prompt, reply, call and full
  result). It used to be thrown away in `miot-llm`.
- **Live activity**: each cat POSTs one record to its own node (phase, what
  woke it, calls in flight, ✓/✗, local tasks, last reasoning); nodes carry it
  on the status exchange, so any node serves every cat's (`GET /activity`).
  REPL: a live row above the hairline, `/activity [cat]`, `/tasks <cat>`;
  CLI: `kot activity`, `kot task list --cat`.
- **The model sees its own calls in flight**: `Running`/`Cancel`, a "still
  running" section every turn, and one notice when a call has been silent
  2 minutes. `Bash` streams its output, and keeps it on timeout or cancel.
- **Local tasks** (`LocalTask`): a cat's own to-do list next to its session
  file, keyed by epoch. It survives a process restart; open ones ride every
  wake's prompt, which is what let meow resume after an upgrade.
- **REPL**: long lines are word-wrapped before they're inserted (they were
  cut off at the terminal edge, which made artifacts unreadable), and
  `/artifact`/`/artifacts <id>`/`/note` render markdown.

Only nodes on this build report activity; as of this writing meow, yuki and
shiro do.

## Outages of 2026-09-25: ryzen, sora, the AWS pair

**ryzen ran out of memory.** Two llama-servers (one per ryzen cat, ~5 GB
each at `-c 8192`) on a 13.7 GB laptop that also runs a desktop and Steam.
Swap is zram — RAM too — so instead of dying it thrashed for 20 minutes
(sshd accepted TCP and never sent a banner; journald's watchdog fired). The
OOM killer was tripped by tama's kot asking for the last page and picked a
4 MB `steamwebhelper`. Fixed: **one shared llama-server**, `--parallel 2 -c
16384` (8192 per cat, unchanged; `/v1/models` still says 8192), capped at
`MemoryMax=7G` with no swap, so a runaway server is killed and restarted
instead of wedging the box. 4.3 GB for both cats where two servers took ~10.
sora still dials `192.168.1.49:8082`: a `systemd-socket-proxyd` socket there
forwards to the shared server. `deploy.py llama` generates all of it
(`LLAMAS`, `LLAMA_PROXIES`).

**sora never came back after the reboot**, because its network was typed by
hand and died with it. It's two systemd units now
(`overlays/deploy/hosts/ryzen/`): `sora-net.service` (tap0 `192.168.1.49/32`,
host route to `.50`, proxy-ARP, the existing `akuma-dnsmasq` container) and
`sora.service` (Firecracker on ryzen's own `akuma-vm.json`/`disk.img`, root —
the laptop user isn't in `kvm` — console in `boot.log`). Not yet proven
across a real ryzen reboot. The piece that was actually missing: **Docker
sets `FORWARD` to `DROP`**, so nothing off the box reached the guest (6,985
packets dropped by the time it was found); tama, on the same host, forwards
nothing and saw sora fine, which hid it. `sora-net` adds two accepts,
Wi-Fi ↔ tap0, in `DOCKER-USER`.

**yuki and shiro went deaf.** `TlsListener::accept` ran each mTLS handshake
inline, one at a time, with no timeout. One client that connected and never
sent a ClientHello (a scanner, through nginx's stream proxy) blocked every
connection after it for good: `Recv-Q 129` on :9944, the node silent to the
mesh and the operator while its own outbound polling and agent loop carried
on. Handshakes now run in their own tasks, each with a 10 s timeout
(`tls::HANDSHAKE_TIMEOUT`); tests `silent_connections_do_not_block_a_real_client`
(200 silent clients) and `dropping_the_listener_frees_the_port`. Every node had
it; the AWS pair are the ones facing the internet. **Also, separately: their
OpenRouter account is out of credit** — every turn is `402 Payment Required`.
They follow the chain and are reachable, but can't think until it's topped up
or they're moved to GLM.

**meow's kot died silently mid-`Bash`** and herd restarted it; the `apk add`
it was running finished and stayed a zombie (`PPid: 23`, the dead kot's
worker thread — nobody reparents it, nobody reaps it). This is the case
`../akuma/docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md` documents as fixed
(`9316a6b2`), and meow's kernel (`c9586004`) contains that fix — so its fix
doesn't cover however kot died this time. Why it died left no trace: the
generated `start.sh` didn't send stderr to herd's log, so a panic or a failed
allocation had nowhere to go. It does now (`exec … 2>&1`, template and meow).

## Model swaps: tama on GLM, kuro on Gemma (2026-09-25)

Two cats changed model the same evening; `docs/FLEET.md` "As running" has the
full table (and mimi, still `qwen3:4b` on llama-server :8084).

- **tama → `glm-5.3`** (z.ai, same as meow), so a cat can try building and
  extending `kot` on its own host. ryzen got rustup (root, stable 1.98.1) and
  a plain `git clone` at `/root/src/akuma-miot`. The first build was launched
  by hand, not by tama: `systemd-run --unit kot-build -p MemoryMax=6G -p
  MemorySwapMax=0 -p Nice=10`, `jobs = 4`, log in `/root/src/build.log`.
  Capped because ryzen OOMed earlier the same day (above). A native glibc
  build, not the shipped static musl one. The shared llama-server now serves
  only sora.
- **kuro → `gemma4-yolo-4b` on Ollama**, `MIOT_LLM=http://192.168.5.2:11434`
  (`Llm::local` appends `/v1/`; Ollama's OpenAI-compatible endpoint answered
  a Bash tool call correctly from inside `fc` before the switch). The first
  Ollama cat in the fleet, on purpose: llama-server refuses Ollama's Gemma 4
  blob (`wrong number of tensors; expected 2131, got 720`). Ollama must be
  started by `../yolo/run-ollama.sh` — the bare `ollama serve` that was
  running had no `OLLAMA_KEEP_ALIVE`, so the 14 GB model would unload after
  5 idle minutes and reload on the next wake. Nothing brings it back after a
  mac reboot.

## Compaction: what a window would cost (measured 2026-09-25)

A node keeps the current state and the last 4,096 events in memory
(`LOG_CAP`); blocks live on disk. But **every start replays every block since
the last checkpoint**, and checkpoints happen only on `/clear` or
`kot compact`. With the checkpoint at 1590, today's restarts each replayed
~15,600 blocks: yuki (t4g.nano) 195.6 s, tama 39.4 s, meow ≤51 s — minutes of
a node being unreachable after every start. It grows even when idle: a block
seals every 6 s, empty or not, 14,400 a day.

Measured on the live chain: the state snapshot a compaction writes is
**9,094 bytes**; blocks average **41 bytes** (most are ~16-byte empty seals).
A 4-hour window is at most 2,400 blocks (~100 KB), so a compaction is one
~9 KB write plus deleting ≤2,400 rows, six times a day, and the worst-case
replay drops to ~30 s on yuki and ~6-8 s elsewhere. A lagging peer adopts
the 9 KB snapshot instead of pulling blocks.

**The real cost is that a checkpoint is also the session boundary today.**
When `last_checkpoint` moves, every cat resets its conversation, clears its
`question` and empties its local tasks (`agent.rs` `watch_chain`); a
restarted node's `/events` (the REPL's replay) starts at the checkpoint too.
Seen live: a manual compaction emptied meow's task list, and the next "please
proceed with your task" reached a cat with nothing to proceed from. So
automatic compaction needs a session id of its own first: `/clear` starts a
session, compaction only prunes and snapshots, and cats key on the session.
Not built yet (Next, item 0a).

## Block seal times (2026-09-24)

Every block body now ends with the primary's wall clock at seal time
(unix ms, SCALE `u64`), appended after the effects (`node.rs`
`seal_body`/`open_body`). `/events` entries carry it as `at`, and every
client shows it in **UTC** (`09-24 01:40:12Z`). Before this there was no
time on chain at all and clients *estimated* one backwards from "now" by
block distance, which was wrong whenever block production paused.
Backward-compatible by construction: older nodes read a body with
`Decode::decode` (not `decode_all`), so they get the effects and ignore
the tail; replicas store the primary's bytes verbatim, so the fork check's
byte compare is unaffected. Caveat: a block gets a time only if the primary
that sealed it runs this build — blocks from an older primary (yuki on AWS,
as of this writing) show no time, deliberately, rather than a guess.

## What is real and what is not

**Real:** the pallet and its state machine; the FRAME runtime executing
natively, through a real `Executive` block lifecycle; `ensure_signed` deciding
authority from a signature that is now actually verified — `/submit` decodes
and checks a real `UncheckedExtrinsic` (signature, nonce, mortality, genesis
and spec/tx version) before anything dispatches; artifacts stored and read
back from chain state; five containers on a network with the chain ticking in
its own process; four llama-servers; **ParityDB, wired in and proven,
2026-09-22** — `miot` persists every block's effects and replays them
on start (`MIOT_DB`, defaults to `miot.db`; the docker `node` service
mounts a named volume at `/data`), verified both standalone (kill/restart a
bare `miot`, `/events` and `/tasks` came back byte-identical) and
through `docker compose restart node` / `up -d --force-recreate node` — a
task opened before either survived it. Practical upshot: **cats no longer
need restarting when the node does** — `seq` numbering is continuous across
a restart now (the log is real, not reset to empty), so a cat's already-held
cursor stays valid instead of pointing past a wiped log. See
`docs/runbooks/run-local-swarm.md`, updated accordingly.
**a cat running somewhere that isn't a container**
— `kuro` moved from its docker container to the Lima VM (`fc`, aarch64 Linux)
as of 2026-09-22: cross-compiled with the same `aarch64-unknown-linux-musl`
toolchain `build-akuma.sh` already used, copied onto the VM's own disk with
`limactl copy`, and it picked up an in-flight sub-task the moment it
connected. The node never noticed it moved — which is the actual point of
`docs/CLI.md` §5a and `overlays/local/README.md` Stage 1, now demonstrated
rather than asserted.
**a second node, replicated for real, 2026-09-22** — the replica role moved
off docker the same day it was born there: `node2` now runs as a bare
`miot` on **ryzen** (192.168.1.126, systemd `miot-node2.service`,
`MIOT_ROLE=replica MIOT_PEER=http://<mac>:9944`), cross-compiled with the
x86_64 twin of `build-akuma.sh`'s aarch64 toolchain. Verified converged
(`/tasks`/`/events` byte-identical; a live signed `--say` replicated within
one sync interval). The docker `node2` container was stopped, removed and
deleted from the compose file — one replica identity, not two running under
it. `docs/TOPOLOGY.md` has the details.
**a fifth node, durably up on the real akuma hardware, 2026-09-22.**
`node5` (`ssh akuma`, the physical Akuma-kernel box) runs a *fresh-genesis
primary* of its own as a **herd service**, survives `kill` and full reboot
(replaying 1000+ persisted blocks each start), and takes signed extrinsics
from the mac. The same day it started, it also forced three **kernel** fixes
in ../akuma — none of them akuma-miot bugs: writable `MAP_SHARED` file
mappings went from refused-by-design (ParityDB's `MmapMut` died at every
reopen, `os error 38`) to demand-paged with whole-region write-back;
ext2 `truncate` stopped answering `Ok(())` for extend (a silent no-op that
made `set_len`-before-write a zero-byte file); and `posix_fadvise` got its
table row (parity-db `try_io!`s it). `storeprobe` completes **all 7 stages
on real hardware** — the diagnostic this project shipped and never ran,
now green where it was built to run. Full story and the honest residual
(replica-catch-up not re-tested post-fix): `docs/TOPOLOGY.md`'s `node5`
section.
**real compaction, 2026-09-22** — `Store::compact` fires on root's `/clear`
(a genuine snapshot of the whole storage trie via `sp_io::TestExternalities`'s
own `into_raw_snapshot`/`from_raw_snapshot`, not a hand-rolled subset); a
reconciliation now lands on that checkpoint instead of genesis. See item 5's
Part 1 writeup.
**a fourth node, on the actual Akuma kernel, 2026-09-22** — item 3, below,
used to be "run `storeprobe` on Akuma"; the bigger claim turned out true
first. `docs/TOPOLOGY.md` has the diagram and the honest caveat.
**`kot chat` and standalone compaction, 2026-09-23** — `kot chat` talks to a
model in-process with real tool execution (`Bash`/`ReadFile`/`WriteFile`/
`SendMessage`) and no node at all; `RequestCompaction` (a new root-only
`pallet_litter` call) lets `kot compact`, or any cat's tool call, trigger
`Store::compact` on demand instead of only as `/clear`'s side effect. Both
verified live. `docs/LOCAL_SIM.md`.
**`deploy.py`, a Python rewrite of `deploy.sh`, 2026-09-23** — built on the
`../akuma/scripts/box/` pattern (checked-in templates, an env file every
wrapper sources, argv-list subprocess calls instead of hand-quoted strings
a second shell re-interprets — the specific fix for the akuma/fcguest
shape `CLAUDE.md` calls out as least reliable). `env <agent>` verified
byte-identical to `deploy.sh env <agent>` for all five agents before
anything shipped, `--dry-run` traced a full `up all` with no host touched,
then `mac-linux-aarch64` (Lima `fc`) was redeployed for real and confirmed
live on the rebuilt binary. `ryzen-linux-amd64`, `ryzen-akuma-amd64`,
`mac-akuma-aarch64` still need `up`'d — not reachable this session (off
the home LAN). `dumpster-akuma-amd64` (bare metal) deliberately untouched.
`deploy.sh` itself still works, unmodified.
**two cats debated something for real, 2026-09-23** — two local Qwen3-4B
`llama-server`s, each its own `kot run` node, peered into a real two-node
mesh (election, primary/replica, the works — same code path as the fleet).
Given the Akuma OS description and told to discuss it, they held a genuine
multi-round back-and-forth and one published a joint `Artifact`. Doing this
found and fixed two real bugs in `agent.rs`: `SendMessage`'s `to` argument
was silently discarded (a cat could never address another cat by name, only
the operator's own `kot say --to` ever worked), and a `"said"` turn that
didn't call `SendMessage` sent nothing back at all, ever. Also surfaced,
not yet fixed: a non-root broadcast (`to: None`) doesn't wake anyone's
agent loop despite `Effect::wakes()` saying it should (`Effect::to()`
collapses to the same `None`), and a failed `/submit` is never retried by
the agent loop — full writeup, repro commands and what's still open in
`docs/LOCAL_SIM.md`.
**a second debate, same day, published `docs/TOPOLOGY.md` itself as
artifact 1** (new `kot publish <file>` verb — there was no operator-side
way to publish a standalone artifact before) and asked the litter to
propose tooling for itself. Found and fixed a third bug doing it: a turn
whose tool calls included more than one on-chain submission (`Artifact` +
`SendMessage` together) could race — both read the same nonce over HTTP
before either applied, one came back `Invalid(Stale)`. `Cat` now tracks
its own nonce locally (it's the only signer for its account, so it was
always the real authority on it) instead of asking the node before every
submit, fixed under one lock so concurrent calls in a turn get distinct
sequential values instead of racing a read; `meta` is now fetched once,
ever, and cached too, since nothing on this chain can change it. Verified:
the exact repro landed both calls in the same block afterward. Also found,
not fixed: a cat can claim it published something it never actually
called the tool for (`tama` said "Joint report published" via
`SendMessage` with no matching `Artifact` call; `kuro` did it for real
when asked). `docs/LOCAL_SIM.md` has the full writeup.

**Not yet real:**

- ~~**No election, no automatic failover.**~~ **Built, 2026-09-22** — see
  "Election" under Decisions. Honest residuals: membership is static
  (genesis + `MIOT_PEERS`), and a block no replica pulled before its
  primary died is lost to the rewind (no commit index).
- **Akuma-on-the-real-hardware: tested, durable, still one boot.** `node5`
  (above) runs on the physical `akuma` box as a herd service and survives
  restart over a grown ParityDB. `node4` on the Firecracker guest remains
  intermittent for a different reason (index-growth panic). `storeprobe`
  and `mmapprobe` (`crates/miot-store/src/bin/mmapprobe.rs`) are the two
  diagnostics; both have run on the real host.
- **No OpenSSH private-key signing.** `miot-keys` reads the operator's
  *public* key (`account_from_ssh`) but cannot sign with the matching private
  one — nothing here has parsed an OpenSSH private key file. Signing as the
  real operator today means using a seed whose derived account you separately
  told the node is root (`MIOT_ROOT`/`MIOT_ROOT_PUBKEY`), not the operator's
  actual `~/.ssh` identity.
- **No real membership growth path.** `MIOT_MEMBERS` (see `catnip` above) is a
  fixed operator-curated list at genesis. Adding a cat mid-run needs either a
  new extrinsic that provisions an account, or the balances pallet noted above.

---

## Traps that already cost time

- **`gemma3:4b` cannot be a cat.** Ollama refuses: `400 "does not support
  tools"`. Tool support gates model choice.
- **`llama-server` needs `--jinja`** or it silently never emits `tool_calls`.
- **Docker on macOS has no GPU passthrough.** Containerised llama-servers are
  CPU-only and 5–10× slower. Keep models on the host.
- **Pin both Dockerfile stages to the same distro.** A newer builder links a
  glibc the runtime image lacks, and it fails at *exec* time:
  `GLIBC_2.39 not found`.
- **Four llama-servers on one GPU do not parallelise.** One alone answers in
  4.1 s; four concurrently take 16.5 s — exactly 4×. They serialize. `-t 1` is
  enough (measured: identical to `-t 3`).
- **The SSH exec channel to an Akuma guest stalls at exactly 1,048,576 bytes**
  and `dist/miot` is 5.1 MB. Use HTTP via `10.0.2.2`. scp does not work at all
  (no SFTP subsystem).
- **Timers are ~50× too conservative.** `claim_window` 100 blocks was sized for
  a 120–200 s turn; turns here are 3–60 s. Nothing is broken, but a live litter
  wants single-digit windows.
- **`DirectiveNag` had no cap, and a live run hit it for real** (mimi mis-fired
  `TaskUpdate` a few times on the same directive). Fixed —
  `Timers::max_directive_nudges` — see "Decisions" above. Watch for this again
  anywhere else a nag/retry loop is added without an explicit budget.
- **`limactl shell fc -- ls ~/foo` resolves `~` on the host shell, not the
  guest**, because tilde expansion happens client-side before `limactl` ever
  sees the argument — so it silently becomes the *host's* home directory path,
  which then gets treated as a guest path and (usually) fails to resolve.
  Use an absolute guest path, or `limactl shell fc -- bash -lc 'echo $HOME'`
  to get the real one (`/home/<user>.guest`, also aliased at
  `/home/<user>.linux` — same inode, either name works once you're inside).
  `limactl copy` itself has no such problem; it takes real guest paths.
- **A `Failed` parent doesn't normally disappear immediately — `TaskTable::gc`
  (wired into `pallet-litter`'s `on_initialize`) only drops rows closed at
  least `GcKeepFor` blocks ago (14,400, i.e. 24h at `BLOCK_MS=6000`).** Not a
  bug on its own — `gc`'s doc comment calls this "the only kind of compaction
  that is consensus business," deliberately not instant — but 24h is tuned
  for a real chain's audit trail, not this dev loop, and it reads as a bug
  the first time a task lingers in `/tasks` after you thought you were done
  with it. **`/clear` (`clear_all`) is the one exception, fixed 2026-09-22**:
  it now calls `gc(now, keep_for: 0)` itself right after failing every open
  parent, sweeping *every* already-closed/failed parent immediately, not just
  the ones it just failed — "/clear is a session boundary, nobody needs the
  old records lying around." `GcKeepFor` itself is unchanged for the natural
  path (`DirectiveNag` exhaustion) — a parent that failed on its own still
  sits for 24h unless an operator `/clear`s it away. Revisit `GcKeepFor`
  itself with the same "measured against LLM turns" treatment the timers
  above got, if that natural-failure lingering ever gets in the way too.
- **A `WrongStatus` refusal on `TaskPlan`/`TaskUpdate` can be a race, not a
  bug** — observed live, 2026-09-22: a parent got a `PlanNeeded` directive,
  mimi picked it up and spent 45s planning it, and by the time `TaskPlan`
  landed the same parent had already been `/clear`'d (in that case, by a
  concurrent operator test against the same node) — so the pallet correctly
  refused a plan against a task that was no longer `Open`. The refusal is the
  system working as designed (`docs/MAPPING_REPORT.md` §1.1: "applied-vs-
  refused must be typed, never sniffed"), not silent corruption. Before
  chasing this as a state-machine bug, check whether the task's status
  changed between the directive being issued and the reply landing — `curl
  .../tasks` or `.../events` will show it.

- **Akuma's writable-`MAP_SHARED` gap: found, fixed upstream, same day.**
  The 2026-09-22 session that first touched the physical box found ParityDB
  un-restartable there (`os error 38` at reopen; a raw mmap probe of the
  shape **segfaulted**). The fix landed in ../akuma, not here — writable
  `MAP_SHARED` file mappings are now demand-paged with write-back, ext2
  `truncate` really extends, and `posix_fadvise` exists
  (`docs/reference/subsystems/amd64-shared-write-mmap.md` in ../akuma).
  Residuals to keep honest: replica-catch-up sync from this box has not
  been re-tested on the fixed kernel (it wedged mid-sync pre-fix), and
  node4's Firecracker-guest index panic is a *different*, still-open
  failure. Both probes (`storeprobe`, `mmapprobe`) run on the metal now.
- **Herd on the trashcan wedges failed services, not configs.** A service
  whose spawn failed a few times under the box's previous herd stayed dead
  after its conf was repaired — a renamed service started on the first
  reload where the fixed one never did; a reboot cleared it. Also: the
  box's previous `/bin/herd` predated config reload entirely (never picked
  up services added after boot) — the laptop-built herd is installed now.
- **Restarting a process on akuma leaves zombies** (nothing reaps) — `ps`
  fills with dead entries; don't read them as live.
- **A stale `python3 -m http.server` on the mac squatted port 8123** and
  served 404s that looked like a wrong URL. `lsof -i :PORT -sTCP:LISTEN`
  + `ps -p <pid> -o command=` before blaming the client side.

- **The akuma box can stop spawning after a `kill`** (2026-09-22): right
  after `deploy.sh retire-old` killed the old `node5` process, sshd still
  authenticated but every exec returned status 241 with no output, for
  minutes. Not recovered remotely; needs a power cycle. Suspect the amd64
  no-slot-recycler class (`../akuma/docs/archive/AKUMA_AMD64_NO_SLOT_RECYCLER.md`);
  not root-caused. Batch ssh execs to that box, and prefer letting herd
  restart a service over killing it by hand.
- **`kot` as a *replica* on the akuma metal box wedges within minutes**
  (2026-09-22, reproduced across a reboot): listener refuses even from
  `127.0.0.1`, sync stops, threads all `R` and still accruing CPU. The old
  `node5` was durable only as a *primary*, with no outbound HTTP; a replica
  is an HTTP client and server at once, which is the one combination that
  has never stayed up on that kernel. Unconfirmed hypothesis, not
  root-caused. `../akuma` territory. See `docs/TOPOLOGY_TARGET.md`.
  **Did not reproduce, 2026-09-23**: `dumpster-akuma-amd64` (meow) was
  redeployed and brought back up as a replica — joined the mesh (term
  25, synced head), stayed reachable, and its agent loop ran multiple real
  turns (`Bash` → `uname -a`, `SendMessage`, both together in one turn) over
  several minutes with no wedge. One clean run doesn't root-cause or fix the
  intermittent kernel-level issue above — it's still not explained — so
  don't read this as "closed," just as evidence it isn't 100% reproducible
  every boot. Re-added to `deploy.sh`'s `LIVE` array on the strength of this
  run; watch for a recurrence before trusting it unattended.
- **ParityDB on aarch64 Akuma (`akuma-guest`): fixed 2026-09-22, in
  `../akuma`.** Two aarch64-only kernel bugs, found by running `kot` there:
  (a) no `fadvise64` arm in the aarch64 dispatcher, so reopen failed with
  ENOSYS; (b) the amd64 fix's change to the *shared* `mmap::plan` sent
  writable `MAP_SHARED` down aarch64's lazy path, which never registered it
  for write-back, so a clean close lost every write (storeprobe reopened
  empty). Now `storeprobe` passes 7/7 on the guest, and `kot` survives
  `kill -9` with its whole store (753 blocks replayed, no peers to sync
  from). Full write-up: `../akuma/docs/archive/MIOT_MESH_ON_AKUMA.md`, which
  also covers the metal-box replica wedge (open) and PSTATS naming amd64
  syscalls from the aarch64 table (open). **Lesson:** a change to shared
  kernel code must be checked against both dispatchers.
- **amd64 Akuma: kot goes deaf in minutes, on metal *and* in a 1-vCPU
  Firecracker guest on ryzen (`192.168.1.50`, the `ryzen-fc` agent).** Not
  fixed. Handed off in `../akuma/docs/archive/AKUMA_AMD64_KOT_REPLICA_WEDGE.md`.
  Strongest lead: x86_64 `accept4` (288) has no amd64 syscall row, so tokio's
  accepts get ENOSYS. Until then akuma-metal and ryzen-fc can't hold a seat,
  and the mesh is ryzen-linux + mac-linux + mac-fc.
- **Akuma's sshd merges stderr into stdout.** Anything parsed from `ssh
  akuma '…'` output needs `2>/dev/null` on the far side.
- **The z.ai token is a coding-plan key**: `paas/v4` answers "insufficient
  balance"; only `coding/paas/v4` works. `Llm::glm` routes bare model names
  to `zai-coding::`.
- **Lima forwards guest ports to the mac's loopback only by default.** The LAN
  couldn't reach a node in `fc`. `../akuma/overlays/devbox-firecracker/host-setup.sh`
  now creates `fc` with 9944-9949 on `0.0.0.0` (`LIMA_LAN_PORTS`), and refuses
  an old instance without it.
- **`pkill -f <pattern>` over ssh can match the ssh session's own `bash -c`
  line and kill it** (exit 255). Kill by PID.
- **A litter leader's WrongKind/SubtasksOutstanding/no-tool-call refusals can
  be a prompt bug, not model flakiness — found live, 2026-09-23.** meow
  (GLM, `dumpster-akuma-amd64`) was refused 7× `WrongKind`, 3×
  `SubtasksOutstanding`, plus 3 wasted turns with no tool call at all, all
  under the `ClearanceNeeded` directive. Root cause: `agent.rs`'s
  `ClearanceNeeded` prompt told the leader "for EACH sub-task call
  TaskUpdate" without ever naming a sub-task — no id, no result text,
  nothing but the parent id in the header, which is exactly the id
  `clear`/`reopen` refuse (`Error::WrongKind`). The model had nothing to act
  on but guess. Fixed by having the prompt fetch `/tasks`, list every
  sub-task actually `AwaitingClearance` under this parent with its id,
  holder and result, and telling the model to use *that* id, never the
  parent's — `/tasks` itself was missing the sub-task's `outcome` entirely,
  also fixed. Also added an explicit id-shape/always-call-a-tool rule to
  every persona (`AGENT_RULES` in `agent.rs`) and disambiguated
  `TaskUpdate`'s `task` field in the tool schema (`miot-llm`). Redeployed
  fleet-wide the same session; watch whether `WrongKind` on `ClearanceNeeded`
  actually drops to zero over the next few live runs.
- **`deploy.sh`'s fcguest identity check compares against a `mesh.env` key
  that hasn't existed since 2026-09-22 — found live, 2026-09-23.** `mesh.env`
  was relabeled that day to persona names (`mimi=pub:...`, `sora=pub:...`),
  but `cmd_up`'s check still grepped for `$a=pub:...` (the deploy-script
  agent id, e.g. `mac-akuma-aarch64=pub:...`), which never matched anything
  — every fcguest identity check was silently comparing against an empty
  string. Fixed (`field "$a" 5` → the persona name). **Caused a real
  incident chasing it live**: running `kot --seed-file
  ~/.akuma/kot/mac-akuma-aarch64.seed id` to see what was staged there
  auto-generated a fresh random identity at that path (`kot id` calls
  `load_or_create_identity`, which creates on a missing file — a nonobvious
  side effect for what looked like a read-only check), and a subsequent
  `deploy.sh up` hit a transient ssh hiccup on the `test -s
  id_ed25519.seed` guard that made it copy that bogus seed onto the guest,
  overwriting mimi's real identity. Recovered because the real seed was
  never actually lost — just staged under the *pre-rename* name
  (`~/.akuma/kot/mac-fc.seed`, `ryzen-fc.seed`), confirmed by matching its
  derived pubkey against `mesh.env`. Both now also staged under their
  current agent ids so `deploy.sh`'s `$HOME/.akuma/kot/$a.seed` path
  actually resolves. **Lesson: never run `kot id` against a seed-file path
  to "check" it — it writes.**
- **Superseded 2026-09-25: sora (`ryzen-akuma-amd64`) runs, under systemd —
  "Outages of 2026-09-25" above.** Kept for the history:
  **`ryzen-akuma-amd64` is dead right now, pre-existing — confirmed live,
  2026-09-23, not caused by anything this session touched.** `firecracker`
  is up on ryzen and the guest boots, but `kot` crash-loops inside it:
  `/home/netoneko/akuma/boot.log` on ryzen shows `[herd] Service kot exited
  with code  241` three times, herd restarting it each time, then giving up
  silently (no further "Started kot" after the third). This is the same
  class already tracked in `../akuma/docs/archive/
  AKUMA_AMD64_KOT_REPLICA_WEDGE.md` ("amd64 Akuma: kot goes deaf in
  minutes"), not a new bug. `deploy.sh`'s `LIVE` array still lists this
  agent; in practice the mesh runs fine at 4/5 (quorum is 3) without it.
  `../akuma` territory — see that handoff before spending more time here.
- **Docker's `FORWARD DROP` hides behind "it works from the host"
  (2026-09-25).** Anything forwarded through ryzen to a guest — sora on
  tap0 — is dropped once Docker has started, while the host itself reaches
  the guest fine. Check `iptables -L FORWARD -v` (packet counts on the
  policy) before theorizing about proxy-ARP. `sora-net` owns the accepts now.
- **A `watch` receiver made after the first send never sees it
  (2026-09-25).** `post_activity` subscribed inside its own task; a GLM cat's
  loop published before that task ran (no `/v1/models` round trip to yield
  on), so it never showed up at all. Subscribe before spawning.
- **Don't trust `pyte` for ratatui output (2026-09-25).** It has no handler
  for `CSI n S` (scroll up), which `scrolling-regions` inserts use, so every
  insert renders as garbage over the old screen. To check what the REPL
  draws, drive it in a pty (answer its `CSI 6n` cursor query) and read the
  raw bytes.
- **zram is RAM (2026-09-25).** On ryzen, "swap" is compressed memory, so
  swapping doesn't relieve pressure; the box thrashes instead of an OOM kill
  landing. Give memory-hungry services `MemoryMax` and `MemorySwapMax=0`.

---

## Open theory: Akuma's "disk issues" / spawn failures (2026-09-24)

**The operator's hypothesis, not yet tested.** The failures on the Akuma
members may be one problem: kernel heap fragmentation or cache exhaustion
building up while kot runs, possibly caused by something in how ParityDB
uses the disk (mmap, file growth, compaction) that we missed. The failures
in question are `failed to spawn '/bin/sh'`, exec returning `241`, herd not
starting services, and the "disk" symptoms. The theory doesn't explain why
the Firecracker guests fare better than the metal box. That could be
platform differences: real disk vs. a virtio block device, different memory
size, different caching.

What's already known and bears on it:

- **A competing, measured explanation for part of it:** amd64 sshd leaks
  one pipe per session, and `MAX_PIPES` is 64 machine-wide, so spawns fail
  from about the 44th session (`../akuma/docs/README.md`, measured at
  `live=64`). If that were the whole story, failures would track ssh session
  count only.
- **For the theory, roughly (not counted exactly):** on 2026-09-24 the
  freshly rebooted dumpster, with kot running, failed at around the 12th–15th
  ssh session (`exit 241` on a plain `mkdir`). That's well short of the ~44
  the pipe leak predicts, so something else is using the budget, or a
  different resource ran out.
- **ParityDB and Akuma have history:** the amd64 shared-write mmap fix
  (`../akuma/docs/reference/subsystems/amd64-shared-write-mmap.md`, see
  "What is real" above) was found through the block store.
  `storeprobe`/`mmapprobe` (built by `overlays/local/build.sh`) exist to probe
  exactly this.
- **Against "Firecracker is fine":** mimi (Akuma in Firecracker, arm64) also
  had herd list kot as enabled and never start it, and sora's guest went
  unresponsive. Weaker symptoms, but possibly the same class.

Cheapest tests to separate the two:

1. **Session count to failure, with kot stopped vs running,** after fresh
   boots (the one ssh exec that disables herd's kot counts). About 44 both
   ways means the pipe leak; much fewer with kot running means kot or
   ParityDB is taking something.
2. **At the moment of failure, capture whatever the kernel exposes about
   heap, page cache and pipe/slot counts** (serial console, `/proc`), and
   compare with a fresh boot. `../akuma/docs/runbooks/` covers what is
   readable on the metal.
3. **`storeprobe`/`mmapprobe` soak,** alone, on the metal and in a
   Firecracker guest, then check whether spawning still works afterwards.
4. **Point `MIOT_DB` somewhere else, or quiet it** (no compaction, a
   smaller log), and see whether the failure point moves.

## Next, in order

0a. **A session id apart from the checkpoint, then automatic compaction**
   (2026-09-25; measured above, "Compaction: what a window would cost").
   `/clear` bumps a session number in chain state, served on `/head`;
   `watch_chain` resets a cat's conversation and local tasks on *that*, not
   on `last_checkpoint`; the primary compacts on its own every N blocks (4 h
   is 2,400). Decide what `/events` should keep across a compaction the
   session spans — today a restarted node's replay starts at the checkpoint.

0. **Future work on `kot`: the operator's roster name becomes `ken`, not
   `root`** (asked 2026-09-22; not started). It must stay configurable
   (e.g. `--operator-name` / `MIOT_OPERATOR_NAME`), and `ken` is the default
   for what it means in Japanese:
   - 賢 (ken): wise, clever (also common in names)
   - 権 (ken): authority, right (as in 人権 *jinken*, "human rights")

   This renames the *roster label only*. The authority itself stays what the
   pallet calls root (`MIOT_ROOT_PUBKEY`, `Authority::Root`, `set_root`,
   `clear_all`'s check), and the on-chain `from_root` flag on `said` stays
   as is. Nothing on chain carries a name. The places that spell it today:
   - `crates/kot/src/main.rs`: `DEV_ROSTER` (`root=1,…`), and the
     `miot-root` comment on the identity's `.pub` line.
   - `crates/kot/src/agent.rs`: the planner's worker list filters out
     `n != "root"`, so the operator is never handed a sub-task
     (`RootNotAssignable`). This filter must follow the configured name,
     not a literal.
   - `overlays/deploy/deploy.sh` `ids`: writes `root=pub:<acct>` into
     `mesh.env`'s `MIOT_ROSTER`. Regenerating `mesh.env` changes no key
     and no genesis. The roster is client-side lookup only, so this is
     just a redeploy.
   - Docs that say "root" when they mean the operator's *name* (`docs/CLI.md`
     §2 tags, `docs/runbooks/run-the-mesh.md`). Leave the ones that mean
     the *authority* alone.

1. ~~**Signed extrinsics.**~~ **Done, 2026-09-21.** `AccountId` is
   `AccountId32`; `/call` is gone; `/submit` takes a signed
   `UncheckedExtrinsic` and `miot` verifies it for real through
   `frame_executive::Executive` before dispatch. `kot` and `miot --rpc`
   both sign through the shared `miot_runtime::client::sign`.
2. ~~**Wire `miot-store` into `miot`.**~~ **Done, 2026-09-22.** Persists
   every block's effects (not raw extrinsics — `TaskTable::apply`, the
   event-sourcing decision above, is what made replay a matter of folding a
   log rather than re-deriving one); replays on start. Verified: standalone
   kill/restart and `docker compose restart|up --force-recreate node` both
   reproduce identical `/events`/`/tasks`. Prerequisite for item 5, below.
3. ~~**Run `dist/storeprobe` on an Akuma guest.**~~ **Superseded, 2026-09-22
   — the bigger claim turned out true first.** Rather than the diagnostic
   probe, the *full* `miot` was run directly on the actual Akuma kernel
   — not the real `akuma` host below, but `akuma-guest`, a Firecracker
   microVM nested inside the Lima VM `fc` (`../akuma/overlays/
   devbox-firecracker/`). Real multi-threaded tokio, real axum HTTP server,
   real ParityDB (sparse mmap'd files — the exact thing `storeprobe` exists
   to test, now proven live instead of by a diagnostic stand-in), real
   `reqwest` client — running unmodified, as a herd-managed service
   (`/etc/herd/enabled/miot.conf`, auto-starts on boot same as `sshd`).
   Verified end to end: a task opened against the real docker `node` showed
   up in this node's `/tasks` a few seconds later, over the same
   primary/replica HTTP protocol every other node in the mesh speaks.
   `docs/TOPOLOGY.md` has the full diagram and the honest caveat (one boot,
   one binary, a specific syscall surface — not a general claim about
   Akuma). Item 4 below is still genuinely open: this ran on a *Firecracker
   guest*, not the real physical `akuma` host.
   **Correction, same day: not durably up.** ParityDB panics on this guest
   once its index needs to grow past some threshold (`docs/TOPOLOGY.md` has
   the exact panic and what was ruled out chasing it). `node4` runs for a
   while after a fresh store, then crashes and stays down. The claim above
   — real tokio/axum/ParityDB/reqwest working together on Akuma — is still
   true and still the first time any of this ran there; "durable, long-
   running node" is not yet also true, and isn't being chased further right
   now.
4. **Ship `dist/miot` to Akuma** and run a cat there against a host model.
   ~~Needs a host `llama-server` reachable from that box~~ — updated
   2026-09-22, twice: the *node* now runs there **durably** (`node5`,
   herd-supervised, restart-proven over a grown DB — see "What is real"),
   the x86_64-unknown-linux-musl build + busybox-wget transfer path is
   proven, and the kernel's mmap/`truncate`/`fadvise` blockers are fixed.
   What's still open: a cat on the box (an exposed inference port decision —
   unchanged), and the replica direction (node5 as a replica of mac's log —
   it wedged mid-catch-up on the pre-fix kernel and is worth one retry now).
5. ~~**A second node.**~~ **Done, 2026-09-22.** `miot` gained a role
   (`MIOT_ROLE=primary|replica`) rather than a full P2P/gossip layer — a
   replica pulls its peer's block log over two new HTTP endpoints
   (`GET /chain/head`, `GET /chain/blocks?from=N&limit=M`) and folds new
   blocks in through the same `Node::apply_block` a local-store replay
   already used, refusing `/submit` itself (read-only). Deliberately not
   called "leader"/"follower" — `pallet-litter`'s `leader` is already the
   *litter* leader (an agent role); this is a different axis and needed
   different words: **primary**/**replica**. No election, no automatic
   failover — an operator changes a node's role by restarting it with
   different env vars, same trust model as everything else here.
   Verified live: `node2` (`overlays/local/docker-compose.yml`) mirrors
   `node`'s `/tasks`/`/events` within one sync interval and survives its own
   restart. More importantly, `rewind_for_fork` finally ran against a real
   disagreement rather than a synthetic one — a standalone third `miot`
   was pointed at `node`'s peer as a replica, killed, restarted independently
   as its own primary, given a submit that only it received (diverging its
   log), then pointed back at `node` as a replica again. It printed `sync:
   diverged from peer above block 481, rewound to 0 (dropped 485 block(s))`
   and came back byte-for-field identical to `node`. One real bug found and
   fixed doing this: an early version compared only the tip block for
   divergence, which missed it — an empty `Vec<Effect>` (a "quiet" block,
   the common case) encodes identically no matter which chain produced it,
   so a diverged block sitting under a few agreeing quiet blocks above it
   went undetected. Fixed by comparing the replica's *entire* local range
   against the peer once, at (re)connect (`reconcile_if_diverged`), rather
   than the tip on every tick — cheap because a replica that only ever
   appends blocks it received from its peer cannot diverge from it again on
   its own before the next restart. **Follow-up, done same day**:
   `compact()` was never called anywhere at the time this was written, so
   every reconciliation landed at genesis rather than a partial rewind —
   correct per the documented rule, just expensive. Wired up right after
   (`compact()` fires on root's `/clear`; see "real compaction" in "What is
   real," above) — reconciliation now lands on the checkpoint instead.
6. **Open, observed 2026-09-23, not yet root-caused: `@name`-tagging a cat
   in the REPL doesn't reliably get a reaction.** Reported live against the
   deployed fleet — operator types `@sora ...`, sora doesn't respond. Traced
   the plumbing that *should* make this work and it looks correct at every
   layer checked: `Effect::wakes()` (`crates/miot-primitives/src/lib.rs`)
   returns `true` for a `Said` with `to: Some(_)`, and `agent.rs`'s event
   loop does filter `/events` on `wakes == its own hex account`
   (`crates/kot/src/agent.rs:289`). Neither end was changed this session.
   Didn't reproduce this live before writing it down (out of scope for the
   session that found it), so treat the above as "nothing obviously wrong
   in the code path," not "cause found." Things to check first, before
   assuming a deeper bug: whether the tagged cat's agent loop is actually
   running at all right now (a node with no `--llm`/`--glm` is silent by
   design, not broken — `docs/TOPOLOGY_TARGET.md`'s "blocked" agents), and
   whether `@name` in the REPL resolved to the account you expected
   (`parse_targets`/`Roster::account`, `crates/kot/src/client.rs`) —
   `mesh.env`'s `MIOT_ROSTER` was relabeled to cat names this same session
   (`root`/`meow`/`tama`/`kuro`/`sora`/`mimi`), so a stale roster string in
   an operator's shell env would silently resolve `@sora` to nothing or the
   wrong account rather than erroring.

   **Update, 2026-09-23, a related but distinct bug found and fixed:**
   `agent.rs`'s `SendMessage` handling hardcoded `to: None` for every reply
   a cat sent, discarding the tool's own `to` argument entirely — so a
   *cat* could never address another cat by name (only the operator's own
   `kot say --to`, `client.rs`, a different code path, ever worked). This
   doesn't explain the report above by itself (that was a human tagging a
   cat, and `client.rs` already resolved `to` correctly), but it's worth
   re-checking `@name` against the live fleet now that it's fixed: a reply
   from the tagged cat would previously have gone out as a broadcast
   regardless of what it meant to say, and — separately found the same
   session — a non-root broadcast doesn't wake anyone's agent loop at all
   (`Effect::to()` collapses to `None` same as a real broadcast, so
   `agent.rs`'s wake filter never matches). Full detail: `docs/LOCAL_SIM.md`.

7. **Finish the `deploy.py` rollout: three agents still on the old binary.**
   `mac-linux-aarch64` was redeployed with `deploy.py` 2026-09-23 and
   confirmed live; `ryzen-linux-amd64`, `ryzen-akuma-amd64` and
   `mac-akuma-aarch64` were not reachable that session (off the home LAN)
   and still need `python3 overlays/deploy/deploy.py up <agent>` each, once
   back on it. `dumpster-akuma-amd64` (bare metal) is deliberately excluded
   from this round, not just untested — redeploy it by hand
   (`deploy.sh up dumpster-akuma-amd64` or `deploy.py up dumpster-akuma-amd64`,
   either works) only when it's specifically wanted, not as part of "the
   rest of the fleet."

---

## Where to read

- `README.md` — the diagrams and the agent/CLI split
- `docs/PROTOCOL.md` — **the canonical reference**: vocabulary (`TaskId`,
  `Act`, `Effect`, `Directive`), the tx/state/event distinction, every
  dispatchable's authority, the actual tuned timer values (not
  `miot-primitives`'s generic doc-comment defaults), GC, and which tools an
  agent gets for which wake reason. Read this before re-deriving any of it
  from source again.
- `docs/MAPPING_REPORT.md` — design of record: the findings table (§1.1), the
  misconception the port introduced (§1.2), what was deliberately not
  rebuilt. §7 has the open, not-yet-built design questions (wayward,
  participant notification, on-chain tagging) — `docs/PROTOCOL.md` is what's
  built, this is what's proposed.
- `docs/RESULTS.md` — **what actually ran, with numbers.** Evidence, not
  intentions. Read this before trusting any claim elsewhere.
- `docs/CLI.md` — `miot-cli` requirements. Scrollback is sacred.
- `docs/MESSAGE_ROUTING.md` — how a write actually gets to sealed: the
  three-way `Route::{Here,Primary,Nobody}` decision every `/submit` and
  `/account` makes, the mempool queue and peer relay `Nobody` falls into
  since 2026-09-25, and how `/tx/{hash}` learns "sealed" by asking rather
  than by re-deriving it from a synced block. Diagrams, and what it doesn't
  fix (a node with zero reachable peers still can't relay across nothing).
- `docs/LOCAL_SIM.md` — running the agent loop and multi-cat behavior with
  no fleet and no real infra: `kot chat` (a model, in-process, no chain),
  and a peered local mesh of two `kot run` cats against dev `llama-server`s.
  Two real bugs and two open findings came out of actually doing this.
- `docs/runbooks/run-the-mesh.md` — the everyday loop: build, `deploy.sh up`,
  `kot peers` to see who leads, logs, and what "stable" looks like.
- `overlays/deploy/deploy.sh` — the deployment blueprint: one script, three
  shapes (akuma/herd, linux/systemd, lima/systemd).
- `docs/references/storage.md` — the two stores, with measured binary costs
- `docs/references/README.md` — **three event loops, four orders of magnitude
  apart, and none may await another.** Both of meow's deadlocks were that
  mistake.

## One thing to keep

`docs/MAPPING_REPORT.md` §1.1 is a table of findings that were only learnable
by running the thing — timers measured in LLM turns, bounded nudges, "root is
not a worker", "accept a submit without a claim". Each is a named test in
`miot-tasks`. A failure there means something learned the expensive way has
been quietly un-learned. That table is the actual asset; the code is
replaceable.
