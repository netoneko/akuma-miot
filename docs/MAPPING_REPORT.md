# Akuma Miot — architecture

*Miot Kotów* — Polish for "a litter of kittens". **Akuma** is the signature cat,
and the kernel the litter already runs on.

**v3, 2026-09-21.** Re-cut after four decisions, the last of which changed the
shape of the whole thing:

1. **Everything is `std` + tokio.** The only `no_std` crates are the ones FRAME
   needs to be.
2. **meow is not the base.** We harvest its *semantics and findings* — only
   learnable by running the thing — and none of its structure.
3. **No separate node, no forkless upgrades.** Every cat carries the chain.
4. **FRAME, executed natively. No wasm.** Which follows from (3): the blob
   exists to be swapped on chain for an upgrade, and we do not upgrade. FRAME's
   runtime side is an ordinary Rust library — `pallet-litter`'s own tests have
   always run it that way — so the runtime is compiled code and `sc-executor`,
   the wasm toolchain and the state trie all fall away with it. The upgrade
   path stays open: add `substrate-wasm-builder` and `impl_runtime_apis!` later
   and the same runtime compiles to a blob.

**What that bought, measured:** a 1.6 MB binary, a 1 m 39 s cold Linux build,
and three apt packages. See `RESULTS.md`, which is evidence rather than
intentions — including live runs against real models.

**nca is out of scope.** Not a base, not a fork, not a harvest source. It stays
where it is, untouched.

Two corrections from v1, both found by checking rather than remembering:
`nca` declares `genai 0.5.3` but no Rust file references it (it hand-rolled
seven provider adapters); and `rt.rs` was listed as "survives unchanged" when
it has exactly one production caller and Miot deletes that caller.

Claims about polkadot-sdk I have not verified against a pinned checkout are
marked **[verify]**.

---

## 0. Thesis

The litter protocol is already a blockchain that refuses to admit it — an
ordered record, a deterministic pure application driven by that record, a peer
layer that moves bytes without opinions, compaction-as-state-sync, and an
identity story whose own documentation says the missing piece is *address
recovery from a signature*. That last sentence is `ensure_signed(origin)`.

`LITTER_WORKFLOW.md` says it out loud: *"a hand-rolled Tendermint with futures
bolted onto it."* It is not wrong. **Akuma Miot is that observation taken
seriously: the record and its deterministic application become a FRAME runtime,
the agent half is rebuilt on tokio, and the behavioural findings carry across
intact.**

The consensus *machinery* — wasm, the trie database, libp2p, GRANDPA — turned
out to be separable from the part that was worth having. What we kept is
`ensure_signed`, a state machine that ticks on block cadence and waits for
nobody, and an ordered record. What we dropped is everything that existed to
support forkless upgrades we are not doing.

---

## 1. What we take, and what we leave

### 1.1 The findings — this is the actual IP

Every line below was found by running agents and watching them fail. None of it
is derivable from a design document, which is the only reason it is worth
carrying across a rewrite that keeps none of the code.

| Finding | Where it was learned | Becomes |
|---|---|---|
| **Timers are measured in LLM turns, not seconds.** `CLAIM_WINDOW_US` was 180 s — *shorter than one turn* — so offers lapsed and were re-made while the assignee was still thinking about the first copy. Now 600 s. | `tasks.rs` | pallet `Config` constants, in blocks |
| **Reminders must be bounded.** Every nudge wakes a holder and costs a turn. Unbounded reminders pay forever for an agent that was never going to answer, while the rest of the work queues behind it. `MAX_WORK_NUDGES = 3`, then the lease requeues. | `tasks.rs` | pallet storage + `on_initialize` |
| **Nobody is left holding work in silence.** Claiming ends a turn; a replicated record is non-waking by design; so a claimed sub-task rides its lease out untouched. Observed live: two agents claimed, both turns ended cleanly, neither ever reported. | 2026-09-20 | nudge-on-claim, then the budget |
| **Auto-feed inbound, require explicit completion.** The asymmetry is the throughput lever: an agent absorbs many events in one working context and stops when *it* decides, instead of re-deciding its job every message. | `LITTER_WORKFLOW.md` | agent loop policy |
| **Directives must name the exact verb.** A 0.8B model will not infer `[artifact: t1]` from a design document. The table delivers `[plan-needed:]`, `[clearance-needed:]`, `[artifact-needed:]`. | `tasks.rs` | pallet events → agent prompt |
| **Every prompt must carry the parent question.** A turn is stateless, so the chain is the only memory there is. A worker shown only its sub-task answers it in isolation; a leader shown only the results synthesises from findings it can no longer relate to a question. Two consecutive live runs produced confident, well-formed reports on entirely the wrong topic. | live, 2026-09-21 | `question` refetched per effect |
| **One tool with a status enum beats five similar tools.** A small model picks a *value* more reliably than it picks among near-identical tool names, and a new act costs a value rather than new surface. | `TaskUpdate` | one dispatchable, one enum |
| **Root is not a worker.** A leader canvassing the litter split its task four ways and gave the fourth to the operator. That sub-task could never be claimed; an artifact requires every sub-task cleared; the parent could never close. | live, once | pallet invariant |
| **Work must be able to change hands.** An assignee is not the owner of a sub-task for life. A cat dies, wedges, or reports `failed`, and the work has to reach one that can do it — otherwise the parent stalls forever on a member that will never answer. | §1.2 below | `reassign`, leader-only |
| **Re-offering is bounded, like nudging.** An unclaimed offer is re-made `max_reoffers` times and then stops, and the table asks the leader to re-home it by name (`ReassignNeeded`). An unbounded re-offer is the same loop an unbounded nudge is: paying forever for an agent that was never going to answer. | §1.2 below | `on_initialize` |
| **Accept a submit without a claim.** The handshake is in the protocol, but a small model that skips to the result should not have its work thrown away. Losing the ceremony is cheaper than losing the answer. | `tasks.rs` | pallet transition |
| **Applied-vs-refused must be typed, never sniffed.** Three call sites re-derived acceptance by testing whether a note *began with the word "refused"*. The "already claimed" refusals said no such thing, so a no-op would have been replicated to the whole litter as though it had happened. | `tasks.rs` | `DispatchResult` |
| **Broadcast is non-waking; targeted traffic wakes.** Waking four agents per record turns one task into sixteen LLM turns. | `serve.rs` | event filter in the agent |
| **Fail fast when coordination goes silent.** WAYWARD's tools error with "hub unresponsive (last heartbeat Ns ago)" rather than hanging the turn in an undrained backlog. | `hub.rs` | same rule, chain-disconnected |
| **Never hold a lock across I/O.** `serve::drain` held `PMutex<HubState>` across a deadline-bounded read; the deadline poll hook called `local_drain`; `local_drain` re-locked. Both threads parked in `FUTEX_WAIT` on the same word at the same PC. From outside it looked like three unrelated faults. | 2026-09-20 | unrepresentable — see §2.2 |

