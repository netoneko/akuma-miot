# Akuma Miot

```
                      =#=      .-
                      +*#*:.:-**
                      +%%#%##***
                      +%%%#%%#**.
                      +%@@@%%%+*:
          :::::--=+++*%@%@%%%%*-
     :-+##%%%%%%%%@%@#%%@%%%%##%+
  .=##%%%%%%%%%@@@@@%#%%@%%%@@@%%-
.*%%%%%%%@%%%%@%@@@@%%%%@%%%%%@@#-
%@%%%%%%%%%%@%%%%%@@@%%%@@@@@%%%#+
*%%%%%@%@@%@@@%%%%#%@@@@@@@%%@@%@@@*+--
 ::=+*#@@@@@@@@@@@%%%%%%@%%@#----=**@%@#
         .--+**%@@@@@%@@%%@@%*       :-.
                  ::::---#@%%*
```

*Miot Kotów* — Polish for **a litter of kittens**. **Akuma** (悪魔, "demon") is
the signature cat, and the bare-metal kernel this grew out of.

A litter of LLM agents that coordinate through a blockchain instead of through
a socket. Task state, results and final artifacts live on chain; the agents are
ordinary clients that sign extrinsics. Nothing waits on anything.

> **Status: a mesh.** 108 tests. Signed extrinsics, a persisted block log,
> and an elected primary across real machines (`HANDOFF.md`,
> `docs/TOPOLOGY_TARGET.md`). One binary, `kot`: a node + agent loop
> (`kot run`), or a client of any node. See [`docs/RESULTS.md`](docs/RESULTS.md)
> for what actually ran.

```
cargo test --workspace                          # host-native, no docker
cargo run -p kot -- run --as solo --db /tmp/solo.db \
  --llm http://127.0.0.1:8081                   # a mesh of one, with a cat
cargo run -p kot -- --node http://127.0.0.1:9944 task open "the question"
cargo run -p kot -- --node http://127.0.0.1:9944          # the REPL

overlays/local/llama-swarm.sh up                # llama-servers, one per cat
overlays/local/build.sh all                     # static musl kot for aarch64 + x86_64
overlays/deploy/deploy.sh up all                # the real mesh
```

---

## Where it came from

The predecessor is `akuma/userspace/meow` — a `no_std` agent that ran a
"litter" of cats over a hand-rolled hub socket with Raft leader election. Its
own documentation described it as *"a hand-rolled Tendermint with futures
bolted onto it"*, and its open problem was written down plainly:

> What is wanted is **address recovery**, not a name lookup: derive the
> sender's identity *from the signature over the payload* […] a forged `from`
> is not a policy failure, it is a signature that does not recover to anyone in
> the roster. And the reason this matters beyond tidiness: **root is the key to
> the cat house.**

That paragraph is `ensure_signed(origin)?`. Akuma Miot is that observation
taken seriously.

**None of meow's code is imported.** What carries across is the *behaviour* —
a dozen findings that were only learnable by running agents and watching them
fail, each one now a named test in `crates/miot-tasks/src/tests.rs`.

---

## The system

**The chain is the only channel between agents.** No agent reads another's
local store, ever — not even when they share a host.

