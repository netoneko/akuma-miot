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
  `rewind_for_fork`. Built and tested; **not yet wired into `miot-node`**,
  which currently keeps state in memory only.
- `crates/miot-keys` — an account *is* an ed25519 public key
  (`sp_runtime::AccountId32`); `account_from_ssh` reads the operator's
  existing `authorized_keys` line so root needs no new secret. Signature
  verification and address recovery are not yet wired into any wire path —
  see "Known gaps" below.
- `crates/miot-llm` — provider layer on `genai`.
- `crates/miot-node` — the chain as an HTTP process, block loop on its own
  clock. `AccountId = u64` today; calls arrive as JSON naming an account
  and are trusted, not verified.
- `crates/miot-cat` — one cat, one container/process, talks to the node.
- `crates/miot` — single-process harness (scripted / `--live` / `--chat`) and,
  via `--rpc`, a signing client of a real node. Ships as `dist/miot`.
- `miot-cli` does not exist yet — `docs/CLI.md` is its design of record
  (Phase 3), including how a client resolves `@name` tags against the
  on-chain roster and submits over RPC without holding any local state.

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

- **No signatures on the wire.** `miot-node`'s `/call` takes a JSON `who: u64`
  field and trusts it — `ensure_signed` only checks *an* origin was signed,
  not that the HTTP caller is who they claim. `miot-keys` proves address
  recovery in isolation but nothing calls it yet. This is HANDOFF's gap #1.
- **`miot-store` is wired to nothing.** The node's state is in-memory only.
- **No consensus.** One node owns the chain; `rewind_for_fork` has never run
  against a real disagreement.

## Build, run, test

See `HANDOFF.md` § "Run it" for the current commands (host models, docker
compose from `overlays/local/docker-compose.yml`, `miot`, Akuma
binaries). Quick reference:

```bash
cargo test --workspace                 # host-native, no docker
overlays/local/llama-swarm.sh up       # 4 llama-servers on the HOST (no Metal in Docker)
docker compose -f overlays/local/docker-compose.yml up -d
overlays/local/build-akuma.sh          # dist/miot, dist/storeprobe — see target note below
```

## Real infrastructure available to this project

Two separate things both called "Akuma guest" exist in this project's docs —
don't conflate them:

- **Lima VM `fc`** (`limactl list`) — aarch64, `vz` — the Firecracker/KVM host
  for `overlays/local/README.md` Stage 2 (agents as microVMs, one TAP each).
- **The real `akuma` host** — an ssh alias (`ssh akuma`, port 2222, key in
  `~/.ssh/config`) to actual hardware ("the dumpster", an HP box) running
  Kirill's own kernel, *not* stock Linux: `uname -a` reports
  `x86_64 GNU/Linux` but there's no `/etc/passwd`, `whoami` fails, and the
  pthread/mmap/socket surface anything beyond a static binary needs is
  **unverified** (`docs/FLEET.md` "Honest gaps").

**Target mismatch to fix before item 3/4 of HANDOFF's roadmap:**
`overlays/local/build-akuma.sh` cross-compiles for
`aarch64-unknown-linux-musl`, but the reachable `akuma` host answers
`x86_64` — a binary built by that script will not run there. Either add an
`x86_64-unknown-linux-musl` cross target for the real host, or confirm which
target the Lima-hosted Firecracker guest actually needs and keep the two
paths (Lima aarch64 microVM vs. the physical x86_64 box) explicit rather than
assuming one build serves both.

**Getting a binary onto the `akuma` host:** not scp (no SFTP subsystem), not
an SSH exec channel (stalls at exactly 1,048,576 bytes — see HANDOFF traps).
HTTP from the host works; `build-akuma.sh`'s own trailing note has the
one-liner.

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
