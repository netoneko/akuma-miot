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
| **Transactions** (extrinsics) | The verbs. Signed calls that *decide* what happened. | The wire / a signed `UncheckedExtrinsic` | `Litter::update(task: t1.2, act: Done, text: "...")` |
| **Effects** (events) | The **one and only description of a state change**. Everything below is folded from these. | `TaskTable::apply`'s input; `miot`'s in-memory ring (`/events`) for observability | `Effect::Directed{to: mimi, task: t1, directive: PlanNeeded}` |
| **State** | The nouns. One `Task<A>` row per task — a *materialized view*, never mutated except by folding an effect through `apply`. | `pallet_litter::Litter`, one `StorageValue<State<AccountId>>` | `t1.2`'s `status` field flips `Pending → InProgress → AwaitingClearance` over its life |

A task is **state**, and state is **a fold over effects** — genuinely, as of
2026-09-22 (`TaskTable::apply`, `crates/miot-tasks/src/lib.rs`), not just at
the persistence layer. Every verb (`open`, `plan`, `claim`, `clear_all`,
`tick`, ...) first *decides* what effects should happen — pure, no
mutation — then calls `self.apply(effect, now)` once per effect, and
`apply` is the **only** place `tasks`/`artifacts`/`leader`/`next_parent`
are ever written. There is no second, parallel "what does dispatch do to
storage" code path to drift from: live application and replaying a
persisted effect log call the exact same function.

This is why `Opened`, `Record`, and `Closed` carry more than the minimum an
agent's prompt needs (`text`, the result body, the artifact `body`+`author`)
— `apply` has to reconstruct a `Task`/`Artifact` from *only* the effect, so
whatever isn't already derivable from current state (a free-text input the
operator or an agent typed) has to ride along on the effect itself. See
`replaying_the_effect_log_reproduces_live_state_exactly`
(`crates/miot-tasks/src/tests.rs`) — it runs a full lifecycle live, replays
only the resulting effect log into an untouched table, and asserts the two
are field-for-field identical. That test is the actual guarantee; this
section is just the explanation.

**One known, deliberate approximation**: a sub-task's `opened_by` (`/tasks`'
display field only, never read for authorization) is stamped as the
*assignee* when `apply(Assigned)` creates the row fresh, because the
`Assigned` effect that creates it doesn't carry who called `plan`. Cheap to
add a field later if that field ever needs to be exact; not worth it today
for a value nothing but a listing reads.

**Persistence** (`miot-store` → `miot`, HANDOFF item 2, done
2026-09-22): the effect log described above *is* what gets persisted
(`Node::persist`, one `Store::append` per block) and replayed on start
(`Node::replay`, folding through `pallet_litter::Pallet::replay_effect` →
`TaskTable::apply`) — `apply` was built to make that trustworthy rather than
merely plausible, and it's now load-bearing, not just tested in isolation.

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
ones that matter. At `BLOCK_MS = 6000` (`miot`):

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

## Tool exposure per effect type (`crates/kot/src/main.rs`, `crates/miot-llm`)

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
