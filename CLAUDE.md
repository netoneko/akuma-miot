# Akuma Miot

A litter of LLM agents that coordinate through a blockchain instead of a
socket — task state, results and final artifacts live on chain; the agents
are ordinary clients. Full state-of-the-project narrative, traps already
paid for, and the current roadmap live in `HANDOFF.md` at the repo root —
read that first, it is kept current and this file does not repeat it.

## Layout

- `crates/miot-primitives` — vocabulary (`TaskId`, `Act`, `Effect`, `Limits`,
  `Timers`). `no_std`.
- `crates/miot-tasks` — **the lifecycle, as a pure state machine.** No clock,
  no I/O. This is the real thing; everything else hosts it.
- `crates/pallet-litter` — thin FRAME wrapper: `ensure_signed` → load →
  `TaskTable::apply` → store → emit. One dispatchable per verb
  (`open`/`plan`/`update`/`reassign`/`say`/`set_leader`/`set_root`/
  `clear_all`/`publish_standalone_artifact`/`request_compaction`).
  `request_compaction` (root-only, added 2026-09-23) touches no task state —
  it exists only so `node.rs::submit` can match it and fire `Store::compact`
  on demand, the same mechanism `clear_all` triggers as a side effect.
- `crates/miot-runtime` — `construct_runtime!`, executed **natively** (no
  wasm, no `sc-executor`). Timer/limit constants live here as
  `parameter_types!`, tuned against measured LLM turn lengths, not against
  `miot-primitives`' generic defaults.
- `crates/miot-store` — the chain's block log on ParityDB: append, compact,
  `rewind_for_fork`, plus a small `aux` space (the election's persisted
  term/vote). Every mesh member is durable.
- `crates/miot-mesh` — **who produces blocks**: Raft-style leader election
  (terms, one vote per term, majority quorum, pre-vote, check-quorum,
  stickiness) as a pure state machine, no clock, no I/O, same rule as
  `miot-tasks`. Election only; blocks move by pull-sync and *leader wins,
  back to the last compaction*, plus (2026-09-25) a **leader push** to a
  peer that can't call out: status polls carry the poller's own status
  (`Mesh::on_inbound`), and the primary pushes blocks to any peer whose head
  is stuck (`Mesh::push_targets`, `node.rs` `/chain/push`). Its tests are a
  simulated network (partitions, one-way links, kills, chaos).
  Background: HANDOFF, "One-way reachability".
- `crates/miot-keys` — an account *is* an ed25519 public key
  (`sp_runtime::AccountId32`); `account_from_ssh` reads an
  `authorized_keys` line so root is just a public key.
- `crates/miot-llm` — provider layer on `genai`; `Llm::local` (any
  OpenAI-compatible server — llama-server, never ollama in the fleet, with one
  deliberate exception: kuro on Ollama's `gemma4-yolo-4b` since 2026-09-25,
  `docs/FLEET.md`) and
  `Llm::glm` (z.ai **coding plan** endpoint, token from a file; reasoning
  effort `low` by default since 2026-09-25, `--reasoning`/`MIOT_REASONING`
  to change it — `miot_llm::GLM_REASONING` has the measurements). A hosted
  model can't be asked its context window: `--context-window`/
  `MIOT_CONTEXT_WINDOW` sets it (2026-09-26; without one a GLM cat never
  compacted — HANDOFF, "Tokens, racing tool calls, a multiline composer").
- `crates/kot` — **the one binary**, ships as `dist/<arch>/kot`. Polish for
  "cat". `kot run --as <name>` is a mesh node (`node.rs`) plus, given
  `--llm`/`--glm`, that cat's agent loop (`agent.rs`) in the same process,
  still talking to its node over HTTP (`docs/CLI.md` §5a). Every other verb
  (`task open|list`, `say`, `artifact`, `peers`, `log`, `clear`, `compact`,
  `id`, bare `kot` = REPL) is a stateless client of *any* node
  (`client.rs`); a replica forwards `/submit` and `/account` to the elected
  primary. `chat` (`chat.rs`, added 2026-09-23) is the one verb that isn't:
  a model in this process with no node and no chain at all, real tool
  execution (`Bash`/`ReadFile`/`WriteFile`/`SendMessage`) via
  `miot_llm::local_tools`. Both `run`'s agent loop and `chat` are hosts of
  `agent_state_machine.rs` (2026-09-24) — one loop: tool results are fed
  back through the same inbox as wakes (HANDOFF, "The agent state machine"). `crates/miot` (the old node+client binary) was
  merged in and deleted 2026-09-22 (`docs/CLEANUP.md` item 2).
  `tests/election.rs` runs three real nodes over localhost HTTP, kills the
  primary, revives it. `activity.rs` (2026-09-25) is each cat's live record
  — never on chain, carried on the mesh status exchange, `GET /activity` —
  and `local_tasks.rs` a cat's own to-do list (`LocalTask`); HANDOFF,
  "Watching the litter".
