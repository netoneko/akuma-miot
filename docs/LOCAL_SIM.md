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
