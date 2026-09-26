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
motivation for a reboot tool that compacts first (below) — still designed,
not yet built as of this writing.

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

GLM-4.7-Flash itself never finished pulling in this session (Ollama's
download stalled twice at exactly the same byte count, 19,019,269,280 —
restarted, not yet confirmed fixed) — qwen3:4b stood in as one of the three
real target families while that's unresolved.

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

## Not done yet

- **A real local GLM smoke test.** No current GLM checkpoint fits in 48 GB
  — the family has scaled up hard (GLM-5.3-Flash, what meow/tama actually
  run, is 320.6B total/17.3B active; even Unsloth's most aggressive 1-bit
  dynamic quant is ~93 GB). GLM-4.7-Flash (30B total/~3.6B active,
  Unsloth's `UD-Q4_K_XL` ≈ 18 GB) is the smallest current family member and
  fits comfortably — pulled via `ollama pull glm-4.7-flash:q4_K_M` (the
  official `library/glm-4.7-flash` tag) for exactly this purpose.