- `overlays/deploy/hosts/ryzen/` — sora's host side as systemd units
  (`sora-net.service`, `sora.service`), 2026-09-25. ryzen's llama-server
  (one; since 2026-09-25 only sora uses it — tama is on GLM) comes from
  `deploy.py llama`.
- `miot-cli` never shipped under that name — `docs/CLI.md` is its design of
  record, and `kot`'s client verbs are the implementation.

## Where to read

- `HANDOFF.md` — state, traps, roadmap. Start here every session.
- `docs/AGENT_STATE_MACHINE.md` — the one agent loop (`kot run` and
  `kot chat`) as a diagram: wakes, queries vs records, follow-up cap and
  held results, the check-in before idling, how long tool output is fed
  and paged. Written 2026-09-24 after meow's kernel build stalled; read it
  before changing `agent_state_machine.rs`.
- `docs/TEAHOUSE.md` — **the teahouse** (茶馆), the name of the mesh/chain:
  the seven-member topology as actually running (5 home + 2 AWS), with a
  diagram, what was shown live, and the honest limits. Current-state; it
  supersedes `docs/TOPOLOGY_TARGET.md`'s five-agent plan. Added 2026-09-24.
- `docs/MAPPING_REPORT.md` — design of record, the findings table (§1.1) is
  the actual asset (timers, "root is not a worker", "accept a submit without
  a claim" — each is a named test in `miot-tasks`).
- `docs/RESULTS.md` — what actually ran, with numbers. Trust this over any
  other claim.
- `docs/CLI.md` — `miot-cli` requirements. Scrollback is sacred; no
  alt-screen TUI. §5a covers connecting to any node with no local DB.
- `docs/references/storage.md` — the two stores (chain: ParityDB; per-cat
  local: Turso), why they're split, and the leader-wins rewind rule.
- `docs/FLEET.md` — which cat runs which model on which host, updated as
  hardware changes; check dates before trusting a host/model assignment.
- `docs/LOCAL_SIM.md` — running the agent loop and multi-cat behavior with
  no fleet and no real infra (`kot chat`; a peered local mesh of `kot run`
  cats against dev `llama-server`s). Two real `agent.rs` bugs and two open
  findings (a non-root broadcast wakes nobody; a failed `/submit` is never
  retried) came out of actually doing this, 2026-09-23.
- `docs/MESH_AUTH.md` — who a peer/client actually is, on the wire: the
  `x-miot-signer`/`x-miot-sig` header envelope, then mTLS pinned to the same
  keys. Added 2026-09-23, prompted by planning an AWS deploy.
- `docs/MESSAGE_ROUTING.md` — where a write actually goes: the
  `Route::{Here,Primary,Nobody}` decision every `/submit` makes, the mempool
  queue and peer relay a no-route node falls into (2026-09-25), and how
  `/tx/{hash}` learns "sealed" by asking a reachable peer rather than
  re-deriving it from a synced block (which carries only effects, not
  extrinsics). Diagrams; read before changing `node.rs::route`/`mempool_round`.
- `docs/GIT_HOME.md` — the requested git home for the litter (a repo the
  cats push their own branches to, Kirill pulls from): the options, the
  recommendation, what's still Kirill's call, and `MIOT_CONTEXT`, the shared
  system-prompt files that tell every cat where the source is. 2026-09-25.
- `docs/KEY_MANAGEMENT.md` — what one account's key now backs (chain writes,
  every read, the TLS connection itself), the dev-seed footgun in `kot`'s own
  CLI defaults, and the genesis-generation procedure (`deploy.py`/`deploy.sh
  ids`) — including what changes when a new node (e.g. on AWS) joins.
  Added 2026-09-23.

## Known gaps (don't assume these are fixed without checking the code)