### 1.2 A misconception the port introduced, and how it surfaced

Worth recording, because it is the exact failure mode this document's whole
"carry the findings, not the code" premise is supposed to prevent.

`LITTER_WORKFLOW.md` states the operator's exclusion as **four** things:

> it is never assignable: not planned to, not offered to, not listed as
> available, and **not a re-homing candidate**.

The first port read that as one rule — *refuse `root` at plan time* — and
implemented exactly that. But the fourth clause only means anything if
**re-homing exists**, and it had been dropped on the way across. What survived
was an exclusion from a mechanism that was no longer there.

The consequence was not cosmetic. `assignee` is set once, at `plan`, and
`Requeue` deliberately preserves it — the work is still *theirs*, merely
unclaimed. So a sub-task was bound to its cat **for life**: a cat that died,
wedged or simply could not do the job got the same offer re-made forever, the
nudge budget bounded only nudges, and the parent could never close because an
artifact requires every sub-task cleared. The identical stall the "root is not
a worker" finding exists to prevent, reachable without involving `root` at all.

**How it surfaced:** asking what it would take for one model to help another
recover from a failed feature attempt. The answer required work to change
hands, and nothing could move it.

**The fix** is two bounded mechanisms, matching the shape the litter already
uses everywhere else:

- `max_reoffers` bounds re-offering exactly as `max_nudges` bounds nudging.
  Past the budget the table stops and raises `Directive::ReassignNeeded` — a
  named verb for the leader, not a silence.
- `reassign(task, to)` is a **leader act**, alongside `plan`, not a value in
  `Act`. That enum is what a worker does to its own task and every one of its
  acts takes only text; this one takes another account.

A failed result is discarded on re-home, so the new holder starts clean rather
than inheriting a wrong answer as context. A `Cleared` sub-task cannot be
re-homed — that would reopen settled work behind the leader's own clearance.

The general lesson: **a finding stated as an exclusion is evidence of a
mechanism.** When porting one, check the mechanism came too.

### 1.3 The structure — what we are deliberately not rebuilding

Diagnosed from the code, so the new thing does not re-earn any of it:

1. **The core is untestable.** meow cannot `cargo test` natively *at all* —
   raw Linux syscalls throughout `libakuma`, `no_std` end to end. That is
   precisely why `litter-raft`, `litter-wire` and `litter-hub` were extracted
   into sibling crates, and why there is an in-binary `meow test` suite behind
   a `tests` feature flag. **~8,900 lines reachable only through
   aarch64-musl-in-Docker.** This is the root defect; most of the others are
   downstream of it.

2. **Hand-rolled JSON walkers with positional zipping.** `TaskPlan` parses its
   assignments as *parallel flat arrays* —
   `strings_at(args, &["assignments","*","who"])` and the same for `what` and
   `expect` — then zips them back into pairs. The code documents its own
   failure modes: a ragged plan "loses the tail rather than the whole call",
   and `expect` "cannot be zipped positionally — an assignment that omits it
   would shift every later expectation onto the wrong sub-task", so the column
   is only trusted when complete. This entire bug class is a `serde` derive.

3. **One binary does everything.** CLI, TUI, agent loop, hub, raft, relay,
   17 tools, HTTP client and TLS, under five feature flags that change
   *behaviour* rather than adding capability (`compact-tools` silently drops
   every tool description from the schema; `size` caps tool output at 2 KB
   instead of 32 KB).

4. **Static mutable globals as the concurrency model.** Hub address, token
   budget, debug mode, the leaked owner context, the signing keys — all
   process-wide statics, with the convention recorded as *"single-threaded by
   design — same convention as the other litter statics."*

5. **Transport fused to semantics.** `serve.rs` (833 lines) does framing,
   authority stamping, task hooks, history paging and compaction in one type.

6. **`rt.rs` (384 lines) exists to own a socket.** Raw `clone(2)`, a
   hand-built TCB with a self-pointer for `%fs`, per-target `CLONE_SETTLS`
   divergence — Linux refuses it with a NULL tls, Akuma/amd64 refuses its
   *absence* — a 64 KiB `.bss` stack table. One production caller
   (`live.rs:831`), reached only when the process won the bind race. Miot's
   agent is a client; the caller disappears and so does the file.

### 1.4 Code we still read, but do not import

