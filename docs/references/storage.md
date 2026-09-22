# Storage — what a cat persists, and with what

Decision, 2026-09-21, **revised after measuring** and after a correction to the
premise. Numbers below are from real builds (macOS aarch64, `opt-level=3`,
LTO, `panic=abort`, stripped).

## Two stores, not one

The first version of this document reached the wrong answer by treating
"storage" as one problem. It is two, with opposite requirements:

| | The chain write path | The agent-local store |
|---|---|---|
| Holds | block log + one `State` blob | tool call results, transcripts, turn records |
| Size | single-digit KB | **megabytes, and growing every turn** |
| Access | append; read one blob; replay | *"what did tama actually run?"* |
| Shared | replicated across cats | never leaves the machine |
| Wants | nothing | **a query engine** |

The original evaluation looked only at the left column, concluded "no SQL
workload", and was right about that column and wrong about scope. **Tool call
results are the query workload**, and they are the bulk of what a cat produces.
Once they go in the same place, being able to ask questions of it stops being a
nicety.

## Measured cost

| | stripped binary | delta over baseline |
|---|---:|---:|
| baseline (hello world) | 296 KB | — |
| `parity-db 0.5` | 669 KB | **+373 KB** |
| `turso 0.8.0-pre.11` | 13 412 KB | **+12.8 MB** |

Turso is not small. In our binary the marginal cost is lower — the agent
already links tokio and reqwest — but it is still several megabytes, against a
`miot` binary (named `miot-sim` at measurement time) that was 1.6 MB.

**Verified, not assumed:** the file Turso writes opens in the system `sqlite3`
binary and returns the row. The format-compatibility claim holds.

```
$ ./tdb /tmp/probe.db
Text("v")
$ sqlite3 /tmp/probe.db "select * from t;"
k|v
```

## Decision

1. **Chain write path: ParityDB** (`crates/miot-store`). At +373 KB it is
   small enough that hand-rolling an append-only file would trade crash-safety
   and atomic commits for nothing. A block and the state it produced go in **one
   commit**, so a crash can never leave a block whose state is missing — which
   would be a log that cannot be replayed or rewound to.

   The crate depends on `parity-db` and **nothing else**: no FRAME, no SCALE,
   no runtime. It stores opaque bytes keyed by height, which is what lets it
   test natively in 0.15 s and means a change to the block format never touches
   storage.

2. **Agent-local store (`miot-bodies`): Turso.** Tool call results, turn
   records, prompts and transcripts. This is where the volume is, this is where
   the questions are, and it is per-cat and never replicated — so a heavy
   dependency here costs a binary, not a protocol.

That split puts the megabytes where the megabytes already were, and keeps the
part that has to be boring boring.

### Why Turso rather than `rusqlite`

Pure Rust. No C toolchain, so it cross-compiles to
`aarch64-unknown-linux-musl` — the same property that made us drop RocksDB, and
the one an Akuma guest will need. `rusqlite` would reintroduce exactly the
dependency we removed.

### The store is a cat's memory, not the litter's

**One Turso file per cat, and it is private.** Nothing reads another cat's
store, ever — not the leader, not the operator, not the chain. Tool calls and
their results just lie around locally.

An earlier draft of this section got that wrong in its example queries
(`WHERE cat='tama'`, `GROUP BY cat`), which quietly assume one store holding
every cat's history. There is no such store. The queries a cat can actually run
are scoped to itself:

```sql
-- what have I already tried on this task? (the one that matters in-loop)
SELECT ts, tool, args, result FROM tool_calls WHERE task='t1.2' ORDER BY ts;

-- did I already read this file, and what did it say?
SELECT result FROM tool_calls WHERE tool='FileRead' AND args LIKE '%memory.rs%';

-- where is my own token budget going?
SELECT tool, count(*), sum(tokens), avg(ms) FROM turns GROUP BY tool;
```

The first is the valuable one, and it is valuable *inside the agent loop* rather
than to an operator afterwards: a cat picking up re-homed work can ask what it
already knows before spending a turn rediscovering it.