- **No OpenSSH private-key signing.** Root signs with the project-native
  seed at `~/.akuma/miot/id_ed25519.seed`; its `.pub` is what every node's
  `MIOT_ROOT_PUBKEY` holds. Not the operator's `~/.ssh` key.
- **Mesh membership is static.** `MIOT_ROSTER` is genesis (committed to
  chain state by name, `pallet_litter::Roster`, served as `/roster`;
  `MIOT_MEMBERS` is gone, 2026-09-23) and `MIOT_PEERS` is config; changing
  either is a coordinated restart, not an operation. A block log remembers
  the genesis it was built under and refuses a different one.
  No joint consensus — fine for one operator, not for anything else.
  **Patrons are the exception that isn't genesis (2026-09-25):** an
  account in a member's `MIOT_PATRONS` may read and pull but never vote
  or write, and its own node runs `--patron` (a learner that pulls from
  any member it can reach). Per node, so it rolls out without a new chain.
  `docs/MESH_AUTH.md`, "Patrons".
- **An operator's client doesn't pin the node (2026-09-23).** `kot`'s
  client reads the roster from whichever node it connects to (`/roster`)
  and trusts that node's cert as presented (`tls::client_config_any_node`);
  the node still pins the client to genesis. Something on the path can pose
  as a node to the client (fake reads, seeing what's sent) but can't relay
  to a real one without a member's key. Accepted for a private chain; node↔
  node traffic stays fully pinned.
- **Election ≠ replication.** A block the primary produced that no replica
  pulled before it died is lost to the rewind (records, not work —
  `miot-store`'s docs). There is no commit index.
- ~~**A push-only node can't write.**~~ **Has a fallback** (checked in the
  code 2026-09-25, same day, prompted by root hitting the 503 for real):
  `/submit`'s `Route::Nobody` case no longer refuses — it queues the raw
  extrinsic in a bounded in-memory `Node::mempool` and answers `200
  "pending"` immediately, and a new periodic `mempool_round` relays it to
  every peer this node's own config *can* reach until one of them can
  forward it (a new `/mempool/relay` endpoint, `crates/kot/tests/
  mempool.rs`). A new `/tx/{hash}` endpoint, itself relayed the same way,
  answers whether it's landed. **The router forwards
  (`docs/runbooks/deploy-aws-node.md` §1) are still the real fix and still
  not done** — this is a same-day fallback that gets a write there via
  whatever peers are reachable, not a route home for yuki/shiro themselves.
  Kept for the history: yuki and shiro follow a home primary by push, with
  no route to it, so `/submit` on them had nowhere to forward: it answered
  503 "the primary is kuro, but this node has no route to it". `HANDOFF.md`,
  "A mempool for the no-route case".
- **`seq` in `/events` restarts when a node rebuilds its log** (demotion,
  rewind, adopted checkpoint). The agent loop resets its cursor; any other
  client holding one should too.
- ~~**A non-root broadcast (`say` with no `to`) wakes no cat's agent loop.**~~
  **Fixed** (checked in the code 2026-09-25): `Effect::wakes()` is true for
  every `Said`, and `Node::absorb` sends a broadcast out with the `"*"`
  sentinel that each cat's filter accepts. Kept for the history:
  `Effect::wakes()` says it should (`to.is_some() || from_root`), but
  `Effect::to()` — what actually reaches the wire as the `wakes` hex field
  — collapses `to: None` to `None` regardless of `from_root`. A human
  watching the REPL/`kot log` still sees the text (rendering doesn't
  consult `wakes()`), which is why this went unnoticed. Found 2026-09-23,
  not fixed — `docs/LOCAL_SIM.md`.
- ~~**A failed `/submit` is never retried.**~~ **Fixed** (checked in the
  code 2026-09-25): `Cat::submit` makes 4 attempts with backoff, retrying
  an unreachable node or a stale nonce, not a refusal. Kept for the history: `Cat::submit` (`agent.rs`) makes
  one HTTP attempt; on failure the wake is gone for good — `seen`/`cursor`
  already advanced before the attempt. Task wakes get incidental cover from
  the chain's own re-nudge tick; a `"said"` DM has no backstop at all.
  Found 2026-09-23, not fixed — `docs/LOCAL_SIM.md`.

- **A cat's conversation now survives a restart (2026-09-25, night).** It's
  saved per turn to `~/.akuma/kot/<name>.history.<epoch>.json` and restored
  with a one-time "you were restarted" note (uptime included); before that
  every restart was total amnesia except the local task list, which is how
  meow ended up in a reboot loop. HANDOFF, "Why meow kept rebooting".
  Since 2026-09-26 a fed tool result shrinks to a one-line stub after 6
  turns (`Inspect` rereads it), and result ids carry on across a restart.
- **A cat's `Bash`/`ReadFile`/`WriteFile` run one at a time, in call order,
  across turns (2026-09-26).** Before that they all ran at once and meow's
  edit scripts shredded its own files. A long build now holds up the file
  calls after it; that's the trade. `docs/AGENT_STATE_MACHINE.md`, "One lane".
- **`git push` from the metal Akuma box fails on a big pack** (EBADF in
  `pack-objects`, open kernel bug). `akuma-litter` is seeded from the mac so a
  cat's push is small. An Akuma ssh session has no `$HOME`: set
  `HOME=/root` before testing git there, or credentials look missing.
- **sora's guest and the metal box need keys that live outside
  `../akuma/target`** (a `cargo clean` deleted the only way in, 2026-09-25):
  `~/.akuma/kot/fcguest.ssh-key` for sora's guest (in its image's
  `/etc/sshd/authorized_keys`), `~/.ssh/id_ed25519` for the metal box.