```
╔══════════════════════ AGENTS — std + tokio, one binary ══════════════════════╗
║   akuma guest             linux host              linux host                 ║
║   (aarch64-musl)                                                             ║
║  ┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐           ║
║  │  miot-cli run   │    │  miot-cli run   │    │  miot-cli task  │ operator  ║
║  │      "tama"     │    │      "kuro"     │    │      "root"     │ one-shot  ║
║  │ ┌─────────────┐ │    │ ┌─────────────┐ │    │                 │           ║
║  │ │ miot-bodies │ │    │ │ miot-bodies │ │    │  no agent loop  │           ║
║  │ │   LOCAL     │ │    │ │   LOCAL     │ │    │  behind it —    │           ║
║  │ │ working set │ │    │ │ working set │ │    │  root is not a  │           ║
║  │ │ tool output │ │    │ │ tool output │ │    │  worker         │           ║
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
                 ║  │  │  │  the pure machine    │  │  │  ║
                 ║  │  │  └──────────────────────┘  │  │  ║
                 ║  │  │  ids · status · leases     │  │  │  ║
                 ║  │  │  nags · messages           │  │  │  ║
                 ║  │  │  RESULTS · ARTIFACTS       │  │  │  ║
                 ║  │  └────────────────────────────┘  │  ║
                 ║  └──────────────────────────────────┘  ║
                 ╚════════════════════════════════════════╝

                  ── or, before the chain exists ──
                 ╔════════════════════════════════════════╗
                 ║  miot-coord — same miot-tasks, one     ║
                 ║  tokio task, no chain, no wasm         ║
                 ╚════════════════════════════════════════╝
```

### Two laws

> **I. The chain never waits.** The state machine is bounded and eternal. It
> ticks on block cadence forever, whether or not any agent answers. Every
> deadline is a block height. An agent that goes silent costs the chain
> nothing — its lease expires and the work requeues.
>
> **II. The agent never blocks.** Not on the chain, not on a tool, not on the
> model. Chain events, tool results and body fetches are all just inbound
> traffic. Submits are fire-and-forget.

An LLM turn is minutes. A block is seconds. Nothing may be built that assumes
the first finishes inside the second.

---

## The agent loop

Not event-driven request/response. A long-lived task with an inbox, a set of
in-flight futures, and a policy that decides when enough has accumulated to be
worth a turn.

```
┌───────────────────────────── miot-agent ──────────────────────────────────┐
│                                                                           │
│  INBOUND — never blocks, accumulates during a turn                        │
│                                                                           │
│   chain events  ──┐                                                       │
│   tool results  ──┼──► inbox ──► ┌──────────────┐                         │
│   local reads   ──┤              │  AGGREGATOR  │  policy decides WHEN    │
│   operator msgs ──┘              │              │  a turn is worth it     │
│                                  └──────┬───────┘                         │
│                                         │ assemble                        │
│                                         ▼                                 │
│                                  ┌──────────────┐                         │
│                                  │ turn builder │  role · changed ·       │
│                                  │              │  results · working set  │
│                                  └──────┬───────┘                         │
│                                         ▼                                 │
│                                  ┌──────────────┐   minutes. the inbox    │
│                                  │   miot-llm   │   keeps filling.        │
│                                  └──────┬───────┘                         │
│                                         │ tool calls                      │
│                        ┌────────────────┴────────────────┐                │
│                        ▼                                 ▼                │
│              LOCAL (a query)                    PUBLIC (a record)         │
│              fs · shell · search                TaskUpdate · TaskPlan     │
│                        │                                 │                │
│                 dispatch, don't await            signed extrinsic         │
│                        ▼                          fire-and-forget         │
│              ┌──────────────────┐                        │                │
│              │ in-flight futures│                        └──► confirmed   │
│              └────────┬─────────┘                             later, as   │
│                       └── results ──► inbox                   an event    │
│                                       (this turn, or three later)         │
│                                                                           │
│  no shared state · no locks · nothing held across an .await               │
└───────────────────────────────────────────────────────────────────────────┘
```

A tool call does not end a turn, and a turn does not await a tool.
`trait Aggregate` is per-agent policy — `Immediate`, `Barrier(tool)`,
`Quorum`, `Deadline{n,t}`, `Coalesce` — with two rules it may not break: an
**assignment or directive always assembles a turn**, and a **broadcast record
never does on its own** (waking four agents per record turns one task into
sixteen LLM turns).

---

## One task's life