Cross-cat comparison — *which cat is worth its wall clock* — is therefore **not
a query**. It is an operator walking each cat's file, or it is built from what
cats chose to publish. That is a real limitation and the right one: making it a
query would mean shipping every cat's tool output somewhere central, which is
precisely what this design refuses.

### Publishing is an act, not a default

The asymmetry the litter already uses for messages applies to tool output too:

| | where it goes | bounded | who sees it |
|---|---|---|---|
| raw tool output | the cat's own Turso file | no | only that cat |
| what the cat **chooses to say** | on chain, as a result or message | `MaxResult`, 16 KiB | the litter |

A cat *may* send a tool result to everyone. It does so deliberately, by putting
it in a `done` or a message, and it pays the size cap to do it — which is the
forcing function that makes it summarise rather than paste. Everything it does
not publish simply lies around locally and is nobody else's business.

### Tools are per-cat too

Cats do not share a tool surface. One may have a shell, another may not; one
model supports tool calls at all and another does not (`gemma3:4b` refuses
outright — see `RESULTS.md`). The litter is heterogeneous in **capability**, not
only in persona.

That is the deeper reason re-homing exists. A sub-task can be undoable by its
assignee for reasons that have nothing to do with effort — it lacks the tool, or
its model cannot call one — and no amount of nudging fixes that. The work has to
be able to reach a cat that can actually do it.

## Conflict resolution: the leader wins

No fork-choice rule, no longest-chain, no voting. **The leader's chain is
canonical by definition.** A cat that finds its log diverging rewinds to the
fork point, discards its own blocks above it, and adopts the leader's.

**Rewind lands on a compaction, or on genesis — never on an arbitrary height.**

```
                  ⬇ last compaction
  ours    1 ── 2 ─[3]─ 4 ── 5' ── 6' ── 7'     fork_point(theirs) = 4
  leader  1 ── 2 ─[3]─ 4 ── 5 ── 6 ── 7 ── 8   rewind → 3, state@3, dropped 4
  after                [3]                     replay leader 4..8
```

The rewind is deliberately **coarser than the divergence**. Landing on the exact
fork point would need a state snapshot at every height; landing on a compaction
needs one per epoch, and the cost is replaying a few blocks that were never in
dispute. That keeps the set of recovery targets to a handful of well-known,
already-agreed heights instead of all of them.

A fork *below* the last compaction lands at genesis — we cannot restore a state
we no longer hold, so the honest answer is to rebuild rather than pretend. Rare
by construction: a compaction is a point the litter has already agreed on.

Right for a litter, badly wrong for a public chain — the difference is the
trust model. This is one operator's swarm in one trust domain, so the expensive
machinery that exists to stop a leader lying buys nothing.

**A rewind discards records, not work** — which is less costly than it first
looks, and worth being precise about.

What a cat *did* — its tool calls, their output, what it learned — is in its own
local store, and no rewind touches that. What goes is the on-chain *claim* of
having done it. Since the compaction state carries the open sub-task list
forward, the task is still open afterwards: the tick re-offers it and the cat
resubmits from what it already holds. One cheap turn, not a redo.

The protocol already has the paths. A post-rewind resubmission arrives without
a claim and possibly past its lease — which is exactly *"accept a submit
without a claim"* and the late-submit branch, both of which exist because
losing an answer is worse than losing the ceremony.

The exposure is bounded as well: only blocks above the last compaction can be
discarded, so the window is one epoch of churn rather than all of history.

### Compaction is the recovery boundary

State is written at compaction and nowhere else, which is what makes the rewind
target small, known and agreed.

It is also the same boundary meow already had. Its compaction marker was where
a cold agent stopped paging history (`LITTER_STATE_MACHINE.md`); here it is
*also* where a diverged cat rewinds to. One concept, two uses, and the second
falls out of the first rather than being invented.

Compaction drops the blocks below it in the same commit — after a compaction
there is nothing to replay from below it, so keeping them would be keeping
history nobody can use.

