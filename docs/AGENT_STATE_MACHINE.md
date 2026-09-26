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
- **One lane** (2026-09-26): every tool that touches this host's
  filesystem — `Bash`, `ReadFile`, `WriteFile`, `Edit`, `MultiEdit`, `LS`,
  `Glob`, `Grep` — runs one at a time, in the order it was called, across
  turns too. `Edit`/`MultiEdit`/`LS`/`Glob`/`Grep` (`crates/miot-llm/src/
  edit_tool.rs`, `fs_tools.rs`) were added later the same day, shaped after
  Claude Code's own tool set — GLM and Qwen are both trained as drop-ins
  for it — with `docs/TOOLING.md` carrying the fuller reasoning and the
  transcript numbers behind it. They go through the same lane as the
  original three: each one reads or writes a file the same as `ReadFile`/
  `WriteFile` do. Each call takes
  its place in a fair mutex when it's dispatched, not when its task first
  runs. Before this they all ran at once. meow's transcript for the evening
  of 2026-09-25 shows 12 overlaps, including a `sed -i` on `hda.rs` that
  started while the previous turn's edit-and-build script was still
  rewriting that file, and a note's `WriteFile` racing the `reboot` beside
  it. meow explained the shredded files as "duplicate-spawned" calls, but
  nothing was duplicated. A queued call shows in `Running` and the
  still-running list as "queued behind the calls before it". Its timeout
  and stall clock start only when it runs, and `Cancel` removes it from
  the queue. The cost is that a long build holds up every file call after
  it, and the rules tell the model so. Chain and session tools (`Peers`,
  `Running`, `Cancel`, `Inspect`, …) are not in the lane.
- **Aged** (2026-09-25): a fed row stays in the conversation for
  `RESULT_TURNS` (6) turns. After that it's replaced in place by a one-line
  stub: its first line (the call and how it went), its length, and
  `Inspect {"id": N}`. Rows under 400 characters are left alone. Before this,
  every result ever fed stayed in history for good. meow's GLM loop had no
  context window to compact against (`Llm::context_window` only asks a
  llama-server), so it re-sent about 75k tokens a turn, and 181 KB of its
  235 KB history was old tool output. A hosted model now gets its window
  from `--context-window`/`MIOT_CONTEXT_WINDOW` (`deploy.py` sets 1M for
  the GLM cats).
- **Ids carry on across a restart.** The history file keeps the next id and
  the rows not yet aged. An id from before the restart answers `Inspect`
  with "not kept": the tool log itself isn't persisted. A history file in
  the old format (a bare array) is loaded with every results block cut to
  one line per row. meow's went from 235 KB to 76 KB.
- **`LocalTask`** answers a change with the change and the open ids
  (`L1 is done. 1 open: L2.`), not the whole list. `list` still shows
  everything, and a mistake still comes back with the list.
