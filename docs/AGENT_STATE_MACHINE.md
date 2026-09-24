# The agent state machine

`crates/kot/src/agent_state_machine.rs` is the one agent loop. A cat under
`kot run` (`agent.rs`) and `kot chat` (`chat.rs`) both host it. They differ in
three things only: where input comes from (the chain's `/events` or stdin),
which extra tools they offer, and how they show things. HANDOFF, "The agent
state machine", has the history. This file shows how the loop works now and
what changed on 2026-09-24 after meow's kernel build stalled.

Tests: `crates/kot/tests/agent_state_machine.rs` runs the real loop against a
scripted fake model server, 30 tests.

## Vocabulary

- **Wake**: something worth a turn. A chain event rendered into a prompt, or
  an operator's line. Resets the follow-up count.
- **Query**: a tool whose output comes back (`Bash`, `ReadFile`, `Peers`,
  `Inspect`, ...). It is spawned, not awaited, and its result lands in the
  inbox labelled `[#id Tool]`.
- **Record**: a write (`SendMessage`, `TaskUpdate`, `Artifact`, ...). Spawned
  and shown, never fed back.
- **Follow-up**: a turn driven only by results, with no new wake. At most
  `MAX_FOLLOWUPS` (8) in a row.
- **Session**: bumped by every `Reset`. Anything from an older session (a
  turn still thinking, a query still running, a wake queued ahead of the
  reset) is dropped.

## The diagram

```
                     wake                       result (query finished)
          (chain event / operator line)                   │
                       │                                  │
                       └──────────────┬───────────────────┘
                                      ▼
┌──────────────┐  anything  ┌──────────────────────────────────┐
│     IDLE     │───────────►│            AGGREGATE             │
│ no query or  │            │ fold in all queued wakes/results │
│ record in    │            │ results only: wait for the batch │
│ flight       │            │ (every query back, or 10 s)      │
└──────────────┘            └────────────────┬─────────────────┘
       ▲                         wake in it? │ results only?
       │               ┌─────────────────────┴──────────────────┐
       │               ▼                                        ▼
       │     followups := 0                         followups < 8 ? ──no──► HELD
       │     held results go in                          │ yes             queued, not dropped;
       │     first (oldest)                              │ followups += 1   fed with the next wake
       │               │                                 │ (8th says: "last
       │               │                                 │  one, report now")
       │               └───────────────┬─────────────────┘
       │                               ▼
       │                      ┌─────────────────┐
       │                      │    THINKING     │  llm.converse(system, history, tools)
       │                      └────────┬────────┘
       │                               │  a Reset arrived meanwhile? → drop every call, IDLE
       │                               ▼
       │                      ┌─────────────────┐
       │                      │     ACTING      │  queries → spawned, result → inbox
       │                      │                 │  records → spawned, shown, never fed
       │                      │                 │  no call at all → Host::spoke
       │                      └────────┬────────┘
       │                               │
       │     fed results, made calls, all of them records?
       │     (and the host wants it: a cat yes, kot chat no)
       │                 no ┌──────────┴──────────┐ yes → armed
       │                    │                     ▼
       │                    │       once no query is in flight and nothing
       │                    │       is queued, and followups < 8:
       │                    │                ┌──────────────┐
       │                    │                │   CHECK-IN   │  followups += 1
       │                    │                │ "nothing is  │  fed CHECK_IN alone
       │                    │                │  running..." │
       │                    │                └──────┬───────┘
       │                    │     calls nothing     │     calls a tool
       │                    │   (erased from        │   → ACTING, as any turn
       │                    │    history) ──┐       │     (a check-in never
       │                    │               │       │      arms another)
       └────────────────────┴───────────────┘       ▼

  Reset (checkpoint moved, /clear), at any point:
    history, held results, check-in and queued wakes cleared; session += 1;
    turns and query results from the old session are dropped when they land.
```

A turn is never cancelled by new input. A wake that arrives mid-turn waits
for the next turn and is folded in there. Closing the inbox (`kot chat` at
EOF) drains every query and record still in flight, then returns.

## Tool output

- **Fed**: up to `FEED_CHARS` (3000) characters. A longer result is fed as its
  first 1000 and last 2000, with a marker naming the exact
  `Inspect {"id": N, "offset": 1000}` that reads on. Build errors are at the
  end of a log, and a README's point is at its start.
- **Kept**: the tool log holds each result in full up to 256 K characters,
  then a quarter from the start and the rest from the end. It survives
  `Compact`.
- **`Inspect`** returns one page of 2700 characters from `offset`, plus
  "for the next part" when there's more. A page fits under `FEED_CHARS`, so
  it's never cut a second time. Before this change, `Inspect` returned the
  whole result, which was then cut to the first 3000 characters again. The
  "Inspect for the rest" hint pointed at something that could not show the
  rest.
- **`Bash`** waits 30 s unless given `timeout` (seconds, at most 3600). Its
  result comes back whenever it finishes; other results and wakes are
  handled meanwhile. On timeout the shell is killed (`kill_on_drop`) and the
  model is told it can ask for longer. A child the shell forked may outlive
  it.

## Why it changed: meow's kernel build, 2026-09-24

Root asked meow (GLM on the bare-metal Akuma box) to compile the akuma
kernel from its runbook. Read from `/var/log/herd/kot.log` on the box and
from the chain:

1. Five follow-up turns of looking around: `ls`, the runbooks, `uname -m`,
   the self-host runbook, `rust-toolchain`, `ls amd64/`, then
   `cat amd64/README.md`. The fifth result hit the old cap of 4. It was
   *held back* and then silently dropped, since the batch was discarded.
   The model never saw it.
2. Root asked "how is it going" (a wake). meow read `amd64/README.md` again,
   43 KB, and was fed only its first 3000 characters.
3. meow sent "Going preem… next I'm checking whether a plain host-side
   `cargo build`… works. Will report back soon" and called no tool. Nothing
   was in flight and nothing woke it, so the loop went idle for good. No
   build ran, and no more reports came.
4. Had it run `cargo build`, the fixed 30 s `Bash` timeout would have
   killed it.

Each of these maps to a change above: held results are kept, the cap is 8
and announces its last turn, the check-in catches (3), `Bash` takes a
`timeout`, and results are fed head and tail with `Inspect` paging.

What the check-in costs: one extra model turn at the end of each chain of
tool work that ends in a write, on a cat. Chatter costs nothing extra (a
reply to a wake with no tool results in it doesn't arm), and neither does a
turn that starts more work. `kot chat` has it off
(`Host::check_before_idle`).

What it doesn't cover:

- A reply to a wake that promises work but calls no query ("on it!"). No
  results are involved, so nothing arms. The rules now say plainly that a
  message alone doesn't start anything.
- A model that ignores the check-in.

## Also seen in the same log, not fixed here

- **meow's node ran out of memory on the Akuma box.** `memory allocation
  of 9728 bytes failed` while replaying about 9.3k blocks. It restarted
  several times, and a message root sent at 15:06 was answered at 17:20.
  HANDOFF, "Open theory".
- **meow misread the runbook.** It concluded the amd64 self-host runbook
  was for "a specific HP box (192.168.1.123), which isn't this machine". It
  is that machine; its DHCP lease moved to `.120`.
