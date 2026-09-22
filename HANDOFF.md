# Handoff

State of Akuma Miot as of 2026-09-22. What runs, what doesn't, what to do next,
and the things that will waste your time if you don't know them.

---

## What this is

A litter of LLM agents that coordinate through a blockchain instead of a
socket. Task state, results and final artifacts live on chain; the agents are
ordinary clients. The lineage is `akuma/userspace/meow`, whose own docs called
it *"a hand-rolled Tendermint with futures bolted onto it"* — **none of its code
is imported**, only its behavioural findings, each now a named test.

## Run it

```bash
cargo test --workspace                 # 94 tests, host-native, no docker

# models on the HOST (Metal). Docker on macOS has no GPU passthrough.
overlays/local/llama-swarm.sh up       # 4 llama-servers, ports 8081-8084

docker compose -f overlays/local/docker-compose.yml up -d   # 2 nodes + 4 cats
curl -s localhost:9944/head
curl -s localhost:9944/meta                          # genesis hash + spec/tx version
# there is no more unauthenticated /call — every act is a signed extrinsic.
# --rpc is the same `miot` binary (shipped as dist/miot; `miot node` is the
# other thing it can do) pointed at a real node instead of driving one:
cargo run -p miot -- --rpc http://localhost:9944 --identity-seed 1 \
  --open "your question here"
cargo run -p miot -- --rpc http://localhost:9944 --repl   # interactive session, real signed lines
cargo run -p miot -- --rpc http://localhost:9944 --clear  # fail every open task — new session, same chain
docker compose -f overlays/local/docker-compose.yml logs -f
curl -s localhost:9944/artifact/t1 | python3 -m json.tool
```

**The hardcoded scripted-cats demo, `--live`, and in-process `--chat` this
section used to show are gone** — merged out of `miot` (2026-09-22,
alongside the `miot`+`kot`→`miot`+`kot` rename): the demo was
redundant with `cargo test`'s own coverage, and `--live`/`--chat` simulated
cats "talking" to each other, which was never actually true of the real
architecture — the chain is the only channel between agents, always.
`kot` (formerly `kot`) is a real cat's agentic loop against a real
node; there's no in-process stand-in for it anymore.

Akuma-shippable binaries:

```bash
overlays/local/build-akuma.sh          # dist/miot (5.1 MB), dist/storeprobe (0.8 MB)
                                       # aarch64 musl. For the x86_64 hosts (ryzen, the
                                       # real akuma box) build the same two -p miot -p
                                       # miot-store binaries with x86_64-linux-musl-gcc +
                                       # rustup target x86_64-unknown-linux-musl — see
                                       # docs/TOPOLOGY.md (no script for it yet).
```

---

## The crates