```
 root            pallet-litter                  tama(leader)         kuro
  │                    │                              │                │
  ├─ open t1 ─────────►│ t1 Open                      │                │
  │                    ├─ [plan-needed: t1] ─────────►│ wake            │
  │                    │◄─ TaskPlan(t1, [tama, kuro]) ┤                │
  │                    │ t1.1 Pending→tama                             │
  │                    │ t1.2 Pending→kuro                             │
  │                    ├─ Assigned(t1.1) ────────────►│ wake            │
  │                    ├─ Assigned(t1.2) ──────────────────────────────►│ wake
  │                    │◄─ claim t1.1 ────────────────┤                │
  │                    │ InProgress, lease = now+150 blocks            │
  │                    ├─ "proceed" nudge ───────────►│  budget 3,     │
  │                    │                              │  resets on any │
  │                    │◄─ done t1.1 ─────────────────┤  act by holder │
  │                    │ AwaitingClearance                             │
  │                    │◄─ claim t1.2 ─────────────────────────────────┤
  │                    │◄─ done t1.2 ──────────────────────────────────┤
  │                    ├─ [clearance-needed: t1] ────►│ wake            │
  │                    │◄─ clear t1.1, clear t1.2 ────┤                │
  │                    ├─ [artifact-needed: t1] ─────►│ wake            │
  │                    │◄─ artifact t1 (markdown) ────┤                │
  │                    │ t1 Closed                                     │
  │◄─ event: Closed(t1, "Does this codebase work?") ───────────────────│
```

The bracketed directives are not chat. The table *names the exact verb* and
delivers it, because a 0.8B model will not infer `[artifact: t1]` from a design
document.

**When a cat cannot.** If nobody claims, the offer is re-made a bounded number
of times and then the table raises `[reassign-needed: t1.1]`; if a cat reports
`failed`, the leader sees it at clearance. Either way the leader calls
`reassign(t1.1, kuro)` and the work changes hands with fresh budgets and no
inherited wrong answer. Without that, a sub-task is bound to its cat for life
and the parent can never close — see `docs/MAPPING_REPORT.md` §1.2.

---

## The split: agent vs CLI

`miot-agent` is a **library**. `miot-cli` is the **binary**, and it has two
jobs that must not be confused.

```
                      ┌──────────────────────────────────────┐
                      │            miot-cli                  │
                      └──────────────┬───────────────────────┘
                 ┌───────────────────┴────────────────────┐
                 ▼                                        ▼
    ┌─────────────────────────┐            ┌──────────────────────────┐
    │  RUN MODE — a cat       │            │  OPERATOR MODE — hands   │
    │  `miot run --as tama`   │            │  `miot task open "…"`    │
    │                         │            │  `miot artifact t42`     │
    │  long-lived. hosts the  │            │  `miot peers` `miot log` │
    │  miot-agent loop, wires │            │                          │
    │  in concrete impls:     │            │  one-shot. signs one     │
    │   Chain  → miot-chain   │            │  extrinsic or reads      │
    │   Llm    → miot-llm     │            │  state, prints, exits.   │
    │   Tools  → miot-tools   │            │                          │
    │   Store  → miot-bodies  │            │  NO agent loop. This is  │
    │                         │            │  `root`, and root is not │
    │  never renders for a    │            │  a worker.               │
    │  human; emits events    │            │                          │
    └─────────────────────────┘            └──────────────────────────┘
```

**Why the library/binary line is where it is:** `miot-agent` owns no sockets,
no clock and no globals. It is `async fn`s over injected traits, so the whole
loop — aggregation policy, turn assembly, the waking rule, the nudge budget —
is testable on the host against fakes, with no chain and no model. Everything
that actually touches the world lives in `miot-cli` and the four impl crates.

This is the rule the whole workspace is built on, and it is the one defect that
made meow's architecture unusable: **meow cannot `cargo test` natively at
all** — raw Linux syscalls throughout, `no_std` end to end — which is why its
three testable pieces had to be carved into sibling crates and the rest hides
behind an in-binary suite. Anything here that cannot be tested on the host is
in the wrong crate.

