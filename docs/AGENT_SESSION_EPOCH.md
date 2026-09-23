# A local agent session, tied to the current epoch — handoff, 2026-09-23

**Status: IMPLEMENTED, 2026-09-23.** `docs/PROTOCOL.md`'s "Local session,
tied to the current epoch" section is the write-up of what actually got
built (`Session` in `crates/kot/src/agent.rs`) and the answers this doc's
open questions got. Left below verbatim as the trace that led there — still
useful context for *why* those answers were picked, but read `PROTOCOL.md`
first for what's actually running.

Written for the next agent after a
conversation that traced how the agent loop's "memory" actually works today
and found it has none beyond one in-process `String`. Kirill's ask, verbatim:
*"we need a local session that is associated with the current epoch."* This
doc is the trace (code-grounded, file:line) plus the design space — not a
prescribed implementation.

## How it works today (verified against the code, not assumed)

**Every LLM turn is a fresh, isolated 2-message call.** `Llm::turn`
(`crates/miot-llm/src/lib.rs:124-142`) builds exactly
`ChatRequest::new(vec![ChatMessage::system(persona), ChatMessage::user(prompt)])`
and nothing else — no accumulated messages array, no prior turns included.
`Cat::prompt`'s own doc comment says why: "a turn is stateless, so the chain
is the only memory" (`crates/kot/src/agent.rs:135-136`).

**What stands in for memory** is two things, both cheap and both fragile:

1. **`question: String`** — one plain local variable in `Cat::run`'s loop
   (`crates/kot/src/agent.rs:360`). Set once from whichever fires first: a
   root broadcast (`said` with `root: true`, only if `question` is still
   empty) or a task `opened` event's text (unconditional overwrite,
   `agent.rs:381-390`). Threaded into every prompt as "The litter is working
   on: {question}".
2. **The chain itself** — `ClearanceNeeded`'s prompt branch, for example,
   re-fetches `/tasks` live and rebuilds its list from scratch every call
   (`agent.rs:177-209`). Nothing about task state is cached turn to turn.

**`cursor: u64`** and **`seen: HashSet<u64>`** (`agent.rs:359,361`) are the
other two pieces of loop state — the `/events` polling cursor and a
dedup set so a coalesced batch doesn't re-fire an already-acted-on `seq`.

