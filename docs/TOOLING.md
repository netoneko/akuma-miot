# Tooling — `Edit`/`MultiEdit`/`Grep`/`Glob`/`LS`, and what the data said

Grew out of a session (2026-09-26) that started as "check the way meow edits
files" and turned into pulling meow's real transcript
(`~/.akuma/kot/dumpster-akuma-amd64.transcript.jsonl`, 8 MB, 96 process
starts, 814 model turns — fetched over ssh in <1 MB `dd` chunks, since a
single `cat` stalls the exec channel at exactly 1,048,576 bytes, see
`CLAUDE.md`'s traps) and actually measuring how it uses its tools, how
restarts and prompt caching interact, and how much time reboots cost. Every
number below is from that transcript, not estimated.

## How meow actually edits files, before this

Of 552 `Bash` calls, 298 (54%) were themselves file-write operations: 259
`echo >`/`>>`, 66 `sed -i`, 45 heredocs, 42 `cat >`, 37 `printf >`. Real
`WriteFile` tool calls: **11** (9 ok, 2 failed). `ReadFile`: 136 (3 failed).
`Bash` overall: 461 landed results, 6.7% failed. So almost every edit was
done by spelling the file's content out inside a shell command instead of
using the dedicated tool — more tokens per edit (quoting, escaping, `sed`
regex syntax) and more fragile than a single targeted call.

## Is there a standard tool shape for this? Not one spec — real convergence

No single official cross-vendor spec exists (OpenAI's `apply_patch`,
Anthropic's `str_replace_based_edit_tool`, Google's Gemini-CLI `read_file`/
`write_file` are all different), but there's real *de facto* convergence for
the model families this litter actually runs:

- **GLM** (meow, tama): Zhipu markets GLM as a drop-in for Claude Code
  ("point `ANTHROPIC_BASE_URL` at z.ai, use it inside Claude Code"), so its
  agentic-coding SFT data is almost certainly saturated with Claude-Code-
  shaped tool traces — separate `Read`/`Edit`/`Write`/`Bash` tools, not
  Anthropic's own bundled multi-command tool.
- **Qwen** (a future fleet member): same story once removed — Alibaba's own
  "Qwen Code" is another Claude-Code-shaped clone.
- **Gemma** (kuro): general-purpose, not coding-agent-tuned, no strong prior
  either way — doesn't argue *against* the same shape.

So: not "support each separately." Two of the three model families this
litter runs already expect Claude-Code-shaped tools, and the third is
indifferent.

## `Edit`, then `MultiEdit`/`Grep`/`Glob`/`LS`

`crates/miot-llm/src/edit_tool.rs` defines `Edit`'s schema (name,
description, JSON schema only); `crates/miot-llm/src/fs_tools.rs` defines
`MultiEdit`/`Grep`/`Glob`/`LS`, added right after in the same session once
the follow-up question ("what about file inspection tools?") landed:
meow's transcript has 127 of 552 `Bash` calls as plain read-only navigation
(`ls`/`find`/`grep`/...) — the read-side twin of the write-side gap `Edit`
closed. Dispatch for all five is kot's own, in `crates/kot/src/
agent_state_machine.rs`'s `local_tool` match arm next to `Bash`/`ReadFile`/
`WriteFile`. Each is deliberately in its own file (not mixed into
`miot-llm`'s other, fully-original tool definitions), with a header
explaining where the *shape* comes from.

Two different footings, both noted where they apply:

- `Edit`'s shape traces to Anthropic's *officially published* API tool
  (`str_replace_based_edit_tool`) — its `str_replace` command's "must match
  the file exactly once" rule is kept verbatim, since that's the retry
  behavior GLM/Qwen have specifically seen — reconciled with Claude Code's
  own separate-tool convention and `old_string`/`new_string`/`replace_all`
  field names.
- `MultiEdit`/`Grep`/`Glob`/`LS` aren't from a published API at all —
  they're Claude Code's own product tool set, observed and documented by
  the wider community rather than specified by Anthropic for third-party
  reuse, and by now widely copied by other coding agents (the same
  convergence discussed above). `MultiEdit` batches several `Edit`-shaped
  edits into one all-or-nothing write; `Grep` mirrors real Grep's
  `output_mode` (`content`/`files_with_matches`/`count`) and `-i`/`-n`/
  `-A`/`-B`/`-C`/`head_limit`, backed by the system `grep`, not a
  hand-rolled matcher — `multiline` and `type` (ripgrep-specific) aren't
  reproduced; `Glob` finds files by name pattern via `find`, sorted most-
  recently-modified first; `LS` lists one directory non-recursively, with
  an `ignore` list.

Both footings are the same kind of provenance note, not a license
requirement either way: a tool's name and JSON parameter names are
functional API surface — closer to a method signature than to creative
expression — and nothing here copies Anthropic or Claude Code source.

One deliberate split kept from the real thing, for maximum compatibility
with what GLM/Qwen have actually seen: a tool naming one specific file
(`Edit`, `MultiEdit`) takes `file_path`; a tool naming a directory or
search scope (`Grep`, `Glob`, `LS`) takes `path` — matching kot's own
pre-existing `ReadFile`/`WriteFile` convention. `Edit`/`MultiEdit`'s
exact-match-once rule: `old_string` must match exactly once unless
`replace_all` is true, else it fails saying whether the match was missing
or ambiguous (and how many times), so the model narrows and retries rather
than guesses. All eight file-touching tools (`Bash`, `ReadFile`,
`WriteFile`, `Edit`, `MultiEdit`, `LS`, `Glob`, `Grep`) share the one-lane
mutex (`docs/AGENT_STATE_MACHINE.md`, "One lane") — each one reads or
writes the filesystem the same as the original three did, so all of them
queue the same way.

Tests: `crates/kot/tests/agent_state_machine.rs` —
`edit_replaces_one_exact_match`, `edit_refuses_when_old_string_is_not_found`,
`edit_refuses_an_ambiguous_match_unless_replace_all`, `multi_edit_applies_
every_edit_as_one_write`, `multi_edit_writes_nothing_if_any_edit_fails`,
`ls_lists_a_directory_marking_dirs_and_honouring_ignore`,
`glob_finds_by_name_pattern_recursively`,
`grep_content_mode_shows_matching_lines_with_numbers`,
`grep_with_no_matches_is_still_ok` (grep's exit 1 is a successful empty
search, not a tool failure), and `bash_and_file_calls_run_one_at_a_time_in_
order` now covers `Edit` too.

## Restarts, aging, and the prompt cache

Real numbers from the same transcript: **96 process starts**, ~20 h of
total downtime across 95 gaps (median 133 s, one 5.9 h outlier), 6 sessions
that did zero turns before rebooting again. 87 of the 552 `Bash` calls
contain the word `reboot` — meow is testing kernel builds by rebooting the
actual box it runs on; this is the kernel-build workflow, not a crash loop.

Prompt-cache hit ratio (`cached_tokens`/`total_tokens`): mean 0.15 on the
first turn after a restart vs 0.36 on turn 3+ — cache is colder right after
a restart, as expected. But the *median* is 0 even in steady state — half
of all turns get no cache credit at all. `age()` (`docs/
AGENT_STATE_MACHINE.md`, "Aged") mutates an already-sent history message in
place every `RESULT_TURNS` (6) turns, which breaks any provider's prefix
cache from that point on — happening constantly, given 552+ tool calls in
this transcript.

Two tests separate what a restart does from what aging does
(`crates/kot/tests/agent_state_machine.rs`):

- `a_restart_alone_keeps_the_prefix_a_cache_could_still_hit` — restoring
  from `Host::history_path` resends byte-identical history; a real
  provider's prefix cache could still hit across a restart. The empirical
  cold-cache-after-restart effect is therefore not kot resending different
  bytes — more likely a provider-side cache TTL expiring during the
  downtime gap.
- The existing `an_old_result_shrinks_to_a_stub`, plus the second half of
  the test above, show `age()` breaking the prefix on its own schedule,
  restart or not.
- `a_reported_cache_hit_reaches_the_transcript` mocks a provider actually
  reporting a cache hit and confirms `cached_tokens` is parsed and logged
  correctly — the thing you'd check against a real endpoint.

Also from the same transcript: **zero `compact` events** across all 814
turns and 96 restarts. GLM's context window is reported as 1M tokens and
turns run 44–56k tokens median — only ~5% of budget, nowhere near
`FORCE_COMPACT_PCT`, and nothing else nudges a model this size to compact
voluntarily. So the same ever-growing, cache-hostile history gets fully
resent across every one of those 87 self-triggered reboots. This is the
motivation for the `Reboot` tool below, built the same day.

## The Langfuse-shaped disk log

`crates/kot/src/langfuse_log.rs` (`Host::langfuse_log()`, default `None`,
same shape as `Host::transcript()`) appends one event per line, shaped
exactly like an entry of Langfuse's ingestion API's `batch` array
(<https://langfuse.com/docs/api-and-data-platform/features/ingestion-api>,
MIT-licensed and open source — nothing here calls out to a server, this is
disk-only): a `trace-create` once per session, a `generation-create` per
model turn with real `usageDetails` (prompt/completion/cached tokens), and
a `span-create` per tool call and write. The point is that turning this into
a real `POST /api/public/ingestion` later, if that's ever wanted, is
wrapping chunks of up to 3.5 MB in `{"batch": [...]}` and sending them, not
reshaping the data — this session's whole cache-hit/restart/tool-stats
analysis above was done by hand-parsing the plain transcript; a real
observability tool would just show it.

One caveat noted in the file itself: Langfuse's `usageDetails` buckets are
additive (Anthropic's convention: `input + output + cache_read_input_tokens
= total`), but the OpenAI-compatible wire format `miot_llm` reads reports
`cached_tokens` as a *subset* of `prompt_tokens`, not additive on top. The
log accounts for that (`input` here is `prompt_tokens - cached_tokens`) so
the arithmetic lines up, but the mapping hasn't been verified against a
live Langfuse instance.

Test: `langfuse_log_records_a_trace_generation_and_spans`.

Since 2026-09-29 a `trace-create` also carries the build as Langfuse's own
`version` field (`crate::version::VERSION`, `<crate>+<sha>`, `-dirty` if the
tree was uncommitted) and `metadata.reasoning` next to `model` and
`context_window`, so runs from different builds/settings can be grouped and
compared. Traces written before that have neither — treat them as one
unversioned baseline.

### First real numbers (meow, the trashcan, 2026-09-29)

Pulled from `~/.akuma/kot/dumpster-akuma-amd64.langfuse.jsonl` (2.4 MB,
md5-checked): 2026-09-26 19:12 → 09-28 23:37, `zai-coding::glm-5.3-flash`,
69 traces, 505 generations, 467 tool spans. **No version in these** (see
above). All figures are this one cat on one workload (Intel HDA audio bring-up
in `../akuma`), so read them as leads, not constants.

| | |
|---|---|
| Prompt tokens | 62.8M: 43.5M cached, 19.3M not — **69.3% hit** |
| Output tokens | 437k; 18 turns over 8k tokens make up 263k (60%) |
| Prompt size | median 69k, max 269k |
| Turn latency | median 17 s, mean 33 s, p90 51 s, max 617 s; 4.6 h total |
| Output speed | median 8.4 tok/s (latency-bound, not generation-bound) |
| Tools (467) | Bash 338 (72%), Inspect 31, SendMessage 29, ReadFile 24, LocalTask 12, MultiEdit 12, Edit 4, Grep 3, Glob 2 |
| Tool failures | Bash 13/338 (mostly `sleep`/`while ps` polling that timed out, a `grep` exit 1), MultiEdit 1, Edit 1 — both `old_string not found` |
| Traces | 69, of which 55 on 09-27 alone; 22 have ≤12 turns |

What the numbers say:

- **Restarts are not where the cache goes.** First turns of a trace hit 47%
  but are only ~9% of uncached tokens (1.8M of 19.3M).
- **Mid-session collapses are.** 123 of 483 consecutive same-trace turns
  (25%) reused under half of the previous turn's prompt (prompts >8k), and
  they account for roughly 85% of the uncached tokens. Only 1 of those 123
  had a prompt that *shrank* by >2k tokens, so it's not compaction. By gap
  from one turn's end to the next's start: <5 s 18% collapse (n=358), 30–120 s
  29%, **2–5 min 60%** (n=80). Two candidates, not yet separated: the
  provider's prefix cache expiring during long tool waits, and our own
  history rewriting (a fed tool result shrinks to a stub after 6 turns —
  `Inspect` — which edits the middle of the prefix). The 18% with no wait at
  all points at the second.
- **Tools:** Bash is nearly everything and Grep/Glob are barely used, same
  navigation-through-Bash pattern as the earlier transcript analysis; the
  same 375 s `wavplay` command ran 4 times rather than being read back from
  its log; one `**/*.wav` Glob took 262 s on that box.
- **Caveat:** 267 of 505 generations have empty `output`. Probably
  tool-call-only turns (tool calls aren't in `output`), unconfirmed.

### Changes made from those numbers (2026-09-29)

- **Batched aging** (`age()`, `AGE_BATCH`). A fed result used to become a
  stub exactly `RESULT_TURNS` (6) turns later, one row at a time — which
  rewrites an already-sent message and misses the provider's prefix cache from
  there on, nearly every busy turn. Now the rewrite waits until the oldest row
  is 6 turns *past* due and stubs every due row at once, sooner if the due rows
  hold 40k chars (`AGE_FORCE_CHARS`) or the last turn was 5+ minutes ago
  (`CACHE_COLD` — the cache is probably gone anyway, so it's free). A result now
  stays in full for 6–12 turns, not 6. **Not yet measured**: whether this
  moves the 69% — compare versioned traces after a redeploy. z.ai documents no
  cache TTL and a [public measurement](https://github.com/deepseek-ai/deepseek-harness/discussions/5227)
  of the Coding Plan found misses even with a stable prefix (no session
  affinity, cats sharing a key evicting each other), so some of the 25% may not
  be ours to fix.
- **Each generation now says why its cache held or didn't** (`metadata`):
  `prefix_kept` of `prev_msgs` (how much of the last request this one repeated
  byte for byte, system prompt counted first), `aged` (rows stubbed this turn),
  `system_changed` (a context-budget warning rides in the system prompt),
  `gap_ms` since the last turn, `tool_calls`, `reasoning_chars`. A miss with
  `prefix_kept == prev_msgs` and `aged == 0` is the provider's, not ours.
  A trace also carries `metadata.started_by` (`start`, or a reset's reason).
  No finish reason yet: `miot_llm::Turn` doesn't carry one.
- **`Bash {"background": true}`.** Skips the one-at-a-time lane and isn't part
  of any turn's batch, so a `sleep`/`while ps` poll or an unrelated build no
  longer holds up edits or delays other results (results used to wait for
  every outstanding query, up to 10 s); it lands as its own turn when done and
  still counts as "something is running" for the check-in. The model opts in;
  the system prompt says never for something a later call needs.
- **A failed `Edit`/`MultiEdit` says what was close:** "does match if
  whitespace is ignored" (tabs/indentation/trailing space) or "closest line
  is N: …" (character-pair similarity ≥ 0.5). Meow's failures were 2 of 16
  edit calls, so this is small.
- **Not done:** a cap on output tokens per turn (GLM's thinking is easy to cut
  off into an empty answer — [OpenCode issue](https://github.com/redhat-et/pricetag/issues/12));
  skipping a repeated identical Bash command.

## The `Reboot` tool

Compacts (same mechanism as the model's own `Compact` call — history
replaced with a summary the model writes, persisted to disk before
anything else happens) and *then* actually reboots the host. Directly
targets the "zero compact events across 87 reboots" finding above: the
conversation now leaves itself a note before the box goes down, instead of
whatever wasn't written to a `LocalTask` just being gone.

Gated two ways, deliberately not offered by default:

- `Host::reboot_tool() -> bool` (`crates/kot/src/agent_state_machine.rs`,
  default `false`) — `tools()` doesn't even advertise `Reboot` unless this
  says yes, and the dispatch match arm re-checks it (`"Reboot" if self.host.
  reboot_tool() => ...`) rather than trusting the tool list alone.
- `AgentConfig::reboot_tool` (`crates/kot/src/agent.rs`) →
  `--reboot-tool`/`MIOT_REBOOT_TOOL` (`crates/kot/src/main.rs`) →
  `overlays/deploy/deploy.py`'s `Agent.reboot_tool`, `True` only for
  `dumpster-akuma-amd64` (meow) — the one cat whose own workflow already
  reboots the box it runs on as part of its kernel-build loop.

The actual OS-level side effect is split out into `Host::reboot(&self)`
(default: does nothing), separate from the gate, on purpose: the shared
agent loop decides *when* to compact-and-reboot, but issuing the real
command is host-specific and must never run by accident under `cargo
test`. `CatHost::reboot()` (`agent.rs`) is what actually shells out —
busybox `reboot -f` first (what meow already runs by hand today, per its
own transcript), a plain `reboot -f` as a fallback. The test harness's
`TestHost::reboot()` just records that it was called.

Tests: `reboot_is_not_offered_unless_enabled`,
`reboot_compacts_then_calls_the_hosts_reboot` — the latter checks the
compacted summary lands in the persisted history file and that the host's
`reboot()` was invoked, never a real `reboot -f`.

## The nudge budget vs. a model that only ever promises

Found live the same day, watching meow directly (not from the transcript
this time — `/activity`-style tailing while it was happening): from ~15:53
to 16:22 it answered three consecutive local-task nudges with a promise
("Firing it now, nya:", "Grepping the real stop-fn name + firing the fixed
chain, one pounce, nya:") and no tool call. `maybe_nag`'s budget
(`MAX_LOCAL_TASK_NUDGES`, 3) counts a nudge the moment it's *sent*, not
whether the reply actually did anything — so three empty promises in a row
looks identical to three nudges into true silence, and the budget ran out
("This is the last reminder on these") right as an operator message
happened to arrive and rescue it. Without that message, it would have gone
idle for good.

Fix: `AgentStateMachine::awaiting_nudge_reply`/`last_unfulfilled_promise`
track whether the *previous* nudge got a real tool call. If not, the next
nudge quotes the unfulfilled promise back verbatim ("Last time you said
this, then called no tool: \"...\". ... call the tool in this response, not
another promise") so the model can't just repeat itself. **The budget
itself is unchanged, deliberately** — a model that only ever promises needs
the same backstop as one that never answers, or nagging it burns turns
forever exactly the way the bound exists to prevent. This was a rule-based
fix, not a text-classification one: whether the previous turn called a tool
is a certain, structured fact from the API (`turn.calls.is_empty()`), not
something to infer from the prose — no "intent detection" crate needed or
appropriate here.

Test: `a_nudge_that_gets_only_a_promise_is_quoted_back_next_time`.

## Verified live against a real model, not just the fake-server tests

2026-09-26, `kot chat --llm http://localhost:11434 --model qwen3:4b` (Ollama,
already on this box) against small throwaway sandboxes — real tool calls
from a real model, not the scripted fake server the unit tests use:

- `Glob{"pattern": "*.rs"}` (no `path`) → correct file list, 15ms.
- `Grep{"pattern": "TODO"}` → correct files_with_matches list.
- `Edit{file_path, old_string, new_string, replace_all}` — the model
  produced this exact shape unprompted, from the schema description alone.
- `MultiEdit` with two edits in one call, both applied correctly, in order.
- **Found a real bug this way**: `LS` with no `path` argument at all (the
  model expected the same cwd-default `Glob`/`Grep` have) hit `read_dir("")`
  → "No such file or directory". Fixed (`local_tool`'s `LS` arm now
  defaults the same way the other two do) and covered by
  `ls_with_no_path_at_all_defaults_to_the_working_directory`.

GLM-4.7-Flash itself never finished pulling in this session — `qwen3:4b`
stood in as one of the three real target families while that ran. A trap
worth naming so nobody re-falls into it: Ollama pre-allocates a pull's
blob at its full final size before any content lands, so `ls -la`'s size
on the `*-partial` file is always the download's final size, never its
progress — watching it made two separate restarts here look like a dead
stall (stuck at exactly 19,019,269,280 bytes) when the pull may well have
been progressing the whole time. `du` on the same file shows real bytes on
disk; measured properly after a third, genuinely fresh restart, it was
moving at ~1.1 MB/s — slow (hours, for 19 GB) but not actually stuck.

## Working directory matters now, for meow

`ReadFile`/`WriteFile`/`Edit`/`MultiEdit`/`LS`/`Glob`/`Grep`'s cwd-default
(a bare relative path, or — the `LS` finding below — no path at all) only
lands somewhere useful if the process's own working directory is the
actual source checkout, not wherever herd/systemd happens to start it
(`/root` or `/root/kot`). `overlays/deploy/deploy.py`'s `Agent.cwd` (added
2026-09-26, meow only: `/src/github.com/netoneko/akuma`) threads a `cd` into
`start.sh.tmpl` before `exec`. **Only wired for the `akuma`/`fcguest`
shapes** — a `linux`-shape agent (tama) would need the same threaded into
`kot.service.tmpl`'s `WorkingDirectory=` if it's ever needed there; not
built, since nothing needs it yet.

## `deploy.py` retries now — the akuma box answers, slowly, not never

Staging this session's binary on meow's box (`dumpster-akuma-amd64`) hit
the same failure repeatedly: `_put_via_http`'s HTTP `GET` would succeed
(logged, 200), and the very next `ssh` call — a *different* connection,
moments later — would die at the connection layer (exit 255). Kirill's own
observation nailed it: the box wasn't down, its LAN ping round-trip was
measured over 800 ms during what's presumably a heavy local `cargo build`
(normal: 50-75 ms), and it dropped back to normal once the build let up.
Congestion, not an outage — consistent with `../akuma/docs/
SELF_HOSTING_AMD64.md`'s open question about intermittent network silence
on this box, from the same day.

`overlays/deploy/deploy.py`'s `on()`/`put()` used to `die()` on the first
failure, which meant re-running `up` by hand and hoping the box answered
this time — exactly what was happening for real, several times, before
this fix. `_run_retrying` (`ON_RETRIES` = 3, `ON_RETRY_DELAY_S` = 5) wraps
every `ssh`/`scp`/`limactl copy` call `on()`/`put()` makes; a failure logs
and retries rather than dying immediately. This isn't theoretical — it's
what actually got both this session's real deploys (the tool set, then the
nudge-guard fix) to land: one needed 1 retry, the other 2, and one attempt
still needed a fully manual re-run afterward because even 3 tries within
one `_run_retrying` call weren't enough that time (each dead SSH attempt
itself took minutes to fail, not seconds — the congestion was on the order
of many minutes, not a quick blip).

## Confirmed live: the whole chain actually works

After staging, `dumpster-akuma-amd64` was redeployed for real (killing the
running process, not just `NO_ENABLE=1` staging) and watched come back:
binary md5 on the box matched the local build exactly
(`679b554099f8d57e0cd96dc685b7c667`), new PIDs came up, and its history
restored cleanly — 1124 messages, 0 trimmed. Its very next nudge (fresh
budget, since restart doesn't persist `local_nudges`/`awaiting_nudge_reply`
— only history/local-tasks survive a restart) got a real `Bash` call, not
another empty promise.

## Not done yet

- **A real local GLM smoke test.** No current GLM checkpoint fits in 48 GB
  — the family has scaled up hard (GLM-5.3-Flash, what meow/tama actually
  run, is 320.6B total/17.3B active; even Unsloth's most aggressive 1-bit
  dynamic quant is ~93 GB). GLM-4.7-Flash (30B total/~3.6B active,
  Unsloth's `UD-Q4_K_XL` ≈ 18 GB) is the smallest current family member and
  fits comfortably — pulled via `ollama pull glm-4.7-flash:q4_K_M` (the
  official `library/glm-4.7-flash` tag) for exactly this purpose.