- **A checkpoint is also the session boundary (2026-09-25).** Any
  compaction (`/clear`, `kot compact`) moves `last_checkpoint`, and every cat
  treats that as a new session: conversation, `question` and local tasks
  gone. So compaction can't be automatic yet, and replay after a restart
  grows by ~14,400 blocks a day until someone compacts. Measured cost and the
  fix: HANDOFF, "Compaction: what a window would cost".

## Build, run, test

```bash
cargo test --workspace                        # host-native, no docker (there is no docker any more)
overlays/local/build.sh all                   # dist/{aarch64,x86_64}/kot (+ storeprobe, mmapprobe)
overlays/deploy/deploy.sh up <agent>|all      # the 5-agent mesh, docs/TOPOLOGY_TARGET.md
kot --node http://192.168.1.126:9944 --roster "$MIOT_ROSTER" peers   # roster + who is primary
cargo run -p kot -- run --as solo --seed 1 --db /tmp/solo.db         # a mesh of one, local dev
```

## Real infrastructure available to this project

Three separate things get called "Akuma" in this project's docs — don't
conflate them, and use these exact names going forward (a previous session
briefly reused `fc` for two of them, which was confusing enough to correct
mid-conversation):

- **Lima VM `fc`** (`limactl list`) — aarch64, `vz` nested virt, plain
  Linux. Both the Firecracker/KVM host for the next item, and where a cat
  can run directly in its own Linux userspace (`node3`/`kuro` in
  `docs/TOPOLOGY.md` do this) — two different uses of the same VM, not the
  same thing.
- **`akuma-guest`** — the actual Akuma kernel (not Linux), booted as a
  Firecracker microVM *nested inside* `fc` (`../akuma/overlays/
  devbox-firecracker/`). `docs/TOPOLOGY.md`'s `node4` runs here, verified
  live, as a herd-managed service — but that's one boot, one binary, a
  specific syscall surface exercised; not a general claim about Akuma.
- **The real `akuma` host** — an ssh alias (`ssh akuma`, port 2222, key in
  `~/.ssh/config`) to actual hardware ("the dumpster", an HP box) running
  the same Akuma kernel *on real hardware*, not nested in anything:
  `uname -a` reports `x86_64 GNU/Linux` but there's no `/etc/passwd`,
  `whoami` fails, and the pthread/mmap/socket surface anything beyond a
  static binary needs is **unverified** (`docs/FLEET.md` "Honest gaps") —
  `akuma-guest` running the same syscall surface doesn't settle this one;
  it's a different machine.

**Two arches, one script:** `overlays/local/build.sh aarch64` (Lima `fc`,
`akuma-guest`) and `x86_64` (ryzen, the akuma box, a ryzen Firecracker
guest). The old aarch64-only `build-akuma.sh` is gone.

**Getting a binary onto the `akuma` host:** not scp (no SFTP subsystem), not
an SSH exec channel (stalls at exactly 1,048,576 bytes — see HANDOFF traps).
HTTP from the host works; `overlays/deploy/deploy.sh`'s `put` does it
(python `http.server` on the mac + busybox `wget`, md5-checked).

