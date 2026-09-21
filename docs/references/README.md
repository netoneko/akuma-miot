# `docs/references/` — one reference per subsystem

**Index and rationale. The per-subsystem files are not written yet.**

`MAPPING_REPORT.md` is the *design of record* — why this exists and what it
maps onto. These are the *references* — how one subsystem actually behaves,
for someone working inside it. They are split per subsystem for one specific
reason, recorded below.

---

## Why split: there are three event loops, and they do not share a clock

This is the thing that must not get muddled, and it is why one document cannot
serve all of it. The three loops differ by **four orders of magnitude** in
their time constants:

| Loop | Lives in | Cadence | Deterministic | May block on |
|---|---|---|---|---|
| **Chain tick** | `pallet-litter::on_initialize` | one block, ~6 s, fixed | **yes — must be** | *nothing, ever* |
| **Agent turn** | `miot-agent` | one LLM turn, **120–200 s**, unbounded | no | nothing (it accumulates instead) |
| **CLI** | `miot-cli` interactive | one keystroke, ~16 ms | no | nothing |

**The agent loop is the loose one**, and it is loose by two separate
mechanisms, not one:

- a turn takes minutes and may take longer with no upper bound;
- tools are dispatched and **not awaited**, so results re-enter the inbox
  whenever they land — this turn, or three turns later — and an
  aggregation policy decides when enough has accumulated to be worth
  assembling.

A turn therefore spans **20–34 chain ticks**. The chain will expire a lease,
requeue work and nudge a holder several times over inside a single turn, and
that is correct behaviour rather than a race to be fixed.

### The rule that falls out

> **No loop may ever await another.** They meet only through queues and
> streams. A loop that waits on a slower one has adopted the slower one's
> latency, and a loop that waits on a faster one has made the faster one
> late.

This is not theoretical. Both of meow's deadlocks were exactly this mistake:

- The leader polled **its own inbox over loopback** while being the only
  thread that could answer it — a loop waiting on itself.
- `serve::drain` held `PMutex<HubState>` across a deadline-bounded read; the
  deadline poll hook called `local_drain`; `local_drain` re-locked a
  non-reentrant mutex. Both threads parked in `FUTEX_WAIT` on the same word at
  the same PC. From outside it looked like three unrelated faults.

Each reference below therefore has to state, in its own words: **its cadence,
what it is allowed to wait on, and what it hands to the adjacent loop.** That
is the part a single combined document reliably blurs.

---

## Planned files

| File | Subsystem | The loop it documents |
|---|---|---|
| `chain.md` | `pallet-litter`, `miot-runtime`, `miot-node` | the tick: leases, nags, directives, GC. Law I. |
| `agent.md` | `miot-agent` | the turn: inbox, aggregation policy, in-flight futures, the waking rule. Law II. |
| `cli.md` | `miot-cli` | the composer: input, printing, scrollback. Supplements `../CLI.md`, which is requirements rather than behaviour. |
| `tasks.md` | `miot-tasks` | **no loop** — a pure state machine. Documents the lifecycle and every finding it encodes. |
| `tools.md` | `miot-tools` | dispatch-don't-await, local-vs-public, the aggregation policies. |
| `llm.md` | `miot-llm` | providers, streaming, what a turn costs. |
| `bodies.md` | `miot-bodies` | the agent-local store. Why it is never networked. |
| `chain-client.md` | `miot-chain` | subxt: submission, event streaming, reconnection, the disconnected-fail-fast rule. |

`tasks.md` is deliberately in the list despite having no loop: it is the one
subsystem every other one talks to, and "this has no loop and no clock" is
itself the fact worth stating loudly.

---

## When

After Phase 4, when there is an agent and a node to describe truthfully.
Writing them earlier would document intentions rather than behaviour, and the
whole point of a reference is that it is checkable against the code.

Until then: `MAPPING_REPORT.md` §2 for the design, `CLI.md` for the CLI
contract, and `miot-tasks`'s own module docs plus its 31 tests for the
lifecycle.