- **`∑ tok`** (the on-chain stats) counts `total − cached`, where the
  provider reports a prompt cache (`prompt_tokens_details.cached_tokens`,
  now also in the transcript's `cached_tokens`). A provider that reports none
  is counted the old way, so `∑` is the sum of every turn's whole context.

## Watching it work (2026-09-25)

Asked for after meow forgot a job on restart and a GLM turn was a minute of
silence with nothing to look at. Nothing here is on chain.

**What the operator sees.**

- **Reasoning.** `miot_llm::Turn::reasoning`: GLM/OpenRouter send it apart
  (`reasoning_content`); qwen3 on llama-server sends `<think>…</think>`
  inline, which is cut out of the reply (`normalize_reasoning_content`).
  Shown as a `✎ reasoning` block on stdout (the herd log / journald), head
  and tail if long. Never fed back into history.
- **Text beside tool calls**, which used to go unshown, and a `◌ … started`
  row when each call leaves (its `⚙` result row still comes when it lands).
- **Transcript.** `~/.akuma/kot/<name>.transcript.jsonl`, next to the
  session file: `start` (system prompt, model), then per `turn` the prompt
  fed, reasoning, text, calls, tokens; every `result` in full (as kept);
  every `record`; `reset`, `compact`, `held`, `dropped`. Appends across
  restarts; moved to `.1` past 64 MB.
- **Activity** (`crate::activity`). One live record per cat: phase
  (`thinking` / `waiting` on tools / `idle` / `compacting`) and for how
  long, what woke it, every call in flight with its `r`-id and output so
  far, ✓/✗ tallies and the last six results, open local tasks, the tail of
  the last reasoning. The cat POSTs it to its own node (`POST /activity`,
  signed, accepted only from the node's own key) on every change and every
  5 s; each node carries its cat's record on the `/mesh/status` exchange it
  already does, both directions, flattened into the status JSON so an older
  node just ignores it. `GET /activity` on any node serves every cat it can
  hear, push-only ones included. The REPL shows one row above the hairline
  (`活 meow ◌ 42s · kuro ⚙2 Bash 1m03s · tama · ✗Bash`), redrawn every
  second; `/activity [name]` and `kot activity [name]` print the full
  snapshot into scrollback. The record also carries the cat's local task
  list (every open one plus the newest finished, 24 at most, with done/total
  over the whole list): the row shows it as `3/4`, and `/tasks <cat>` (or
  `kot task list --cat <cat>`) prints it with each finished step's note.
  Bare `/tasks` is still the litter's tasks on chain.

**What the model sees.**

- **Calls in flight.** `Bash` streams stdout/stderr into a live buffer.
  `Running` lists the model's own queries in flight (id like `r3`, elapsed,
  bytes, time since last output, latest output); `Running r3` shows more of
  one. `Cancel r3` drops it (a `Bash` child is killed) and its result comes
  back marked `cancelled` with what it printed. A timed-out `Bash` keeps its
  output too. Chain writes are never listed: the model was told writes just
  happen, and listing one waiting to be tallied had GLM debating whether to
  resend it.
- **"Still running"** heads every turn taken while queries are out.
- **Stall notice.** A query silent for `Host::stall_after` (2 min) gets the
  model one turn: `[r5 Bash still running] … no output at all in 2m01s. It
  may be stalled: Running r5 …, Cancel r5 …. Or leave it`. Once per
  silence (new output re-arms it); a follow-up turn, so past
  `MAX_FOLLOWUPS` it's dropped, not held.
- **Local tasks** (`crate::local_tasks`, the `LocalTask` tool): the cat's own
  to-do list, ids `L1`…, at `~/.akuma/kot/<name>.tasks.json` keyed by epoch
  like the session file, so it survives a process restart and is emptied
  when the checkpoint moves. Open ones ride every wake's prompt, which is
  what lets a restarted cat pick up where it was (its conversation is still
  in memory only).

**Run live, 2026-09-25** (`docs/LOCAL_SIM.md`: sima on local Qwen3-4B,
simb on GLM-5.3): simb split a four-step job into L1–L4, ran the quick
step, started `sleep 200` (silent), got the stall notice at 2m01s, called
`Running r5` then `Cancel r5` (cancelled at 147.4 s), marked its tasks done
and reported. Restarted with an open L5, it answered "what's still open?"
correctly from the reminder alone. Two bugs came out of that run and are
fixed: a GLM cat never showed up in `/activity` (its loop published before
the POST task subscribed to the watch channel), and an idle cat's record
read as stale (no heartbeat).

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
- ~~A model that ignores the check-in.~~ **Covered for the case that
  actually bit meow (2026-09-25):** an ignored check-in with an open
  `LocalTask` now gets nudged — the same idea as a chain task's `nudge`
  (`Timers::work_nag`, "you claimed this, do it now."), just for the to-do
  list nothing on chain knows about. `AgentStateMachine::maybe_nag`, run off
  the same watchdog tick that catches a stalled call: idle
  ([`LOCAL_TASK_NAG_AFTER`], 150 s, mirrors `work_nag`'s default) with
  `Host::reminder` returning something queues a real wake (resets the
  follow-up budget, unlike a stall notice), bounded to
  [`MAX_LOCAL_TASK_NUDGES`] (3, mirrors `max_nudges`) and reset the moment a
  turn starts a query again. A cat with nothing on its local list, or a host
  with `check_before_idle` off (`kot chat`), is left alone. **Still not
  covered:** a model that ignores the check-in with *no* open local task —
  a bare "I'll get to it" with nothing written down has nothing to nudge it
  back with.
- ~~A nudge answered with a promise, not a tool call, burns the budget the
  same as true silence.~~ **Covered (2026-09-26):** found live, meow — three
  nudges in a row each got "Firing it now, nya:" and no tool call, and the
  budget (3) ran out right as an operator message happened to arrive and
  save it. The next nudge now quotes the unfulfilled one back
  (`last_unfulfilled_promise`), so it can't just repeat itself — the budget
  itself is unchanged, on purpose: a model that only ever promises needs
  the same backstop as one that never answers at all, or nagging it burns
  turns forever exactly the way the bound was meant to prevent.

## Also seen in the same log, not fixed here

- **meow's node ran out of memory on the Akuma box.** `memory allocation
  of 9728 bytes failed` while replaying about 9.3k blocks. It restarted
  several times, and a message root sent at 15:06 was answered at 17:20.
  HANDOFF, "Open theory".
- **meow misread the runbook.** It concluded the amd64 self-host runbook
  was for "a specific HP box (192.168.1.123), which isn't this machine". It
  is that machine; its DHCP lease moved to `.120`.
