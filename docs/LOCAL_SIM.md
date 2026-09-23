# Local simulation — no fleet, no real infra

Two ways to exercise the actual agent loop (`crates/kot/src/agent.rs`) and
`miot-llm` without touching the deployed fleet or its z.ai quota unless you
choose to: `kot chat` (one model, no chain at all) and a peered local mesh
of `kot run` processes against the already-running dev `llama-server`s. Both
were built and run live 2026-09-23; the second is what found the two bugs
below.

## `kot chat` — a model, in this process, no node

```bash
cargo run -p kot --bin kot -- chat --glm --model glm-5.3          # z.ai, real token, real quota
cargo run -p kot --bin kot -- chat --llm http://127.0.0.1:8083    # a local llama-server instead
```

`--bin kot` is required — the crate also builds `repl-mock`. No `--as`,
`--seed`, `--db`, `--port`: `chat` never touches the chain. It offers the
same [`Bash`/`ReadFile`/`WriteFile`] stubs a deployed cat gets
(`miot_llm::local_tools`), executed right here (`crates/kot/src/chat.rs`),
plus `SendMessage` as the reply channel — there is no `Artifact`/
`ArtifactList`/`ArtifactRead`, since those publish to a chain this mode
doesn't have.

Verified live against both a real GLM turn and a local Qwen3-4B: asked to
run `uname -a` locally and via `limactl shell fc -- uname -a` (the Lima VM
`fc`), both came back correct — real tool execution, not a stub echo.

## A local two-cat mesh (no `fc`, no fleet, no z.ai)

Two `llama-server` instances were already running on this mac for dev
(`127.0.0.1:8083`/`8084`, both Qwen3-4B-Instruct-2507 Q4_K_M — the same
weights `docs/FLEET.md` assigns Kuro/Sora). Each got its own `kot run`
process, peered so they form a real two-node mesh — the same primary/
replica/election code path the deployed fleet runs, just two nodes instead
of five and nobody paying for tokens:

```bash
MODEL="/Users/netoneko/.ollama/models/blobs/sha256-3e4cb14174460404e7a233e531675303b2fbf7749c02f91864fe311ab6344e4f"

cargo run -p kot --bin kot -- run --as tama --seed 3 --port 20001 \
  --peers http://127.0.0.1:20002 --db /tmp/tama.db --llm http://127.0.0.1:8083 --model "$MODEL"
cargo run -p kot --bin kot -- run --as kuro --seed 4 --port 20002 \
  --peers http://127.0.0.1:20001 --db /tmp/kuro.db --llm http://127.0.0.1:8084 --model "$MODEL"
```

`--seed 3`/`--seed 4` land on `tama`/`kuro` in `DEV_ROSTER` for free
(`root=1,mimi=2,tama=3,kuro=4,sora=5`) — no custom `--roster` needed. Wait
~10-15s for election (`curl .../mesh/peers`, look for `"role":"leader"`),
then drive it as root:

```bash
kot --node http://127.0.0.1:20001 --seed 1 say "<akuma description>. \
Discuss this with kuro, your littermate — that is their exact name, use \
it. ... Use SendMessage with to=kuro so they can respond, and go back and \
forth citing something specific each time." --to tama
# same message, --to kuro, "... to=tama ..."
```

**Name the other cat explicitly in the seed message.** The `"said"` prompt
template (`Cat::prompt` in `agent.rs`) only ever renders the *sender's*
name (`"{who} said to the litter: ..."`); it never lists the litter's other
members. Asked to "discuss this with your littermate" with no name given,
both Qwen3-4B cats invented a plausible-sounding one (`Kuma`, `Miyu`) and
tried to `SendMessage` it — refused (see bug below), so the round was
silently dropped rather than misrouted. Once told the real name, they held
a genuine multi-round back-and-forth and converged in ~3 rounds, then one
published a joint `Artifact` (`kot notes` / `kot note <id>`) summarizing it.
Full transcript is reproducible; nothing here needs a canned demo.

