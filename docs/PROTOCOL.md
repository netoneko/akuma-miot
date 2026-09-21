# The Akuma Miot protocol

Canonical reference. `HANDOFF.md` is state/roadmap, `docs/MAPPING_REPORT.md`
is the design history and open questions, `docs/CLI.md` is the client's own
contract — this is the vocabulary and rules those all assume you already
know. If a doc and this one disagree, the source (`crates/miot-primitives`,
`crates/miot-tasks`, `crates/pallet-litter`) is the tiebreaker and this file
is wrong.

## The three layers, and why they're not the same thing

A recurring confusion: is a task "a transaction," "a row of state," or "a log
entry"? All three exist and are deliberately different:

| Layer | What it is | Lives where | Example |
|---|---|---|---|
| **Transactions** (extrinsics) | The verbs. Signed calls that *mutate* state. | The wire / a signed `UncheckedExtrinsic` | `Litter::update(task: t1.2, act: Done, text: "...")` |
| **State** | The nouns. One `Task<A>` row per task, mutated in place by transactions. | `pallet_litter::Litter`, one `StorageValue<State<AccountId>>` | `t1.2`'s `status` field flips `Pending → InProgress → AwaitingClearance` over its life |
| **Effects** (events) | A derived, append-only *log* of what happened, for observability and for waking agents. | `miot-node`'s in-memory ring (`/events`), not consensus state | `Effect::Directed{to: mimi, task: t1, directive: PlanNeeded}` |

A task is **state**, not a transaction and not a log entry. `t1` failing
means a transaction (`clear_all`, or the block tick exhausting
`DirectiveNag`) set `t1.status = Failed` in the one `State` blob, *and* — as
a side effect of that same transaction — appended an `Effect::Failed{task:
t1}` row to the event log. The log is a *projection*, not the source of
truth; replaying it is how a fresh reader (a restarted node, `--rpc --chat`'s
start-up replay) catches up, not how the live node itself decides anything.

**One further wrinkle, at the persistence layer specifically**
(`miot-store`, being wired into `miot-node` as of 2026-09-22 — see
`HANDOFF.md` item 2): on disk, only raw transactions are kept, plus a full
`State` snapshot at each compaction point — nothing in between. So
*reconstructing* state after a restart or a fork rewind genuinely does mean
replaying transactions (through `Executive::apply_extrinsic`, block by
block, from the last snapshot forward) — an event-sourced model at the
storage layer, even though the live in-memory runtime is ordinary mutable
state, not re-derived per read. Both descriptions are correct; they're about
different layers.

## Vocabulary (`crates/miot-primitives/src/lib.rs`)

**`TaskId { parent: u32, sub: u16 }`** — `sub == 0` *is* the parent (not a
sentinel), so `t1` and `t1.0` are the same id. Displays as `t1` / `t1.2`.

**`TaskStatus`** — parents walk `Open → Planned → Closed`, with `Failed` as
the other terminal (no artifact ever produced). Sub-tasks walk `Pending →
InProgress → AwaitingClearance → Cleared`, with `Act::Reopen` sending one
back to `Pending`.

**`Act`** (what `Litter::update` takes) — `Claim`, `Done`, `Failed`,
`Clear`, `Reopen`, `Artifact`. One dispatchable, one enum, deliberately: "a
small model picks a *value* more reliably than it picks among near-identical
tool names."

**`Authority`** — `Root` (operator, outranks leader, **never a worker**),
`Leader`, `Peer`. Decided by `ensure_signed`/`ensure_root` off the *recovered
signer*, never off a field the caller filled in.

**`Directive`** (what the table tells the *leader* to do next) —
`PlanNeeded`, `ClearanceNeeded`, `ArtifactNeeded`, `ReassignNeeded`,
`LeaderElected`. Re-sent on `DirectiveNag`'s interval until acted on or the
budget (`MaxDirectiveNudges`) runs out.

**`Requeue`** (why a sub-task went back to `Pending`) — `Unclaimed`,
`Rehomed`, `LeaseExpired`, `Reopened`.

**`Effect<A>`** — the full event vocabulary, and the *only* place waking is
decided (`Effect::wakes()`), so no consumer re-derives it:

| Effect | Waking? | Addressed to |
|---|---|---|
| `Assigned` | always | the assignee |
| `Directed` | always | the leader |
| `Nudge` | always | the current holder |
| `Said` | `to.is_some() \|\| from_root` | `to`, or everyone if root spoke untagged |
| `Opened`, `Planned`, `Record`, `Requeued`, `NudgeBudgetSpent`, `Closed`, `Failed`, `Rehomed` | never | broadcast |

The rule in one sentence: **a record is not an instruction, targeted traffic
and root's own words are.** Waking four cats per broadcast turns one remark
into four LLM turns — measured, not assumed (`docs/MAPPING_REPORT.md` §1.1).

## Dispatchables (`crates/pallet-litter/src/lib.rs`)