| From meow | Lines | Why it is worth reading during the port |
|---|---:|---|
| `tasks.rs` | 1550 | The lifecycle, in full, as a pure state machine. The reference for `pallet-litter`'s behaviour — *not* a source file to copy. |
| `litter-wire` | 973 | How to keep two ends of a protocol from drifting. Same discipline, new crate. |
| `sig.rs` | 185 | The key model: a key is an **agent's** identity, not a litter's. Carries over exactly. |
| `record.rs`, `membership.rs` | 345 | The record/peer-layer split that made the Polkadot mapping obvious in the first place. |

Everything else — `live.rs`, `serve.rs`, `rt.rs`, `api/client.rs`,
`litter-raft`, `litter-hub`, `hub.rs`, `relay.rs`, the tool layer — is
superseded.

---

## 2. The system

### 2.0 Two laws

Everything below follows from these, and they are the litter's own finding
restated: the raft thread *"is never blocked by inference, which is the
concrete consensus-networking-strictly-decoupled-from-model-worker-tasks."*
The chain is that thread now.

> **I. The chain never waits.** The state machine is bounded and eternal. It
> ticks on block cadence forever, whether or not any agent answers. Every
> deadline is a block height. An agent that goes silent costs the chain
> nothing — its lease expires and the work requeues.
>
> **II. The agent never blocks.** Not on the chain, not on a tool, not on the
> model. Chain events, tool results and body fetches are all just inbound
> traffic. Submits are fire-and-forget; confirmation arrives later as an event
> like anything else.

An LLM turn is minutes. A block is seconds. Nothing may be built that assumes
the first finishes inside the second.

### 2.1 Deployment

**The chain is the only channel between agents.** Nothing else crosses a host
boundary — the litter's *"the hub socket is the only channel"*, carried forward.

```
╔══════════════════════ AGENTS — std + tokio, one binary ══════════════════════╗
║   akuma guest             linux host              linux host                 ║
║   (aarch64-musl)                                                             ║
║  ┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐           ║
║  │ miot-cli "tama" │    │ miot-cli "kuro" │    │ miot-cli "root" │ operator  ║
║  │ ┌─────────────┐ │    │ ┌─────────────┐ │    │                 │           ║
║  │ │ miot-bodies │ │    │ │ miot-bodies │ │    │                 │           ║
║  │ │   LOCAL     │ │    │ │   LOCAL     │ │    │                 │           ║
║  │ │ working set │ │    │ │ working set │ │    │                 │           ║
║  │ │ tool output │ │    │ │ tool output │ │    │                 │           ║
║  │ │ never shared│ │    │ │ never shared│ │    │                 │           ║
║  │ └─────────────┘ │    │ └─────────────┘ │    │                 │           ║
║  └────────┬────────┘    └────────┬────────┘    └────────┬────────┘           ║
╚═══════════╪══════════════════════╪══════════════════════╪════════════════════╝
            └──────────────────────┼──────────────────────┘
                   signed extrinsics + event stream
                                   │
                 ╔═════════════════▼══════════════════════╗
                 ║  CHAIN — miot-node                     ║
                 ║    AURA + GRANDPA, 3+ validators       ║
                 ║  ┌──────────────────────────────────┐  ║
                 ║  │ miot-runtime  (wasm blob)        │  ║
                 ║  │  ┌────────────────────────────┐  │  ║
                 ║  │  │ pallet-litter              │  │  ║
                 ║  │  │  ┌──────────────────────┐  │  │  ║
                 ║  │  │  │ miot-tasks           │  │  │  ║
                 ║  │  │  │   the pure machine   │  │  │  ║
                 ║  │  │  └──────────────────────┘  │  │  ║
                 ║  │  │  ids · status · leases     │  │  ║
                 ║  │  │  nags · messages           │  │  ║
                 ║  │  │  RESULTS · ARTIFACTS       │  │  ║
                 ║  │  └────────────────────────────┘  │  ║
                 ║  └──────────────────────────────────┘  ║
                 ╚════════════════════════════════════════╝

                  ── or, before the chain exists ──
                 ╔════════════════════════════════════════╗
                 ║  miot-coord — same miot-tasks, one     ║
                 ║  tokio task, no chain, no wasm         ║
                 ╚════════════════════════════════════════╝
```

`miot-tasks` is the lifecycle as a plain state machine — `no_std`, no I/O, no
clock, `now` a parameter — which is what `tasks.rs` already is.
`pallet-litter` is a thin FRAME wrapper over it (`ensure_signed`, read, apply,
write, emit); `miot-coord` is the same machine in one tokio task with no chain
at all. The chain is a **deployment mode**, so Phases 0–3 need neither it nor
wasm.

Cost of the thin wrapper, stated: the pallet reads and writes the whole table
per extrinsic, so weight and PoV scale with table size. At ~8 agents and tens
of tasks that is a few KB and fine. If it grows, split to a per-task
`StorageMap`; `miot-tasks`'s interface does not move.

### 2.2 Workspace

```
crates/
  miot-primitives/   no_std+alloc  TaskId, TaskStatus, Act, PlanItem, Limits
  miot-tasks/        no_std+alloc  the lifecycle state machine — pure
  pallet-litter/     no_std FRAME  thin wrapper over miot-tasks
  miot-runtime/      no_std wasm   the chain runtime
  miot-node/         std           sc-* node: AURA + GRANDPA
  miot-coord/        std tokio     chainless coordinator (dev + single host)
  miot-chain/        std tokio     subxt: submit, stream events   ─┐ both impl
  miot-bodies/       std           agent-LOCAL store               │ the same
  miot-llm/          std tokio     provider layer                  │ traits
  miot-tools/        std tokio     async tool registry            ─┘
  miot-agent/        std tokio     the loop — a LIBRARY, no I/O of its own
  miot-cli/          std tokio     the binary
```