| crate | what | tests |
|---|---|---|
| `miot-primitives` | vocabulary: `TaskId`, `Act`, `Effect`, `Limits`, `Timers`. `no_std`. | 5 |
| `miot-tasks` | **the lifecycle, as a pure state machine.** No clock, no I/O. Event-sourced: `TaskTable::apply` is the only place state is written, live or replayed. | 41 |
| `pallet-litter` | thin FRAME wrapper: `ensure_signed` → load → apply → store → emit | 16 |
| `miot-runtime` | `construct_runtime!`; `AccountId32`/`MultiSignature`, real `UncheckedExtrinsic` + `Executive`, **executed natively — no wasm** | 2 |
| `miot-store` | block log on ParityDB, compaction-boundary rewind, leader-wins | 14 |
| `miot-keys` | ed25519 identity: seeds for cats, the operator's SSH *public* key → `AccountId32`, hex wire encoding | 14 |
| `miot-llm` | provider layer on `genai` (15 providers, GLM included) | — |
| `kot` | one cat's agentic loop, signs its own extrinsics and talks to a node — Polish for "cat" (renamed from `kot` 2026-09-22, once cat+node work converged into `miot` below and it needed its own name); a container today, or any process that can reach a node (running on a Lima VM, and on the real Akuma kernel via Firecracker, as of 2026-09-22 — `docs/TOPOLOGY.md`) | — |
| `miot` | one binary (ships as `dist/miot`), two jobs, merged 2026-09-22 (formerly `miot` + a separate `miot`): `miot node` is the chain as a process — HTTP, real block lifecycle (`Executive`) on its own clock, `/submit` verifies before it dispatches, persists+replays via `miot-store` (`MIOT_DB`), compacts on `/clear`; anything else is an RPC client of one — one-shot (`--open`/`--say`/`--clear`) or `--repl` (an operator's interactive session, real signed lines, replays history on start). The old scripted-demo/`--live`/in-process-`--chat` modes were dropped, not merged. | — |

**`miot-tasks` is the real thing.** Everything else hosts it. That is why the
pallet is thin and why the same machine runs with or without a chain.

---

## Decisions that took the longest to reach

**FRAME, executed natively. No wasm.** The blob exists so it can be swapped
on-chain for a forkless upgrade; we do not upgrade. FRAME's runtime side is an
ordinary Rust library, so `sc-executor`, the wasm toolchain and the state trie
all fall away. Cost: no runtime upgrades, no `sc-*` tooling. Benefit: a 5.1 MB
static binary that runs on `busybox` with nothing else in the image. The
upgrade path stays open — add `substrate-wasm-builder` and `impl_runtime_apis!`
and the same runtime compiles to a blob.

**Use polkadot-sdk for everything it has.** A hand-rolled signing envelope
(domain string, genesis, nonce, length-prefixing) was written, tested, and
deleted: `UncheckedExtrinsic` + the `frame-system` transaction extensions cover
all of it plus mortality and spec-version binding, and it is now actually
wired — `miot` runs `frame_executive::Executive::initialize_block` /
`apply_extrinsic` / `finalize_block` for real, which is also what makes
`CheckGenesis`/`CheckMortality` mean something (they bind to `BlockHash`
storage, which nothing populated before). What survived from the hand-rolled
version is `account_from_ssh` — reading an OpenSSH **public** key, which
polkadot-sdk does not do — so root is the operator's existing key, *"no new
secret to manage."* Signing as that key from a client is not built: `miot-keys`
has no OpenSSH **private**-key parser, only `Identity::from_seed`. Anything
that isn't the real operator (every cat, and `miot --rpc` for now) signs
with a deterministic seed instead — fine here, since a forged sender still
can't happen without the matching private key, and one operator's own swarm
already trusts every seed it configured.

**A no-fee chain still needs `frame_system::CheckNonce` to see an account,
which needs a balances-shaped hook nothing here provides.** `CheckNonce`
refuses any account whose `providers`/`sufficients` are both zero — a gate
`pallet-balances` normally trips when it credits an account. We have neither
fees nor balances, so nothing was ever going to trip it, and *every*
signature, however correct, was refused with `InvalidTransaction::Payment`
(read: not a fee error, a "this account doesn't exist yet" error). Fixed by
having `miot` call `inc_providers` for every account in `MIOT_MEMBERS`
(default `1,2,3,4,5`, the same seed convention as `MIOT_ROSTER`) at genesis —
named `catnip` in `crates/miot/src/main.rs`, because an account that hasn't had
any can't be nonce-checked. **Worth reconsidering later:** a minimal
`pallet-balances` (zero-fee weight, or just used for its provider bookkeeping)
might be a smaller surface than operator-curated membership lists, especially
once membership needs to grow without a redeploy.

**`DirectiveNag` was unbounded, and a live run found that the hard way.**
Watching a real litter close a task, mimi (leader) mis-fired `TaskUpdate`
(`WrongStatus`/`WrongKind`) a few times on the same directive, and nothing
would ever have stopped the chain re-nagging it forever — no cap existed,
unlike a worker's `MaxNudges`. Fixed: `Timers::max_directive_nudges` (default
3, same count as `MaxNudges`) bounds it, and exhausting the budget now fails
the parent outright (`TaskStatus::Failed`, `Effect::Failed`) rather than
going quiet — there's no "reassign the leader" act the way `ReassignNeeded`
exists for a stuck worker, so silence would mean nobody ever heard about it.
A resolving action (a result landing, a leadership change, a successful
`plan`) resets the budget; only the nag loop itself spends it.