**All three (`question`, `cursor`, `seen`) are plain Rust locals. None are
persisted.** A process restart zeroes them: `cursor` restarts at 0 and
replays `/events?since=0` (the whole log the node currently holds — see the
"node's log restarted" guard right above it, `agent.rs:364-376`, which is a
*different* case: the node's own `seq` resetting under the cat, not the
cat's own restart). `question` restarts empty and is only reconstructed if
the replay happens to still contain an `opened`/root-`said` event — which it
usually does, but that's incidental, not designed.

There is no LLM-context "compaction" to speak of, because there is no
accumulating context to compact — each call is capped at persona + one fresh
prompt + tool schemas, by construction, forever.

## The gap

Nothing here survives a reboot on purpose, and nothing here is scoped to
"the litter's current run" versus "the litter three rewinds ago." A cat that
restarts mid-task has to re-derive everything from a full-log replay, which
mostly works by luck (the `opened` event is still in the log) rather than by
design, and has no way to tell *which* epoch of chain history the state it
just rebuilt actually belongs to.

## What already exists to build this on

**"Epoch" is not a new word here — `docs/references/storage.md`'s "Conflict
resolution: the leader wins" section already uses it** for the span between
two compactions, and ties it explicitly to recovery: *"Rewind lands on a
compaction, or on genesis — never on an arbitrary height"*; *"the exposure
is bounded... only blocks above the last compaction can be discarded, so
the window is one epoch of churn rather than all of history."* That doc also
already draws the boundary Kirill's ask needs: **"What a cat did — its tool
calls, their output, what it learned — is in its own local store, and no
rewind touches that. What goes is the on-chain claim of having done it."**
So a local session is explicitly meant to *outlive* a rewind within the
protocol's own design — it just doesn't exist yet.

**The current epoch already has a number, and it's already on the wire.**
`Store::last_checkpoint()` (`crates/miot-store/src/lib.rs:149-150`) is the
compaction height; `node.rs` serves it as `"last_checkpoint"` in
`/mesh/peers` (seen live this session: `"last_checkpoint": 2404`). Whatever
"current epoch" ends up meaning for a session key, this field is the
existing, zero-new-chain-work candidate for it.

**A local agent store was already decided, and never built.**
`docs/references/storage.md`'s decision #2: *"Agent-local store
(`miot-bodies`): Turso. Tool call results, turn records, prompts and
transcripts... per-cat and never replicated."* Measured cost: +12.8 MB
stripped for Turso, format-compatible with `sqlite3` (verified against a
real file, not assumed). **`miot-bodies` is not a workspace member** — check
`Cargo.toml`'s `members` list — so this was a design decision that never
got a crate. Whether the session Kirill's asking for belongs inside a
revived `miot-bodies`, or is small enough to not need Turso at all (a single
JSON file per cat, keyed by epoch?), is an open question below, not settled
by this doc having found the reference.

## Open questions for whoever picks this up

Not a spec — the trade-offs a `kot run` restart actually has to resolve:

1. **What's *in* the session?** At minimum `question` and `cursor` look
   like session state; `seen` might not need to survive a restart at all
   (re-seeing an already-resolved wake is typically a cheap no-op turn, per
   `storage.md`'s "one cheap turn, not a redo"). Does the session also want
   to remember *why* the cat is doing what it's doing beyond `question` —
   e.g. its own last few tool calls, matching the "transcripts" `miot-bodies`
   was scoped for?
2. **What keys a session to an epoch, precisely?** `last_checkpoint` is the
   obvious candidate, but it changes over a session's own lifetime (the
   chain keeps compacting while the cat runs) — does the session need to
   *follow* the current checkpoint forward, or does it belong to whichever
   checkpoint was current when the session started, with a rule for what
   happens when that checkpoint ages out?
3. **What does it mean to open a new epoch?** A `clear_all` ("a new session,
   same chain," per its own doc comment, `main.rs:71`) obviously should — but does a routine
   compaction advancing `last_checkpoint` also start a new session, or only
   a rewind (a fork, a demotion)? Those are different events today
   (`Store::compact` vs `Store::rewind_for_fork`/`adopt_checkpoint`,
   `miot-store/src/lib.rs`) and may deserve different session treatment.
4. **Where does it live?** A file per cat (`~/.akuma/kot/<name>/session.json`,
   next to the seed file convention `crates/kot/src/common.rs` already
   uses), or inside a revived `miot-bodies`/Turso, or something smaller.
   The storage doc's own reasoning ("megabytes, and growing every turn," "a
   query engine") was scoped for tool-call transcripts, which is a much
   bigger problem than one `question` string and a cursor — reconsider
   whether that reasoning still applies to *this* ask specifically, rather
   than assuming session storage and transcript storage are the same
   problem.
5. **Read-vs-derive.** Today, restarting and replaying `/events?since=0`
   *works*, just by accident. Any session design should be judged against
   that baseline: what does persisting the session actually save (fewer
   events re-scanned? correctness when a checkpoint has rolled the `opened`
   event out of the log entirely — a real failure mode replay-from-0 doesn't
   handle, since compaction discards blocks below `last_checkpoint`)?

## Where to read first

- `crates/kot/src/agent.rs` — `Cat::run` (the loop, `question`/`cursor`/
  `seen`), `Cat::prompt` (what "stateless" means turn to turn).
- `crates/miot-llm/src/lib.rs:124-142` — `Llm::turn`, confirms no message
  history is ever sent.
- `docs/references/storage.md` — "epoch" already defined; the `miot-bodies`
  decision that never got built; the rewind/compaction recovery boundary.
- `crates/miot-store/src/lib.rs` — `Store::last_checkpoint`,
  `Store::compact`, `Store::rewind_for_fork`, `Store::adopt_checkpoint`: the
  actual epoch mechanics a session would have to key off.
- `crates/kot/src/common.rs` — the existing per-cat local-file convention
  (`load_or_create_identity`, seed files) if a session ends up being a file
  rather than a database.
- `crates/kot/src/main.rs:71` — "a new session, same chain" is already
  `clear_all`'s own doc comment; check it's still accurate before this doc's
  answer to question 3 gets acted on.