- **Every crate `cargo test`s on the host.** No Docker, no in-binary suites, no
  feature flag between a test and its code.
- **Features add capability, never change semantics.**

### 2.3 The agent loop — decoupled, async, aggregating

The agent is **not** an event-driven request/response loop. It is a long-lived
task with an inbox, a set of in-flight futures, and a policy that decides when
enough has accumulated to be worth a turn.

```
┌───────────────────────────── miot-agent ──────────────────────────────────┐
│                                                                           │
│  INBOUND — never blocks, accumulates during a turn                        │
│                                                                           │
│   chain events  ──┐                                                       │
│   tool results  ──┼──► inbox ──► ┌──────────────┐                         │
│   body fetches  ──┤              │  AGGREGATOR  │  policy decides WHEN    │
│   operator msgs ──┘              │              │  a turn is worth it     │
│                                  └──────┬───────┘                         │
│                                         │ assemble                        │
│                                         ▼                                 │
│                                  ┌──────────────┐                         │
│                                  │  turn builder│  role · changed ·       │
│                                  │              │  results · working set  │
│                                  └──────┬───────┘                         │
│                                         ▼                                 │
│                                  ┌──────────────┐   minutes. inbox keeps  │
│                                  │   miot-llm   │   filling throughout.   │
│                                  └──────┬───────┘                         │
│                                         │ tool calls                      │
│                        ┌────────────────┴────────────────┐                │
│                        ▼                                 ▼                │
│              LOCAL (a query)                    PUBLIC (a record)         │
│              fs · shell · search                TaskUpdate · TaskPlan     │
│                        │                                 │                │
│                 dispatch, don't await            PUT body → hash          │
│                        ▼                                 ▼                │
│              ┌──────────────────┐               signed extrinsic          │
│              │ in-flight futures│               fire-and-forget           │
│              └────────┬─────────┘                        │                │
│                       └── results ──► inbox              └──► confirmed   │
│                                       (maybe this turn,       later, as   │
│                                        maybe three later)     an event    │
│                                                                           │
│  no shared state · no locks · nothing held across an .await               │
└───────────────────────────────────────────────────────────────────────────┘
```

**A tool call does not end a turn and a turn does not await a tool.** Results
re-enter as inbound traffic and are fed to the model whenever the aggregator
says so — possibly in this turn, possibly several turns later, possibly folded
together with others.

#### The aggregation policy

`trait Aggregate` decides when the inbox becomes a turn. It is configurable per
agent because the right answer differs by role and by model size:

| Policy | Assembles when | Good for |
|---|---|---|
| `Immediate` | any inbound item | interactive operator seat; expensive |
| `Barrier(tool)` | a named tool resolves | "I asked for the build, I want the build" |
| `Quorum` | every tool dispatched this turn resolved | classic batch reasoning |
| `Deadline{ n, t }` | `n` items, or `t` elapsed, whichever first | the sane default |
| `Coalesce` | as `Deadline`, folding repeats of one tool into one summary | chatty tools, small models |

Two rules the policy may not break, both from §1.1: an **assignment or
directive always assembles a turn** (they are the waking traffic), and a
**broadcast record never does on its own** (waking four agents per record turns
one task into sixteen turns).

If results land while a turn is in flight, they queue for the next one. Not
cancel-and-restart: a turn is minutes of spend and the model may already be
committed to a tool call.

### 2.4 The consequence nobody likes: leases can expire mid-work

Law I means the chain requeues a claimed sub-task the moment its lease passes,
with no knowledge that its holder is four minutes into a turn. With async tools
and an aggregating loop, a turn can plausibly outlive a lease.

So the late-submit case needs an explicit answer, and it is a **pallet
decision, not an agent one**:

- **Reject late.** Clean, and throws away real work — the exact mistake §1.1's
  "accept a submit without a claim" warns against.
- **Accept late, idempotently**, if the task is still open and nobody else has
  submitted. The second submitter loses a race, not its work.
- **Recommended:** accept late; record both results; let the leader clear one.
  Leader verification already exists, and this is the branch it is for.

Sizing the lease past the aggregator's worst case matters more than the policy
does. Budget from measurement, not from taste.

### 2.5 Where state lives

Everything the litter needs to agree on is **on chain**. The local store holds
only what nobody else will ever read.

| What | Where | Bounded | Survives agent death |
|---|---|:--:|:--:|
| **Task record** — id, parent, status, assignee, lease height, nag budget | chain | ✅ | ✅ |
| **Messages** between agents | chain | ✅ `MaxMessageLen` | ✅ |
| **Sub-task results** | chain | ✅ `MaxResultLen` 16 KiB | ✅ |
| **Artifacts** — the final report | chain (§2.6) | ✅ `MaxArtifactLen` 64 KiB | ✅ |
| **Roster + identity** | chain accounts | ✅ | ✅ |
| **Working set** — conversation, in-flight futures, aggregation buffer | `miot-bodies`, **agent-local** | ✗ | ⚠️ resumable, not authoritative |
| **Raw tool output** — full build logs, file contents, transcripts | `miot-bodies`, **agent-local**, one store per cat | ✗ | ✗ — nor should it |

`miot-bodies` is a **local store, not a service** — one per cat. No HTTP, no
content addressing across the network, no retention negotiation, no hash
verification between hosts. It never leaves the machine it was written on,
which is exactly why it cannot bloat anything.

