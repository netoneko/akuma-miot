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
  (`open`/`plan`/`update`/`reassign`/`say`/`set_leader`/`set_root`).
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
  `miot-tasks`. Election only; blocks still move by pull-sync and
  *leader wins, back to the last compaction*. Its tests are a simulated
  network (partitions, kills, chaos).
- `crates/miot-keys` — an account *is* an ed25519 public key
  (`sp_runtime::AccountId32`); `account_from_ssh` reads an
  `authorized_keys` line so root is just a public key.
- `crates/miot-llm` — provider layer on `genai`; `Llm::local` (any
  OpenAI-compatible server — llama-server, never ollama in the fleet) and
  `Llm::glm` (z.ai **coding plan** endpoint, token from a file).
- `crates/kot` — **the one binary**, ships as `dist/<arch>/kot`. Polish for
  "cat". `kot run --as <name>` is a mesh node (`node.rs`) plus, given
  `--llm`/`--glm`, that cat's agent loop (`agent.rs`) in the same process,
  still talking to its node over HTTP (`docs/CLI.md` §5a). Every other verb
  (`task open|list`, `say`, `artifact`, `peers`, `log`, `clear`, `id`, bare
  `kot` = REPL) is a stateless client of *any* node (`client.rs`); a
  replica forwards `/submit` and `/account` to the elected primary.
  `crates/miot` (the old node+client binary) was merged in and deleted
  2026-09-22 (`docs/CLEANUP.md` item 2). `tests/election.rs` runs three
  real nodes over localhost HTTP, kills the primary, revives it.
- `miot-cli` never shipped under that name — `docs/CLI.md` is its design of
  record, and `kot`'s client verbs are the implementation.

## Where to read

- `HANDOFF.md` — state, traps, roadmap. Start here every session.
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

## Known gaps (don't assume these are fixed without checking the code)

- **No OpenSSH private-key signing.** Root signs with the project-native
  seed at `~/.akuma/miot/id_ed25519.seed`; its `.pub` is what every node's
  `MIOT_ROOT_PUBKEY` holds. Not the operator's `~/.ssh` key.
- **Mesh membership is static.** `MIOT_MEMBERS` is genesis and `MIOT_PEERS`
  is config; changing either is a coordinated restart, not an operation.
  No joint consensus — fine for one operator, not for anything else.
- **Election ≠ replication.** A block the primary produced that no replica
  pulled before it died is lost to the rewind (records, not work —
  `miot-store`'s docs). There is no commit index.
- **`seq` in `/events` restarts when a node rebuilds its log** (demotion,
  rewind, adopted checkpoint). The agent loop resets its cursor; any other
  client holding one should too.

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