| Call | Authority | Effect(s) |
|---|---|---|
| `open(text)` | Root or Leader | `Opened`, then a `Directed{PlanNeeded}` once due |
| `plan(parent, assignments)` | Leader | `Planned` + one `Assigned` per sub-task, atomically |
| `update(task, act, text)` | the holder (worker acts) / Leader (`clear`/`reopen`) | `Record`, plus whatever the resulting status transition triggers |
| `reassign(task, to)` | Leader | `Rehomed` + a fresh `Assigned` to the new holder |
| `say(to, body)` | anyone signed | `Said` |
| `set_leader(who)` | Root | `Directed{LeaderElected}` to the new leader |
| `set_root(who)` | governance/sudo (`ensure_root`) | none — silent storage write, "the key to the cat house" |
| `clear_all()` | Root | `Failed` for every currently `Open`/`Planned` parent, **plus an immediate `gc(now, keep_for: 0)`** (2026-09-22 — see "GC" below) |

## Timers and limits (`crates/miot-runtime/src/lib.rs`)

**The values in `miot-primitives`' doc comments (e.g. "`claim_window` ...
600s") are generic defaults, not what's running.** `miot-runtime`'s
`parameter_types!` are tuned against measured LLM turn lengths and are the
ones that matter. At `BLOCK_MS = 6000` (`miot-node`):

| Constant | Blocks | Real time |
|---|---|---|
| `ClaimWindow` | 60 | 6 min |
| `Lease` | 150 | 15 min |
| `WorkNag` | 30 | 3 min |
| `DirectiveNag` | 30 | 3 min |
| `MaxNudges` | 3 | — |
| `MaxReoffers` | 3 | — |
| `MaxDirectiveNudges` | 3 | — |
| `GcKeepFor` | 14,400 | 24h |

`MaxText=4096, MaxResult=16KiB, MaxArtifact=64KiB, MaxTitle=128,
MaxSubtasks=8, MaxTasks=512, MaxMessage=2048` — byte/row caps, not timers.

`HANDOFF.md`'s "Traps" section already flags these as "~50× too
conservative" for the turn lengths actually observed (3-60s, not 120-200s) —
this table is the reference values that finding is about, not a
contradiction of it.

## GC (`TaskTable::gc`, `crates/miot-tasks/src/lib.rs`)

A pure in-memory prune, not an effect-producing act — a GC sweep never
appears in `/events`, only as `/tasks` shrinking. `gc(now, keep_for)`: find
every **parent** row with status `Closed`/`Failed` whose `now - closed_at >=
keep_for`, then one `Vec::retain` drops those parent rows *and* every one of
their sub-task rows (a sub-task's `TaskId.parent` matches, so one retain
catches both). **Artifacts are never dropped** — they're the durable output;
only the bookkeeping around them is disposable.

Two call sites: `on_initialize` runs it every block with `keep_for =
GcKeepFor` (24h — deliberately not instant, "the only kind of compaction
that is consensus business"). `clear_all` (2026-09-22) also calls
`gc(now, 0)` inline, sweeping *every* already-dead parent immediately, not
just the ones it just failed — `/clear` is an operator's "move on," so
nothing dead is worth 24h of grace.

## Tool exposure per effect type (`crates/miot-cat/src/main.rs`, `crates/miot-llm`)

An agent's tool surface depends on **why it was woken**, not on being a
fixed persona-wide capability list:

| Woken by | Tools offered | Why |
|---|---|---|
| `Said` (chat) | `SendMessage` only (`chat_tools()`) | A reply to chat is not license to act on tasks |
| `Assigned`, `Directed`, `Nudge` (task work) | `TaskUpdate`, `TaskPlan`, `TaskReassign` (`task_tools()`) | The full task-lifecycle surface |

This means an agent asked "what tools do you have?" mid-chat will honestly
answer "just `SendMessage`" — confirmed live, 2026-09-22 (kuro answered
exactly this) — and that's correct behavior, not a bug or a lie: it reflects
the actual `tools` array passed to that specific turn.

## Addressing (`@name`, client-side only)

`@name` is never on the wire — `say`'s `to` field is `Option<AccountId>`.
Resolution happens client-side against a `name=seed` roster
(`crates/miot/src/rpc.rs::resolve`), *before* signing. `@all`/`@cats`/
`@litter` are explicit-broadcast synonyms for `to: None`. Multiple `@name`
tags in one line become multiple `say` extrinsics (one per addressee, same
body, in tag order) rather than a schema change — see `docs/CLI.md` §8 for
why, and its cost (the same body appears once per addressee in the chain
log, not once with a recipient list).

## What this doc does not cover

Open design questions (wayward, participant notification, tagging-on-chain)
live in `docs/MAPPING_REPORT.md` §7 ("Ours, not Polkadot's") — this file is
what's built and running, not what's proposed. Keep it that way: update this
file when behavior changes, file new questions there, don't let the two
drift into saying the same thing two different ways.