Nothing reads another cat's store: not the leader, not the operator, not the
chain. Tool calls and their results just lie around locally. A cat **may**
publish one — deliberately, by putting it in a `done` or a message — and pays
the `MaxResult` cap to do so. Everything it does not publish is nobody else's
business.

Tool *surfaces* are per-cat as well. One cat may have a shell and another not;
one model can call tools at all and another cannot. The litter is heterogeneous
in **capability**, not just in persona — which is the deeper reason re-homing
exists: a sub-task can be undoable by its assignee for reasons no amount of
nudging fixes.

The pressure this creates is deliberate and good: **an agent cannot publish a
40 KB build log.** To get something on chain it has to fit `MaxResultLen`, so
it must say what happened rather than paste what scrolled by. Bounded results
are a forcing function for concise ones.

The invariant: **losing an agent loses its working set and nothing else.** The
chain still knows the sub-task was outstanding, the lease still expires, the
work still requeues, and every result already submitted is still there. That is
the litter's own recovery story with the leader's memory replaced by something
that does not die.

`payload_hash: Option<Hash>` goes into the record shape from day one and stays
`None`. If a payload ever genuinely does not fit, a shared store becomes an
additive migration rather than a redesign.

### 2.6 Artifacts — on chain, markdown, leader-committed

An artifact is the exception that earns itself: **rare** (one per parent task),
**terminal** (the thing the work existed to produce), and the only payload
anyone wants durable and verifiable.

```rust
#[pallet::storage]
pub type Artifacts<T> = StorageMap<_, Blake2_128Concat, TaskId, Artifact<T>>;

pub struct Artifact<T: Config> {
    pub title:  BoundedVec<u8, T::MaxArtifactTitle>,  // 128 — typed, so a
    pub body:   BoundedVec<u8, T::MaxArtifactLen>,    // listing needs no parse
    pub author: T::AccountId,
    pub at:     BlockNumberFor<T>,
}

// leader-only; refused unless every sub-task of `task` is Cleared
pub fn submit_artifact(origin, task: TaskId, title: Vec<u8>, body: Vec<u8>)
```

Runtime validation is deliberately thin: **length bound and valid UTF-8, and
nothing else.** Markdown is never parsed on chain — it is brittle and a model
will violate any structure made into a consensus rule. The format is a
convention; only the bytes are law.

#### Who writes which half

The finding applies directly: *a 0.8B model will not infer `[artifact: t1]`
from a design document.* So the model is never asked to produce what the chain
already knows.

**The agent's code renders the header from chain state. The model writes only
the last two sections.** The agent concatenates and submits, so what is stored
is self-contained — one storage read is a readable document — while the model
never has to spell a field correctly.

```markdown
# Does this codebase actually work?

- **task**: t42 · opened by root at block 1180
- **closed by**: tama (leader) at block 1642
- **litter**: tama, kuro, mimi

## Asked

Debate whether this codebase works and produce a report.

## Sub-tasks

| id    | assignee | status  | claimed → done |
|-------|----------|---------|----------------|
| t42.1 | tama     | cleared | 1184 → 1502    |
| t42.2 | kuro     | cleared | 1186 → 1610    |

### t42.1 — tama
Build passes, 214 tests green, 38 s cold.

### t42.2 — kuro
One lock in memory.rs is taken but never released on the error path.

## Findings          ← the model writes from here down

...

## Answer

Compiles and passes unit tests, but fails under concurrent load due to a
race in memory.rs.
```

#### What the experiment answers

Named before building, or a report arrives and nobody can say whether it
worked:

- Do reports land inside 64 KiB, and what is the distribution?
- Does a small model produce usable **Findings** / **Answer** when the header
  is handed to it rather than asked of it?
- Does `[artifact-needed:]` reliably get a leader to call `submit_artifact`?
  This is the finding most likely to break against a new tool surface.
- Is one artifact per parent the right granularity, or are revisions wanted?

### 2.7 Compaction, which is three unrelated jobs