## How a second node actually does this — HANDOFF item 5, 2026-09-22

Everything above was designed and unit-tested (`crates/miot-store/src/tests.rs`,
especially `a_cat_that_diverged_converges_on_the_leader`) before there was a
second `miot-node` process for it to run against. There now is one.

**Vocabulary note**: this section is about which node's block log is
canonical — a different question from `pallet-litter`'s `leader`, which is
the *litter* leader, an agent role (`mimi`). To keep the two apart, a node's
role is **primary** (produces blocks, accepts `/submit`) or **replica**
(pulls a primary's log, read-only) — never "leader"/"follower".

`MIOT_ROLE=primary` (default, today's only behavior until this) or
`MIOT_ROLE=replica` + `MIOT_PEER=<primary base URL>`. A replica requires
`MIOT_DB` (persistence) — without a durable log of its own it has nothing to
compare against a peer's blocks. Two endpoints exist only for this, kept
under `/chain/*` and never on the cat-facing `/head` (whose `leader` field is
the unrelated litter leader):

- `GET /chain/head` → `{"head", "last_checkpoint"}`, straight off the store.
- `GET /chain/blocks?from=N&limit=M` → raw stored block bytes, hex-encoded,
  for heights `[N, min(head, N+M-1)]`.

A replica does two things, in `crates/miot-node/src/main.rs`:

1. **`reconcile_if_diverged`**, once, before it starts serving (covers both
   a fresh replica and one restarting after having run independently).
   Fetches its peer's blocks for its *entire* local range and runs
   `Store::fork_point` over all of it, not just the tip. That "not just the
   tip" is load-bearing, found live standing this up: an empty
   `Vec<Effect>` — a "quiet" block, the common case — encodes identically no
   matter which chain produced it, so a diverged block sitting under a few
   agreeing quiet blocks above it is invisible to a tip-only comparison.
   Comparing the whole range is what catches it. If `fork_point` comes back
   below the local head, `Store::rewind_for_fork` runs for real and the
   in-memory state rebuilds from `genesis()` — see below for why that's
   always a full rebuild today, not a partial one.
2. **`sync_once`**, on a timer thereafter (`MIOT_SYNC_MS`, default
   `BLOCK_MS`): tails whatever new blocks the peer has, folding each one in
   through `Node::apply_block` — the same step a local-store replay on
   restart already used. No fork check here; a replica that only ever
   appends blocks it received from its peer cannot diverge from it again on
   its own before the next restart, so step 1 doesn't need repeating per
   tick.

**Because `compact()` is never called anywhere in this node yet** (a known,
separate gap), `last_checkpoint` is always 0, so `rewind_for_fork`'s `land`
is always genesis — any detected divergence today means "discard the whole
local log and full-replay the peer's history from block 1," never a partial
rewind to a nearby checkpoint. That is the documented rule above working
exactly as designed for a chain with no compaction yet, not a bug in the
replication code. Wiring in `compact()` (still a separate, open item) would
let both `reconcile_if_diverged` and a real fork's rewind start from the
latest checkpoint instead of genesis, with no change needed in either.

Verified live: a standalone third `miot-node`, run first as a replica of
`node` to build matching history, then killed and restarted as its own
independent primary (same genesis, no peer) and given a submit only it
received, then pointed back at `node` as a replica again — printed `sync:
diverged from peer above block 481, rewound to 0 (dropped 485 block(s))` and
came back with `/tasks`/`/events` byte-for-field identical to `node`'s. That
is `a_cat_that_diverged_converges_on_the_leader` happening for real, not in
a unit test.

## Risks accepted

- **`0.8.0-pre.11` is a pre-release.** Acceptable *here* and not on the chain
  path: if Turso breaks, a cat loses its local history and the litter keeps
  running, because nothing in consensus reads this store.
- **+12.8 MB.** Fine for a host agent. Worth re-measuring before an Akuma
  guest, where the whole point was a small static binary.
- **io_uring fast paths are Linux-only.** Irrelevant at our write volume; the
  portable backend is fine. Re-check if that ever stops being true.