**`clear_all` — a session boundary, not a task-lifecycle act.** Operator-only
extrinsic that fails every currently-open parent at once (reuses
`TaskStatus::Failed`/`Effect::Failed`, same terminal outcome as the
directive-nag exhaustion above — the log doesn't distinguish "leader never
resolved it" from "operator moved on," because downstream nothing needs to).
`/clear` in `miot --chat`; `--clear` on `miot --rpc`. Task ids keep
incrementing past a clear — a "session" is just old parents going quiet, not
a fresh genesis.

**`miot-tasks` is genuinely event-sourced now, not just observably so —
2026-09-22.** Every verb used to mutate `self.tasks`/`self.artifacts`/
`self.leader` directly and return the resulting `Effect`s as a description
of what it had already done. Now every verb *decides* its effects (pure, no
mutation) and calls the new `TaskTable::apply(effect, now)` once per effect
— the **only** place state is written. Live application and replaying a
persisted effect log after a restart go through the identical function, so
they cannot quietly drift apart the way two independently-maintained code
paths eventually do. Forced `Effect::Opened`/`Record`/`Closed` to carry
`text`/`body`/`author` — fields an agent's prompt never needed but `apply`
does, since it can only reconstruct a `Task`/`Artifact` from what the effect
itself carries. Verified by
`replaying_the_effect_log_reproduces_live_state_exactly`
(`crates/miot-tasks/src/tests.rs`): runs a full lifecycle live, replays only
the resulting effects into an untouched table, asserts field-for-field
equality. `docs/PROTOCOL.md`'s tx/state/event section is the write-up; this
is what makes wiring `miot-store` into `miot` (item 2, below) a matter
of persisting+replaying the effect log rather than something to re-derive
from raw transactions.

**Two stores, not one.** Chain write path is ParityDB (`miot-store`, +373 KB).
Agent-local tool output is planned for Turso, and is **per-cat and private** —
nothing reads another cat's store.

**Leader wins, back to the last compaction.** No fork choice, no voting. A cat
that diverges rewinds to the latest compaction at or below the fork point and
replays the leader's blocks. Right for one operator's trusted swarm; badly
wrong for a public chain.

---

## What is real and what is not

**Real:** the pallet and its state machine; the FRAME runtime executing
natively, through a real `Executive` block lifecycle; `ensure_signed` deciding
authority from a signature that is now actually verified — `/submit` decodes
and checks a real `UncheckedExtrinsic` (signature, nonce, mortality, genesis
and spec/tx version) before anything dispatches; artifacts stored and read
back from chain state; five containers on a network with the chain ticking in
its own process; four llama-servers; **ParityDB, wired in and proven,
2026-09-22** — `miot` persists every block's effects and replays them
on start (`MIOT_DB`, defaults to `miot.db`; the docker `node` service
mounts a named volume at `/data`), verified both standalone (kill/restart a
bare `miot`, `/events` and `/tasks` came back byte-identical) and
through `docker compose restart node` / `up -d --force-recreate node` — a
task opened before either survived it. Practical upshot: **cats no longer
need restarting when the node does** — `seq` numbering is continuous across
a restart now (the log is real, not reset to empty), so a cat's already-held
cursor stays valid instead of pointing past a wiped log. See
`docs/runbooks/run-local-swarm.md`, updated accordingly.
**a cat running somewhere that isn't a container**
— `kuro` moved from its docker container to the Lima VM (`fc`, aarch64 Linux)
as of 2026-09-22: cross-compiled with the same `aarch64-unknown-linux-musl`
toolchain `build-akuma.sh` already used, copied onto the VM's own disk with
`limactl copy`, and it picked up an in-flight sub-task the moment it
connected. The node never noticed it moved — which is the actual point of
`docs/CLI.md` §5a and `overlays/local/README.md` Stage 1, now demonstrated
rather than asserted.
**a second node, replicated for real, 2026-09-22** — the replica role moved
off docker the same day it was born there: `node2` now runs as a bare
`miot` on **ryzen** (192.168.1.126, systemd `miot-node2.service`,
`MIOT_ROLE=replica MIOT_PEER=http://<mac>:9944`), cross-compiled with the
x86_64 twin of `build-akuma.sh`'s aarch64 toolchain. Verified converged
(`/tasks`/`/events` byte-identical; a live signed `--say` replicated within
one sync interval). The docker `node2` container was stopped, removed and
deleted from the compose file — one replica identity, not two running under
it. `docs/TOPOLOGY.md` has the details.
**a fifth node, durably up on the real akuma hardware, 2026-09-22.**
`node5` (`ssh akuma`, the physical Akuma-kernel box) runs a *fresh-genesis
primary* of its own as a **herd service**, survives `kill` and full reboot
(replaying 1000+ persisted blocks each start), and takes signed extrinsics
from the mac. The same day it started, it also forced three **kernel** fixes
in ../akuma — none of them akuma-miot bugs: writable `MAP_SHARED` file
mappings went from refused-by-design (ParityDB's `MmapMut` died at every
reopen, `os error 38`) to demand-paged with whole-region write-back;
ext2 `truncate` stopped answering `Ok(())` for extend (a silent no-op that
made `set_len`-before-write a zero-byte file); and `posix_fadvise` got its
table row (parity-db `try_io!`s it). `storeprobe` completes **all 7 stages
on real hardware** — the diagnostic this project shipped and never ran,
now green where it was built to run. Full story and the honest residual
(replica-catch-up not re-tested post-fix): `docs/TOPOLOGY.md`'s `node5`
section.
**real compaction, 2026-09-22** — `Store::compact` fires on root's `/clear`
(a genuine snapshot of the whole storage trie via `sp_io::TestExternalities`'s
own `into_raw_snapshot`/`from_raw_snapshot`, not a hand-rolled subset); a
reconciliation now lands on that checkpoint instead of genesis. See item 5's
Part 1 writeup.
**a fourth node, on the actual Akuma kernel, 2026-09-22** — item 3, below,
used to be "run `storeprobe` on Akuma"; the bigger claim turned out true
first. `docs/TOPOLOGY.md` has the diagram and the honest caveat.

**Not yet real:**

- **No election, no automatic failover.** A node's role (primary/replica) is
  an operator-set env var, changed by restarting the process — deliberate,
  same trust model as `set_leader`/`set_root`, but it means nothing detects
  a dead primary and promotes a replica on its own. Fine for one operator's
  swarm; would need real work for anything else. (In progress this session —
  Part 2 of item 5's plan: an N-way mesh with real leader election.)
- **Akuma-on-the-real-hardware: tested, durable, still one boot.** `node5`
  (above) runs on the physical `akuma` box as a herd service and survives
  restart over a grown ParityDB. `node4` on the Firecracker guest remains
  intermittent for a different reason (index-growth panic). `storeprobe`
  and `mmapprobe` (`crates/miot-store/src/bin/mmapprobe.rs`) are the two
  diagnostics; both have run on the real host.
- **No OpenSSH private-key signing.** `miot-keys` reads the operator's
  *public* key (`account_from_ssh`) but cannot sign with the matching private
  one — nothing here has parsed an OpenSSH private key file. Signing as the
  real operator today means using a seed whose derived account you separately
  told the node is root (`MIOT_ROOT`/`MIOT_ROOT_PUBKEY`), not the operator's
  actual `~/.ssh` identity.
- **No real membership growth path.** `MIOT_MEMBERS` (see `catnip` above) is a
  fixed operator-curated list at genesis. Adding a cat mid-run needs either a
  new extrinsic that provisions an account, or the balances pallet noted above.

---

## Traps that already cost time

- **`gemma3:4b` cannot be a cat.** Ollama refuses: `400 "does not support
  tools"`. Tool support gates model choice.
- **`llama-server` needs `--jinja`** or it silently never emits `tool_calls`.
- **Docker on macOS has no GPU passthrough.** Containerised llama-servers are
  CPU-only and 5–10× slower. Keep models on the host.
- **Pin both Dockerfile stages to the same distro.** A newer builder links a
  glibc the runtime image lacks, and it fails at *exec* time:
  `GLIBC_2.39 not found`.
- **Four llama-servers on one GPU do not parallelise.** One alone answers in
  4.1 s; four concurrently take 16.5 s — exactly 4×. They serialize. `-t 1` is
  enough (measured: identical to `-t 3`).
- **The SSH exec channel to an Akuma guest stalls at exactly 1,048,576 bytes**
  and `dist/miot` is 5.1 MB. Use HTTP via `10.0.2.2`. scp does not work at all
  (no SFTP subsystem).
- **Timers are ~50× too conservative.** `claim_window` 100 blocks was sized for
  a 120–200 s turn; turns here are 3–60 s. Nothing is broken, but a live litter
  wants single-digit windows.
- **`DirectiveNag` had no cap, and a live run hit it for real** (mimi mis-fired
  `TaskUpdate` a few times on the same directive). Fixed —
  `Timers::max_directive_nudges` — see "Decisions" above. Watch for this again
  anywhere else a nag/retry loop is added without an explicit budget.
- **`limactl shell fc -- ls ~/foo` resolves `~` on the host shell, not the
  guest**, because tilde expansion happens client-side before `limactl` ever
  sees the argument — so it silently becomes the *host's* home directory path,
  which then gets treated as a guest path and (usually) fails to resolve.
  Use an absolute guest path, or `limactl shell fc -- bash -lc 'echo $HOME'`
  to get the real one (`/home/<user>.guest`, also aliased at
  `/home/<user>.linux` — same inode, either name works once you're inside).
  `limactl copy` itself has no such problem; it takes real guest paths.
- **A `Failed` parent doesn't normally disappear immediately — `TaskTable::gc`
  (wired into `pallet-litter`'s `on_initialize`) only drops rows closed at
  least `GcKeepFor` blocks ago (14,400, i.e. 24h at `BLOCK_MS=6000`).** Not a
  bug on its own — `gc`'s doc comment calls this "the only kind of compaction
  that is consensus business," deliberately not instant — but 24h is tuned
  for a real chain's audit trail, not this dev loop, and it reads as a bug
  the first time a task lingers in `/tasks` after you thought you were done
  with it. **`/clear` (`clear_all`) is the one exception, fixed 2026-09-22**:
  it now calls `gc(now, keep_for: 0)` itself right after failing every open
  parent, sweeping *every* already-closed/failed parent immediately, not just
  the ones it just failed — "/clear is a session boundary, nobody needs the
  old records lying around." `GcKeepFor` itself is unchanged for the natural
  path (`DirectiveNag` exhaustion) — a parent that failed on its own still
  sits for 24h unless an operator `/clear`s it away. Revisit `GcKeepFor`
  itself with the same "measured against LLM turns" treatment the timers
  above got, if that natural-failure lingering ever gets in the way too.
- **A `WrongStatus` refusal on `TaskPlan`/`TaskUpdate` can be a race, not a
  bug** — observed live, 2026-09-22: a parent got a `PlanNeeded` directive,
  mimi picked it up and spent 45s planning it, and by the time `TaskPlan`
  landed the same parent had already been `/clear`'d (in that case, by a
  concurrent operator test against the same node) — so the pallet correctly
  refused a plan against a task that was no longer `Open`. The refusal is the
  system working as designed (`docs/MAPPING_REPORT.md` §1.1: "applied-vs-
  refused must be typed, never sniffed"), not silent corruption. Before
  chasing this as a state-machine bug, check whether the task's status
  changed between the directive being issued and the reply landing — `curl
  .../tasks` or `.../events` will show it.

- **Akuma's writable-`MAP_SHARED` gap: found, fixed upstream, same day.**
  The 2026-09-22 session that first touched the physical box found ParityDB
  un-restartable there (`os error 38` at reopen; a raw mmap probe of the
  shape **segfaulted**). The fix landed in ../akuma, not here — writable
  `MAP_SHARED` file mappings are now demand-paged with write-back, ext2
  `truncate` really extends, and `posix_fadvise` exists
  (`docs/reference/subsystems/amd64-shared-write-mmap.md` in ../akuma).
  Residuals to keep honest: replica-catch-up sync from this box has not
  been re-tested on the fixed kernel (it wedged mid-sync pre-fix), and
  node4's Firecracker-guest index panic is a *different*, still-open
  failure. Both probes (`storeprobe`, `mmapprobe`) run on the metal now.
- **Herd on the trashcan wedges failed services, not configs.** A service
  whose spawn failed a few times under the box's previous herd stayed dead
  after its conf was repaired — a renamed service started on the first
  reload where the fixed one never did; a reboot cleared it. Also: the
  box's previous `/bin/herd` predated config reload entirely (never picked
  up services added after boot) — the laptop-built herd is installed now.
- **Restarting a process on akuma leaves zombies** (nothing reaps) — `ps`
  fills with dead entries; don't read them as live.
- **A stale `python3 -m http.server` on the mac squatted port 8123** and
  served 404s that looked like a wrong URL. `lsof -i :PORT -sTCP:LISTEN`
  + `ps -p <pid> -o command=` before blaming the client side.

---

## Next, in order

1. ~~**Signed extrinsics.**~~ **Done, 2026-09-21.** `AccountId` is
   `AccountId32`; `/call` is gone; `/submit` takes a signed
   `UncheckedExtrinsic` and `miot` verifies it for real through
   `frame_executive::Executive` before dispatch. `kot` and `miot --rpc`
   both sign through the shared `miot_runtime::client::sign`.
2. ~~**Wire `miot-store` into `miot`.**~~ **Done, 2026-09-22.** Persists
   every block's effects (not raw extrinsics — `TaskTable::apply`, the
   event-sourcing decision above, is what made replay a matter of folding a
   log rather than re-deriving one); replays on start. Verified: standalone
   kill/restart and `docker compose restart|up --force-recreate node` both
   reproduce identical `/events`/`/tasks`. Prerequisite for item 5, below.
3. ~~**Run `dist/storeprobe` on an Akuma guest.**~~ **Superseded, 2026-09-22
   — the bigger claim turned out true first.** Rather than the diagnostic
   probe, the *full* `miot` was run directly on the actual Akuma kernel
   — not the real `akuma` host below, but `akuma-guest`, a Firecracker
   microVM nested inside the Lima VM `fc` (`../akuma/overlays/
   devbox-firecracker/`). Real multi-threaded tokio, real axum HTTP server,
   real ParityDB (sparse mmap'd files — the exact thing `storeprobe` exists
   to test, now proven live instead of by a diagnostic stand-in), real
   `reqwest` client — running unmodified, as a herd-managed service
   (`/etc/herd/enabled/miot.conf`, auto-starts on boot same as `sshd`).
   Verified end to end: a task opened against the real docker `node` showed
   up in this node's `/tasks` a few seconds later, over the same
   primary/replica HTTP protocol every other node in the mesh speaks.
   `docs/TOPOLOGY.md` has the full diagram and the honest caveat (one boot,
   one binary, a specific syscall surface — not a general claim about
   Akuma). Item 4 below is still genuinely open: this ran on a *Firecracker
   guest*, not the real physical `akuma` host.
   **Correction, same day: not durably up.** ParityDB panics on this guest
   once its index needs to grow past some threshold (`docs/TOPOLOGY.md` has
   the exact panic and what was ruled out chasing it). `node4` runs for a
   while after a fresh store, then crashes and stays down. The claim above
   — real tokio/axum/ParityDB/reqwest working together on Akuma — is still
   true and still the first time any of this ran there; "durable, long-
   running node" is not yet also true, and isn't being chased further right
   now.
4. **Ship `dist/miot` to Akuma** and run a cat there against a host model.
   ~~Needs a host `llama-server` reachable from that box~~ — updated
   2026-09-22, twice: the *node* now runs there **durably** (`node5`,
   herd-supervised, restart-proven over a grown DB — see "What is real"),
   the x86_64-unknown-linux-musl build + busybox-wget transfer path is
   proven, and the kernel's mmap/`truncate`/`fadvise` blockers are fixed.
   What's still open: a cat on the box (an exposed inference port decision —
   unchanged), and the replica direction (node5 as a replica of mac's log —
   it wedged mid-catch-up on the pre-fix kernel and is worth one retry now).
5. ~~**A second node.**~~ **Done, 2026-09-22.** `miot` gained a role
   (`MIOT_ROLE=primary|replica`) rather than a full P2P/gossip layer — a
   replica pulls its peer's block log over two new HTTP endpoints
   (`GET /chain/head`, `GET /chain/blocks?from=N&limit=M`) and folds new
   blocks in through the same `Node::apply_block` a local-store replay
   already used, refusing `/submit` itself (read-only). Deliberately not
   called "leader"/"follower" — `pallet-litter`'s `leader` is already the
   *litter* leader (an agent role); this is a different axis and needed
   different words: **primary**/**replica**. No election, no automatic
   failover — an operator changes a node's role by restarting it with
   different env vars, same trust model as everything else here.
   Verified live: `node2` (`overlays/local/docker-compose.yml`) mirrors
   `node`'s `/tasks`/`/events` within one sync interval and survives its own
   restart. More importantly, `rewind_for_fork` finally ran against a real
   disagreement rather than a synthetic one — a standalone third `miot`
   was pointed at `node`'s peer as a replica, killed, restarted independently
   as its own primary, given a submit that only it received (diverging its
   log), then pointed back at `node` as a replica again. It printed `sync:
   diverged from peer above block 481, rewound to 0 (dropped 485 block(s))`
   and came back byte-for-field identical to `node`. One real bug found and
   fixed doing this: an early version compared only the tip block for
   divergence, which missed it — an empty `Vec<Effect>` (a "quiet" block,
   the common case) encodes identically no matter which chain produced it,
   so a diverged block sitting under a few agreeing quiet blocks above it
   went undetected. Fixed by comparing the replica's *entire* local range
   against the peer once, at (re)connect (`reconcile_if_diverged`), rather
   than the tip on every tick — cheap because a replica that only ever
   appends blocks it received from its peer cannot diverge from it again on
   its own before the next restart. **Follow-up, done same day**:
   `compact()` was never called anywhere at the time this was written, so
   every reconciliation landed at genesis rather than a partial rewind —
   correct per the documented rule, just expensive. Wired up right after
   (`compact()` fires on root's `/clear`; see "real compaction" in "What is
   real," above) — reconciliation now lands on the checkpoint instead.

---

## Where to read

- `README.md` — the diagrams and the agent/CLI split
- `docs/PROTOCOL.md` — **the canonical reference**: vocabulary (`TaskId`,
  `Act`, `Effect`, `Directive`), the tx/state/event distinction, every
  dispatchable's authority, the actual tuned timer values (not
  `miot-primitives`'s generic doc-comment defaults), GC, and which tools an
  agent gets for which wake reason. Read this before re-deriving any of it
  from source again.
- `docs/MAPPING_REPORT.md` — design of record: the findings table (§1.1), the
  misconception the port introduced (§1.2), what was deliberately not
  rebuilt. §7 has the open, not-yet-built design questions (wayward,
  participant notification, on-chain tagging) — `docs/PROTOCOL.md` is what's
  built, this is what's proposed.
- `docs/RESULTS.md` — **what actually ran, with numbers.** Evidence, not
  intentions. Read this before trusting any claim elsewhere.
- `docs/CLI.md` — `miot-cli` requirements. Scrollback is sacred.
- `docs/runbooks/run-local-swarm.md` — the everyday loop: bring the local
  litter up, talk to it, rebuild/redeploy after a code change (the compose
  file does not build the image), restart `kuro` on Lima after a node
  restart, and where to look before assuming "a cat isn't responding" is an
  application bug.
- `docs/references/storage.md` — the two stores, with measured binary costs
- `docs/references/README.md` — **three event loops, four orders of magnitude
  apart, and none may await another.** Both of meow's deadlocks were that
  mistake.

## One thing to keep

`docs/MAPPING_REPORT.md` §1.1 is a table of findings that were only learnable
by running the thing — timers measured in LLM turns, bounded nudges, "root is
not a worker", "accept a submit without a claim". Each is a named test in
`miot-tasks`. A failure there means something learned the expensive way has
been quietly un-learned. That table is the actual asset; the code is
replaceable.