The litter's `compact()` does three things at once because the leader owned
everything: bound each inbox (`KEEP_RECENT = 16`, fold the rest into one
`"[compacted: …]"` marker of at most `MARKER_SUMMARY_LINES = 20` lines, prior
markers stripped and refolded so repeated passes do not grow the inbox), give a
cold agent a stopping point for history walks, and carry open work across the
boundary (`carry`, appended as `[still open]` last, *"because it is the part
that is still actionable"*).

**Two of the three do not survive.**

- **The bootstrap marker is gone.** A cold agent reads chain state. The task
  table *is* the baseline, so there is no history to page and nothing for a
  marker to terminate.
- **`carry` is gone.** It exists solely because *"the task table is leader
  memory."* Chain storage removes the reason; `open_subtask_lines` has no
  caller.

What remains splits four ways, and only one of them is consensus:

| Job | Where | Deterministic |
|---|---|:--:|
| **Task-table GC** — drop closed parents and their cleared sub-tasks after N blocks | on chain, `on_initialize` | must be |
| **Block/state pruning** | node flag (`--state-pruning`) | n/a |
| **Agent context compaction** — the model's window filling up | agent-local, never on chain | no |
| **Local store eviction** | `miot-bodies` policy | n/a |

The rule that decides every case: **the runtime may never generate a summary;
it may only record a commitment to one.** Worth stating precisely, because the
litter's fold is *mechanical* — `format!("[r{}] {}: {}", round, from,
summarize(body))`, a truncation — so it is deterministic and could legally run
on chain. `LITTER_WORKFLOW.md`'s diagram describes something else entirely
(*"Mimi summarizes open debate off-chain"*), and that one cannot: two
validators executing one block would have to produce identical bytes out of an
LLM.

An optional `Checkpoint` extrinsic — a published summary others start from — is
a Phase 7 idea with a measurable trigger: *when replaying events to build a
working set gets slow.* Until that is measured it is speculative machinery.

Agent-side context compaction gets its own constants, measured against the
model's window. `KEEP_RECENT = 16` and `MARKER_SUMMARY_LINES = 20` are
inbox-shaped numbers, and there are no inboxes.

## 3. Mapping, term by term

| Litter concept | Polkadot equivalent | Fit |
|---|---|---|
| Ordered event log, epoch | the chain; block number replaces epoch | **exact** |
| `TaskTable::apply` | `#[pallet::call]` dispatchables | **exact** |
| `Applied = Result<String,String>` | `DispatchResult` | **exact** |
| `TaskPlan(task, assignments[])` | one extrinsic, atomic by construction | **exact** |
| `TaskUpdate(task, status, text)` | one dispatchable, one status enum | **exact** |
| `[record]` broadcast | `deposit_event` | **exact** |
| `Authority::{Root,Leader,Peer}` | `ensure_root` / `EnsureOrigin` / `ensure_signed` | **exact** |
| address recovery (§ Future work) | `ensure_signed(origin)` — **free** | **the prize** |
| Per-agent Ed25519 keys (`sig.rs`) | `sp_core::ed25519`, `AccountId32` | **exact** |
| Leases, nags, claim windows | block numbers + `on_initialize` | **better** |
| Compaction marker | state pruning + a checkpoint item | **good** |
| Relay envelope `ol/ot/sig/rl/rs` | signed extrinsic; XCM for the cross-litter hop | **good** |
| Bind race = election | AURA authoring rotation | **replaced** |
| `litter-raft` | GRANDPA | **retired** |
| Inbox bodies | `miot-bodies`, off chain | **does not belong on chain** |

### 3.1 Why wall-clock timers becoming block heights is an upgrade

Every timer in `tasks.rs` is microseconds of host wall clock: `CLAIM_WINDOW_US`
600 s, `LEASE_US` 900 s, `WORK_NAG_US` 150 s, `NAG_US` 120 s. The protocol
already works around clock disagreement elsewhere — `litter-wire`'s `ts` is
hub-assigned *"precisely so that an inbox's ordering doesn't depend on every
agent's clock agreeing."* Block number is a clock everyone agrees on by
construction.

At 6 s blocks: `CLAIM_WINDOW` 100, `LEASE` 150, `WORK_NAG` 25, `NAG` 20. All
`Config` associated constants, so a fast local chain and a public one differ by
configuration.

### 3.2 Latency, stated once

A record is not on chain until it is in a block (≈6 s), arguably not until
GRANDPA finalizes (+2 blocks). The litter's current floor is the ~5 s
`PULSE_TICKS`. **The workload's atom is a 120–200 s LLM turn.** 6–18 s is
noise. This is acceptable here and would not be in most systems; say it once
and stop worrying about it.

### 3.3 Solochain now, parachain later — as a deployment choice

Build a **solochain** (AURA + GRANDPA among the agent hosts). It matches the
litter's stated trust model — *"one network, one operator"* — and adds no
relay chain to a swarm of cats on a LAN.

Keep `pallet-litter` XCM-clean so a parachain is a deployment decision rather
than a rewrite: no pallet code assumes it is the only chain, and every
cross-litter act goes through a `trait LitterRelay` whose solochain impl is a
direct call and whose parachain impl is XCM.

---

## 4. `std` / `no_std`

Three crates are `no_std`, and only because FRAME and wasm require it. That is
the entire boundary.

| Crate | no_std+alloc | std | wasm | Akuma (aarch64-musl) | Linux/macOS |
|---|:--:|:--:|:--:|:--:|:--:|
| `miot-primitives` | ✅ | ✅ | ✅ | ✅ | ✅ |
| `pallet-litter` | ✅ | ✅ | ✅ | ✅ builds | ✅ |
| `miot-runtime` | ✅ | ✅ | ✅ | — | ✅ |
| `miot-node` | ✗ | ✅ | — | ⚠️ experiment (§5.2) | ✅ |
| `miot-chain`, `-bodies`, `-llm`, `-tools`, `-agent`, `-cli` | ✗ | ✅ | — | ✅ cross-compiled | ✅ |

Standard preamble on the three:

```rust
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;
```

with `std = ["dep-a/std", …]` propagated and `default = ["std"]`, so
`cargo test` is a native host run and the wasm build passes
`--no-default-features`. Recent polkadot-sdk deprecates `sp-std` in favour of
plain `core`/`alloc` **[verify against the pinned version]**.

Two habits carried over from meow's own `Cargo.toml`, because they will bite
again:

- Keep `std`-only members **out of `default-members`** if any `no_std` target
  build runs from the workspace root — this is exactly why `litter-hub` is
  excluded today.
- Pin TLS/crypto revisions **identically** across the workspace. meow's
  Cargo.toml records why: two different revs of `embedded-tls` is worse than
  one wrong one, because the two binaries then disagree about TLS.

### 4.1 Determinism checklist before `tasks.rs` semantics become a pallet

- [ ] `now_us: u64` → `BlockNumberFor<T>` everywhere.
- [ ] String task ids (`"t1.2"`) → `(u32, u16)`. Strings in storage are a cost
      and a DoS surface.
- [ ] Every stored `Vec` → `BoundedVec`. `MAX_ROSTER_SIZE = 256` already
      exists and becomes `T::MaxRoster`.
- [ ] Human-readable `Ok(note)` strings → an `Event` enum. Prose does not
      survive the trip.