The sibling `../akuma` repo (the OS itself) has runbooks worth reading before
debugging anything on that hardware rather than re-deriving it:
`docs/runbooks/boot-and-connect.md`, `docs/runbooks/recover-wedged-vm.md`,
`docs/runbooks/debug-network.md`, `docs/runbooks/run-on-firecracker.md`,
`docs/runbooks/selfhost-kernel-build-amd64.md`,
`docs/runbooks/stage-rust-toolchain-amd64.md`. Its `docs/README.md` has a
symptom matrix ("I see X, what do I read?") — check there before forming a
theory about anything that looks like a kernel-level oddity rather than an
akuma-miot bug.

**`overlays/deploy/deploy.py` is now the Python rewrite of `deploy.sh`
(2026-09-23), built on the pattern `../akuma/scripts/box/` uses for the
bare-metal box's own build environment** — five files (`akuma-dev.env`,
`kbuild`, `ubuild`, `mbuild`, `kinstall`) checked into `../akuma` and copied
onto the physical box rather than authored there, so the rig can be
rebuilt from a checkout and can't silently drift from what the repo
documents (`../akuma/scripts/box/README.md`). Same two properties, carried
over: an env file every wrapper sources (an sshd session inherits *no*
environment — the same reason `deploy.sh`'s own `on()`/`put()` fight
akuma's shell every time), now generated once as `kot.env`/`start.sh` and
templated from `overlays/deploy/templates/*.tmpl` (checked in, not authored
inline as heredocs); and every remote command is an argv list handed
straight to `subprocess`, never a hand-quoted string a second shell
re-interprets — the concrete fix for `deploy.sh`'s akuma/fcguest shape
being "the least reliable part" (HANDOFF's herd traps; the fcguest identity
check that once compared against a key `mesh.env` no longer used, relabeled
to persona names 2026-09-22; the `ryzen-akuma-amd64` crash loop, `[herd]
Service kot exited with code 241`). `deploy.py env <agent>` was checked
byte-for-byte identical to `deploy.sh env <agent>` for all five agents
before anything shipped. `--dry-run` prints every `on`/`put` a real run
would do without touching a host. `deploy.sh` itself is untouched and still
works — `deploy.py` is the one to reach for going forward, not a like-for-
like replacement forced on anything already depending on the shell one.

**Redeployed 2026-09-23 with `deploy.py`, four of five agents — every box
except the bare-metal `akuma` host** (`dumpster-akuma-amd64`; skipped
deliberately, not attempted and not verified this round):
`mac-linux-aarch64` (Lima `fc`) redeployed for real and confirmed live
(`systemctl is-active kot.service` → `active`, replayed 5087 blocks,
running the rebuilt binary with today's `agent.rs` fixes). The other three
— `ryzen-linux-amd64`, `ryzen-akuma-amd64`, `mac-akuma-aarch64` — were
*not* reachable this session (done from a coffee shop, off the home LAN:
`ryzen`/`akuma` both timed out) and still need `python3 overlays/deploy/
deploy.py up <agent>` run once back on it. Don't assume they're already on
the new binary without checking.

## Working with Claude Code in this repo

Copied from `../akuma`'s own `CLAUDE.md`, which states it for that repo —
adopted here too, and it applies doubly when work in this repo reaches into
`../akuma` (reading its docs, building against it, deploying to a Lima/
Firecracker guest it owns):

Never use the `fork` subagent type (or any multi-agent fan-out) for work that
touches `../akuma` — do it directly instead. Forking copies the whole
conversation context into a background agent, which costs far more tokens
than doing it inline, and `../akuma` is explicitly a "read the docs, don't
re-derive them" repo (see above) where that context is rarely worth paying
for twice.

**Commit vocabulary describes the user's actions, not a request for yours.**
When the user says "committed", "checkpoint", "committed checkpoint",
"pushed", "landed", "stashed" or similar, they are *reporting what they just
did* so you know the state of the tree — not asking you to do it. Treat such
a line as context, not an instruction. The same goes for a bare noun phrase
on its own line in a longer message; it is a status note.

If you genuinely believe a commit is warranted, say so and stop. Only an
unambiguous imperative addressed to you — "commit this", "please commit",
"make a commit" — is a request.