**Why operator mode has no loop:** `root` is in the roster so it can open
tasks, but there is no agent behind it. The litter found this the only way it
could — a leader canvassing the litter split its task four ways and gave the
fourth to the operator; that sub-task could never be claimed, and since an
artifact requires every sub-task cleared, the parent could never close. Here it
is a compile-time fact rather than a convention: operator mode links no agent.

---

## Crates

| Crate | `no_std` | Status | Does |
|---|:--:|---|---|
| `miot-primitives` | ✅ | **built** | `TaskId`, `TaskStatus`, `Act`, `Effect`, `Limits`, `Timers`. Zero dependencies. |
| `miot-tasks` | ✅ | **built** | The lifecycle as a pure state machine. No clock, no I/O. |
| `pallet-litter` | ✅ | **built** | Thin FRAME wrapper: `ensure_signed`, read, apply, write, emit. |
| `miot-runtime` | ✅ | **built** | `construct_runtime!`, 90 lines. Executed **natively** — no wasm. |
| `miot-node` | ✗ | planned | `sc-*` node, AURA + GRANDPA. |
| `miot-coord` | ✗ | planned | The same state machine in one tokio task, no chain. |
| `miot-chain` | ✗ | planned | `subxt`: submit, stream events. |
| `miot-store` | ✗ | **built** | The block log on ParityDB. Compaction-boundary rewind, leader-wins. |
| `miot-keys` | ✗ | **built** | ed25519 identity. An account **is** a public key; the sender is recovered, never claimed. |
| `miot-bodies` | ✗ | planned | Agent-**local** store, on Turso. Tool call results and transcripts, queryable. Never networked. |
| `miot-llm` | ✗ | **built** | Provider layer on `genai` — 15 providers incl. GLM (`zai`), one OpenAI-compatible path for ollama and `llama-server` alike. |
| `miot-tools` | ✗ | planned | Async tool registry + aggregator. |
| `miot-agent` | ✗ | planned | The loop. A library, no I/O of its own. |
| `miot-cli` | ✗ | planned | The binary. Both modes above. |

Three crates are `no_std`, and only because FRAME and wasm require it. That is
the entire boundary.

---

## What goes on chain

| On chain | Agent-local |
|---|---|
| task ids, status, assignee, lease, nag budget | the conversation / working set |
| messages between agents | in-flight tool futures |
| sub-task results (`MaxResultLen` 16 KiB) | raw tool output, build logs, file contents |
| **artifacts** — the final markdown report (64 KiB) | the aggregation buffer |
| roster + identity (accounts) | |

Bounded results are a **forcing function**: an agent that cannot publish a
40 KB build log has to say what happened instead of pasting what scrolled by.

The invariant: **losing an agent loses its working set and nothing else.** The
chain still knows the sub-task was outstanding, the lease still expires, the
work still requeues, and every result already submitted is still there.

---

## Running it locally

`overlays/local/` — Stage 1 is plain Linux in one Lima VM: three validators,
one `llama-server`, N agents, all ordinary processes. Stage 2 moves the agents,
and only the agents, into Firecracker/Akuma guests. **Akuma is not on the
critical path**; every failure drill works at Stage 1.

---

## Docs

- `docs/MAPPING_REPORT.md` — the design of record: the findings, the mapping,
  `std`/`no_std`, the roadmap, and what was deliberately not rebuilt.
- `docs/RESULTS.md` — what has actually run, with numbers. Evidence, not
  intentions.
- `docs/references/storage.md` — two stores, not one: files on the chain path,
  Turso for the agent-local tool-call history. With measured binary costs.
- `docs/CLI.md` — `miot-cli` requirements. Scrollback is sacred; it is not a
  full-screen TUI.
- `docs/references/` — one reference per subsystem, once there is behaviour to
  describe. Its index records the load-bearing constraint: **three event loops,
  four orders of magnitude apart, and none of them may await another.**
- `overlays/local/README.md` — the local testbed topology.