- [ ] No `HashMap`/`HashSet` iteration order may reach state.
- [ ] Weights on every dispatchable. `TaskPlan` is O(assignments) — bound it
      (`T::MaxAssignments`, ~8).
- [ ] The nag budget is per-holder mutable state → a storage map, or it is
      lost on requeue.

---

## 5. Akuma and Linux

### 5.1 The agent

One binary, one target family, both platforms:

```bash
# Linux / macOS host
cargo build -p miot-cli --release

# Akuma guest (and any aarch64 musl Linux)
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc \
cargo build -p miot-cli --release --target aarch64-unknown-linux-musl
```

No `libakuma`, no `no_std`, no `rt.rs`, no per-target `CLONE_SETTLS`
divergence, no Akuma-custom syscalls 300/319 to feature-gate around. A static
musl binary runs on Akuma because Akuma implements the Linux ABI — the same
path that already puts a tokio/ratatui/PTY binary on an Akuma guest today, and
the same class of program as the tokio/hyper/reqwest/rustls workloads the
kernel verified in 2026-08.

Getting it onto a live guest, from `AKUMA_BUILD.md`'s hard-won notes: **not**
scp (no SFTP subsystem, the client hangs), **not** piping through an SSH exec
channel (reproducibly stalls at exactly 1,048,576 bytes). HTTP over QEMU's
SLIRP works — the host is always reachable at `10.0.2.2`.

Building *inside* Akuma stays blocked: cargo's concurrent spawn path hits
`EFAULT` after ~8 spawns, and `ncaprobe bigspawn 50` proved it is not the
weight of a rustc invocation — 50 identical plain `Command` spawns all
succeeded. Cross-compile from the host.

### 5.2 The local testbed — `overlays/local/`

**Stage 1 is plain Linux in one Lima VM.** Three `miot-node` validators, one
`llama-server`, N `miot-cli` agents, all ordinary processes over localhost. No
Firecracker, no TAPs, no disk images, no Akuma.

Every failure drill in Phase 6 — kill an agent mid-claim, partition one, stall
one past its lease, kill a validator — works at Stage 1. None of them needs
Akuma, so none of them should wait for it.

**Stage 2 moves the agents, and only the agents, into Akuma guests** once
Stage 1 runs a parent task end to end: Firecracker microVMs under the same Lima
VM, one TAP each, following `akuma/overlays/devbox-firecracker/run.sh`'s
existing `--via-lima` / `--local` split. Validators stay processes — a
validator needs a port, not a microVM. Same binary, same config, same chain.

The invariant that makes Stage 2 a deployment change rather than a redesign:
**the chain is the only channel between agents**, enforced from the first
Stage 1 run even when nothing on one host would stop you breaking it.

Three validators is the smallest set that survives losing one, which is the
only consensus failure worth rehearsing. Agents are clients, so their count is
independent.

### 5.2 The node on Akuma is an experiment, not a dependency

A `sc-*` node wants tokio's multi-threaded runtime, rust-libp2p, a **wasm
executor** and a **database**. Akuma provides `pipe2`, `eventfd2`, `futex`,
`pselect6`, `ppoll`, `CLONE_VM` threads, demand paging and `mmap`, and has run
tokio/hyper/rustls programs — so tokio and libp2p are plausible on paper. The
other two are the real questions, and they are not equally hard.

#### Wasm — the solvable half

The runtime *is* a wasm blob, stored on chain as `:code`. That is the forkless
upgrade mechanism.

**Correction.** Earlier drafts of this document said native runtime execution
had been *removed*. It has not. `sc-executor 0.50.0` still exports
`NativeElseWasmExecutor` and `NativeExecutionDispatch`, carrying a deprecation
note that reads *"Will be removed at end of 2024"* — still shipping in the 2606
train, well past its own date.

It does not help, though, and the name says why: *Native-**Else**-Wasm*. It
dispatches to native only when the native runtime version matches the on-chain
one and **falls back to `WasmExecutor`** otherwise. The blob is still the
on-chain `:code`, still the fallback, still built. It skips wasm *execution*
sometimes; it never skips the wasm *build*.

So inside `sc-service` the blob is unavoidable whatever executor is chosen —
and outside it, the blob is not needed at all (§5.3).

| | `wasmtime` | `wasmi` |
|---|---|---|
| Kind | JIT/AOT | interpreter, pure Rust |
| Speed | fast | 10–100× slower |
| Host demands | `mmap` + `PROT_EXEC` churn, trap signal handlers, sometimes hugepages | essentially none — a decode-dispatch loop |

At ~10 extrinsics per block on a private litter chain, 100× slower than a JIT
is still nothing. `wasmi` is therefore the right answer here, and it asks
almost nothing of the kernel.

**[verify]:** the Phase 1 dependency fetch pulled `polkavm-common v0.9.0`, so
polkadot-sdk is moving toward PolkaVM/RISC-V. Confirm `wasmi` is still a
first-class executor in the pinned version before betting on it.

#### The database — measured, not argued

A node stores headers, bodies and the state trie. Two backends:

- **RocksDB** — C++. A C++ toolchain to build, LZ4/snappy, heavy mmap, file
  locking, background compaction threads.
- **ParityDB** — pure Rust, purpose-built, cross-compiles to musl without a C
  toolchain. This is what `crates/miot-store` uses.

**Correction, twice over.** Earlier drafts of this document said (a) an
in-memory backend would make this a non-issue, and (b) the database was "the
actual blocker" for a node on Akuma. (a) was wrong — `--tmp` gives a throwaway
*directory*, not a memory-only store, so the mmap and fsync requirements
remain. (b) was reasoning from requirements rather than from a run, which is
exactly what this document's own premise is supposed to prevent. Akuma
advertises `mmap`, demand paging and MMU-backed isolation, and hosts `rustc`,
which mmaps heavily.