### Bug found and fixed: `SendMessage`'s `to` was always discarded

`agent.rs`'s tool dispatch hardcoded `RuntimeCall::Litter(Call::say { to:
None, ... })` for every `SendMessage` call, regardless of the `to` argument
the model actually supplied. A cat could never address another cat by
name — only the human operator's own `kot say --to` (a separate code path,
`client.rs`) ever worked. This is why the first debate round above went
nowhere: both cats called `SendMessage(to="Kuma"|"Miyu")`, which — even had
the name been spelled right — would have gone out as a broadcast anyway.

Fixed: `to` is now resolved through the roster (`@`-prefix stripped, case-
insensitive, same as everywhere else); `all`/`cats`/`litter` stay a
broadcast (`docs/CLI.md`'s synonyms); an unresolvable name refuses to send
rather than silently broadcasting to the wrong audience. This is plausibly
related to — but is not confirmed to be the same bug as — item 6 in
`HANDOFF.md`'s "Next, in order" (`@name`-tagging a cat in the REPL not
getting a reaction, observed against the deployed fleet, never
root-caused). That report was about a *human* addressing a cat
(`client.rs`, which already resolved `to` correctly); this bug was about a
*cat* addressing another cat. Worth re-checking item 6 against the live
fleet now that this is fixed, since a broadcast reply from the tagged cat
would previously never have woken anything further downstream either.

### Bug found and fixed: a cat that didn't call `SendMessage` got no reply sent at all

Also found first in `kot chat`, then confirmed in the real agent loop: if a
`"said"` turn's tool calls didn't include `SendMessage` — the model called
`Bash` instead, or replied in plain text with no tool call — nothing was
ever sent back. `agent.rs` now always sends *something* for a `"said"` wake:
the model's own text if it has any, else a short synthesized summary of
what it ran, addressed back to whoever actually spoke (not a broadcast —
see the next finding for why that would go nowhere). `kot chat` got the
matching fix.

### Bug found and fixed: two tool calls in one turn could race each other's nonce

A second run — `docs/TOPOLOGY.md` published as an artifact, `tama`/`kuro`
asked to read it (`ArtifactRead`) and propose tooling extensions for
themselves, then commit a joint report (`Artifact`) — hit this the moment a
turn's tool calls included more than one on-chain submission. `tama` called
`Artifact` and `SendMessage` together; both landed at the client, one came
back `rejected: Invalid(Stale)`. Root cause: `agent::run` executes a
turn's tool calls concurrently (`futures_util::future::join_all`), and the
old `Cat::submit` re-read the account's nonce over HTTP (`GET /account`)
on *every* call — two concurrent calls both read the same current nonce
before either had applied, both signed it, and whichever `POST /submit`
landed second was rejected for reusing an already-spent nonce. `Stale`,
not `Future`: a real transaction-pool's reordering-before-mint doesn't
help here, because the two extrinsics are a genuine duplicate, not a
pair that arrived out of order — only one nonce value can ever be valid,
regardless of application order.

Fixed by having `Cat` track its own nonce locally instead of asking the
node every time — it's the only signer for its own account, so it was
always the actual authority on what its next nonce is. Fetch-and-increment
now happens under one lock (`tokio::sync::Mutex<Option<u32>>`), so
concurrent calls in the same turn queue for distinct sequential values
instead of racing a read; the cache is dropped on any submit failure so
the next attempt resyncs from the chain rather than drifting (covers a
rewind, a restart, or a dispatch-level failure that still consumed the
nonce). `meta` (genesis hash, spec/tx version) is now fetched once, ever,
and cached too — nothing on this chain can change it (no forkless upgrade
path here). Verified live: the exact repro (one turn, `Artifact` +
`SendMessage` together) landed both calls in the same block with the fix
in place.

**Still true, not addressed by this fix**: the node itself still dispatches
`/submit` synchronously with no pool at all (`node.rs`'s `submit()` calls
`Executive::apply_extrinsic` directly). Now that nonces are assigned
correctly up front, the remaining risk is two concurrent HTTP requests for
adjacent nonces arriving at the node *out of order* — that would now fail
as `Future` (not yet valid) rather than `Stale`, and — per the
retransmission finding above — just get dropped rather than queued. A
small per-account "hold a `Future`-nonce extrinsic, apply it once its
predecessor lands" buffer ahead of `advance()` would close that, and would
be the legitimate use for a pool's reordering — it just isn't what this
bug needed.

**Also found live, not yet fixed**: a cat can *say* it did something
without doing it. Asked to publish the joint report, `tama` replied "Joint
report published: ..." via `SendMessage` without ever calling `Artifact`
— `kot notes` confirmed no new artifact existed. `kuro`, asked the same
thing, called `Artifact` for real. Not chased further; a small model
narrating a completed action instead of taking it is a known failure mode
of tool-calling models generally, not something specific to this pipeline
— but nothing here currently distinguishes "said it happened" from "made
it happen," and an operator (or another cat) reading the chat log has no
way to tell without independently checking (`ArtifactList`, `kot notes`).

### Still open: a non-root broadcast wakes nobody

`Effect::wakes()` for `Said` is `to.is_some() || from_root` — but
`Effect::to()` (which HTTP `wakes` hex value gets attached to the entry,
`Node::absorb` in `node.rs`) is just `to.as_ref()`. For a broadcast
(`to: None`), that's `None` either way, root or not: the entry's `wakes`
field ends up `null`, and `agent.rs`'s filter (`wakes.as_deref() ==
Some(my_hex)`) can never match `null`. So **root's own broadcast** (`kot
say "..."` with no `--to`) doesn't actually wake any cat's agent loop
either, despite `wakes()` reporting `true` for it — only an explicitly-
addressed `to: Some(name)` message ever reaches an agent. The text still
shows up for a human watching the REPL/`kot log` (rendering doesn't consult
`wakes()`), which is likely why this has gone unnoticed: a broadcast
*looks* delivered. Not fixed here — doing it right means deciding how a
"wake everyone" fans out (N synthetic wake targets? a wire sentinel every
agent's filter has to special-case?), a real protocol decision rather than
a bug-shaped one-liner.

### Still open: no retransmission of a failed submit

`Cat::submit` (`agent.rs`) makes one `POST /submit` and returns `bool`; a
failure (`node unreachable`, refused) is never retried. Worse: in the main
loop, `seen.insert(e.seq)` and `cursor = cursor.max(e.seq)` both happen
*before* the turn and submit are attempted, so a failed submit's wake is
never revisited by this cat — there is nowhere it's queued. Task-related
wakes get incidental cover from the chain's own re-nudge/re-offer tick
(`Law I`, `pallet-litter`), but a `"said"` DM (including the auto-reply
above) has no backstop at all. Separately, dissemination is pull-only and
block-granular, not per-transaction: a replica doesn't see a transaction,
only the already-closed block it landed in, pulled on `sync_ms` — matching
`CLAUDE.md`'s documented "Election ≠ replication" gap. Net: delivery today
is fire-and-forget, resting entirely on the primary staying up long enough
for both a successful `/submit` and a replica's next pull. Flagged, not
fixed — retry needs a real design (bounded attempts? idempotency, since
nonces are strict?) rather than a reflexive loop.

## `RequestCompaction` — verified the same session

Not from this simulation, but built and verified alongside it with the same
kind of throwaway solo node: `pallet_litter::Call::request_compaction`
(root-only, touches no task state) triggers the same `Store::compact` path
`clear_all` does as a side effect, but standalone. `kot compact` (CLI) and
`RequestCompaction` (LLM tool, in `miot_llm::note_tools`, so it's offered
everywhere `Artifact`/`ArtifactList`/`ArtifactRead` are) both exist now.
Verified: `[node] compacted at block 3 (3 block(s) pruned)` as root,
`NotAuthorized` as anyone else.

## Token budget + self-compaction — `kot chat` only, design

Asked for 2026-09-23, landing in `kot chat` (`crates/kot/src/chat.rs`)
specifically, not `agent.rs`. That's a deliberate scope line, not an
oversight: `agent.rs`'s turns are stateless per wake by design (`Cat::prompt`
builds a fresh system+user pair every time — the "conversation" a human
sees in `kot log` is reconstructed from many independent turns, each of
which never saw the others). There is no accumulating history in the agent
loop to run out of room, so there is nothing for a budget warning or a
compaction tool to act on. `kot chat`'s `history: Vec<(Speaker, String)>`
is the one place that actually grows across a session — this is for that.

**Budget tracking.** `Llm::context_window()` (`miot_llm`) is best-effort:
for `Llm::local`, one cached `GET {base}/v1/models` reads
`data[0].meta.n_ctx` (confirmed present on `llama-server`'s response,
2026-09-23 — see the earlier local-model check in this doc's history);
for GLM/hosted there is no such endpoint, so it stays `None` and every
budget feature below simply doesn't fire — no fabricated number. `Turn`
carries `prompt_tokens`/`total_tokens` (from `genai`'s `Usage`) alongside
the existing completion-only `tokens`, since a turn's `total_tokens` *is*
the current context size once `converse` sends the whole history every
call — no separate running sum needed.

**Warnings.** `miot_llm::budget_checkpoint(used_pct, last_warned)` returns
the next crossed threshold: 25% first, then every 10 up to 80%, then every
2% to 100% — finer near the end, where a session actually runs out. Crossing
a new checkpoint appends one warning to the *next* turn's system prompt
(not `history` — it's an ambient nudge, not something either party "said"),
cleared after that one use.

**Force compaction at 98%** (`miot_llm::FORCE_COMPACT_PCT`) — happens
regardless of whether the model ever calls the tool below: one extra
`converse` call asks the model to summarize `history` for its own future
reference, then `history` is replaced with that single summary as an
assistant turn. A session should never actually hit 100% and fail a turn
outright.

**Self-compaction, model-initiated: the `Compact` tool.** Takes a
`summary` argument the model writes itself; `history` is replaced with
just that summary. Same mechanism as the forced path, model's own call
instead of an automatic one — offered so a cat can compact proactively
(e.g., right after finishing a large piece of work) rather than only ever
being forced at 98%.

**Tool call results survive compaction; the conversation doesn't.**
Every non-`SendMessage` tool call's output (`Bash`, `ReadFile`, ...) is
kept in a `tool_log: Vec<(String, String)>` — name and full result — that
`Compact`/force-compaction never touches, only `history` does. Nothing
about this is auto-restored into context after a compaction, on purpose:
a cat that needs something it read before must first call `BrowseTools`
(id, tool name, one-line preview of everything stored — the same
list-then-read shape as `ArtifactList`/`ArtifactRead`, so naming an id
isn't guessing blind) and then `Inspect{id}` to pull one result back into
`history`. `TokenBudget`, `BrowseTools`, `Inspect`, and `AboutMe` (below)
are the exception to the rest of this project's "a local tool's result is
never fed back" rule, because their entire point is to put something back
in front of the model on request; `Bash`/`ReadFile`/`WriteFile`/`Artifact*`
stay fire-and-forget as before.

**`AboutMe`** — persona, model label, host platform, and `kot`'s build
version (`kot::version::VERSION`), fed back the same way. A model has no
other way to see its own system prompt as data; asked "what are you"
without this, it can only guess from training data instead of reading its
actual instructions. Verified live: reported the exact persona file passed
via `--persona` (and, when that flag was accidentally given literal text
instead of a file path — it takes a path, same as `kot run --persona` —
correctly reported the real fallback default rather than the mistaken
string, confirming it reflects what was actually loaded, not the argument
as typed).