So it was measured instead. `cargo run -p miot-store --bin storeprobe` opens a
store, appends 256 blocks, compacts, rewinds, reopens across a process
boundary, and writes a 64 KiB value. On macOS and on bare `busybox`
aarch64 Linux, all seven stages pass.

What it found:

| | |
|---|---|
| apparent size | **97.3 MB** (3 × 32 MB index files) |
| actually allocated | **4.0 MB** |
| verdict | **genuinely sparse — the files have holes** |

So the concern was right in kind: ParityDB does create sparse, mmap'd files.
Whether Akuma's ext2-over-virtio-blk handles holes and file-backed mmap of
them is now **a five-minute test rather than an argument** —
`overlays/local/build-akuma.sh` cross-compiles `storeprobe` to a 0.8 MB static
aarch64 binary, and its exit status is the number of stages it completed.

That staged-exit shape is borrowed from `akuma/userspace/amd64/ruststd`, for
its stated reason: a program that dies at stage 3 has no exit status to report,
so a truncated log has to name the wall it hit.

#### Net

**The database is the blocker; wasm is not.** Which is exactly why the split
holds: run the node on Linux, run agents on Akuma. An agent needs neither an
executor nor a trie — it is an RPC client with a local file store, which is a
class of program Akuma already runs.

## 6. Roadmap

Akuma is **not** on the critical path. Everything through Phase 5 runs on
ordinary Linux, and the litter is fully exercised — including every failure
drill — before an Akuma guest is involved at all.

| Phase | What | Done when |
|---|---|---|
| **0** | `miot-primitives` + `miot-tasks` — the lifecycle as a pure state machine, §1.1 encoded as behaviour. | Every finding in §1.1 has a named test, `cargo test` green on the host. |
| **1** | `pallet-litter` — thin FRAME wrapper over `miot-tasks`, against the §4.1 checklist. Wired to nothing. | The same behaviours pass through `TestExternalities`. |
| **2** | `miot-runtime` + `miot-node` — minimal template, AURA + GRANDPA. First wasm. | 3 validators producing and finalizing locally. |
| **3** | `miot-chain` (subxt) + `miot-llm` + a minimal `miot-agent`. | **First end-to-end:** one agent claims from chain events, runs a turn, submits `done`. |
| **4** | `miot-tools` (async + aggregator) + `miot-bodies` (local) + artifacts. | A parent goes plan → claim → done → clear → artifact across 3 agents; the artifact is readable markdown out of chain state. |
| **5** | `overlays/local` Stage 1 + the failure drills. | Kill an agent mid-claim, partition one, stall one past its lease, kill a validator — the findings hold under all four. |
| **6** | Akuma: cross-compile `miot-cli` to `aarch64-unknown-linux-musl`, `overlays/local` Stage 2. | Akuma guests and host agents claiming sub-tasks of the same parent. |
| **7** | Experiments: a node *inside* Akuma; XCM/parachain; `Checkpoint`. | Each is a blog post; none is a dependency. |

Phases 0–1 need no chain and no wasm — `miot-coord` runs the same state machine
in one tokio task. Phase 6 is a cross-compile because the agent is an ordinary
`std` musl binary, not the hand-rolled SCALE encoder a `no_std` agent would
have required.

## 7. Open questions

**[verify] against a pinned polkadot-sdk, in one sitting:**

1. `sp-std` deprecated — plain `core`/`alloc` in pallets?
2. Current wasm runtime target (`wasm32v1-none` vs `wasm32-unknown-unknown`).
3. Is `wasmi` still a first-class executor? The Phase 1 fetch pulled
   `polkavm-common v0.9.0`, so the executor lineup may have moved. (The
   in-memory-DB question is **settled and the answer is no** — see §5.2.)
4. `frame-benchmarking` worth the setup for a private litter, or are
   hand-assigned constant weights fine?

**Ours, not Polkadot's:**

5. **Does "leader" survive?** The role exists because a socket had to be
   owned, and that reason is gone. `TaskPlan` authority could be a collective,
   a rotating origin, or simply anyone with the pallet enforcing "one open plan
   per parent". But note the finding: a small model needs `[plan-needed:]`
   *delivered to it*, and whoever receives that directive is the leader in
   practice. Decide deliberately rather than porting the role out of habit.
6. **What replaces WAYWARD?** "I can't reach a node / I'm not synced" is the
   same condition. Keep the answer verbatim: **fail fast with a clear message,
   never hang the turn.**
7. **Does `miot-llm` wrap `genai` or define its own trait?** genai normalizes
   14 providers including Ollama, with tool calls, streaming and reasoning
   controls — which is most of `miot-llm`'s job. A thin local trait over it
   keeps the seam for a local `llama-server` path.

---

## 8. Summary

- The litter is already record + deterministic application + peer layer. The
  mapping to FRAME is a translation of *semantics*; the code is new.
- The findings table (§1.1) is the asset. Everything else is replaceable.
- `ensure_signed(origin)` is the litter's biggest unbuilt feature, for free.
- Bodies off chain, hashes on chain. `miot-bodies` behind a trait.
- Three `no_std` crates, forced by wasm. Everything else is std + tokio.
- One agent binary, cross-compiled to `aarch64-unknown-linux-musl`, runs on
  both Akuma and Linux. No `libakuma`, no `rt.rs`, no per-target clone flags.
- Node on Linux. A node inside Akuma is Phase 7, not a dependency.
