# Handoff

State of Akuma Miot as of 2026-09-23 (late: `kot` merge, election, new mesh — `docs/CLEANUP.md`; mesh-internal HTTP now authenticated — `docs/MESH_AUTH.md`). What runs, what doesn't, what to do next,
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
cargo test --workspace                 # 108 tests, host-native, no docker (docker is gone)

overlays/local/build.sh all            # dist/aarch64/kot (9 MB), dist/x86_64/kot (11 MB), static musl
overlays/deploy/deploy.sh up all       # the mesh: docs/TOPOLOGY_TARGET.md (live agents only)

R="$(grep ^MIOT_ROSTER overlays/deploy/mesh.env | cut -d= -f2-)"
kot --node http://192.168.1.126:9944 --roster "$R" peers            # roster, primary, terms, heads
kot --node http://192.168.1.126:9944 --roster "$R" say --to mac-linux "hi"
kot --node http://192.168.1.126:9944 --roster "$R" task open "your question"
kot --node http://192.168.1.126:9944 --roster "$R"                  # the REPL
kot --node http://192.168.1.126:9944 --roster "$R" log --follow

cargo run -p kot -- run --as solo --seed 1 --db /tmp/solo.db         # a mesh of one: local dev
```

Signing defaults to the operator's root identity (`~/.akuma/miot/id_ed25519.seed`);
any node will do (`--node` then `--nodes a,b,c`), because a replica forwards
`/submit` and `/account` to whoever is primary.

---

## The crates

| crate | what | tests |
|---|---|---|
| `miot-primitives` | vocabulary: `TaskId`, `Act`, `Effect`, `Limits`, `Timers`. `no_std`. | 5 |
| `miot-tasks` | **the lifecycle, as a pure state machine.** No clock, no I/O. Event-sourced: `TaskTable::apply` is the only place state is written, live or replayed. | 41 |
| `pallet-litter` | thin FRAME wrapper: `ensure_signed` → load → apply → store → emit; `Replaying` (fold without re-ticking) | 17 |
| `miot-runtime` | `construct_runtime!`; `AccountId32`/`MultiSignature`, real `UncheckedExtrinsic` + `Executive`, **executed natively — no wasm** | 2 |
| `miot-store` | block log on ParityDB, compaction-boundary rewind, leader-wins, `aux` (persisted vote) | 17 |
| `miot-mesh` | **leader election** — Raft's, election only, with pre-vote + check-quorum + stickiness. Pure state machine; tests are a simulated network with partitions and kills | 10 |
| `miot-keys` | ed25519 identity: seeds for cats, the operator's SSH *public* key → `AccountId32`, hex wire encoding | 14 |
| `miot-llm` | provider layer on `genai` (15 providers, GLM included) | — |
| `kot` | **the one binary** (`dist/<arch>/kot`), 2026-09-22: `kot run --as <name>` = a mesh node + that cat's agent loop in one process; every other verb is a stateless client of any node. Absorbed `crates/miot` (node + RPC client), which is deleted. `tests/election.rs` = three real nodes over localhost, kill the primary, revive it | 2 |

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

**Election, 2026-09-22 (`crates/miot-mesh`).** `MIOT_ROLE`/`MIOT_PEER` are
gone; every `kot run` node is a mesh member and the mesh elects which one
produces blocks. Raft's election only: terms, one persisted vote per term,
majority quorum, randomized timeouts, **pre-vote** (a node cut off never
inflates its term, so it never deposes a healthy leader on return),
**check-quorum** (a leader that can't see a majority steps down by itself),
and **stickiness** (nobody votes while they can still hear a leader). That's
how "unreachable" is told apart from "lost the election". A vote goes only
to a log at least as far along, compared as `(head_term, head)`, so an
ex-leader that kept producing on a minority side, and so has the *higher*
head, can't win with blocks the majority never saw. Heartbeats are
**pulled** (everyone polls everyone's `/mesh/status`), so each node only
needs its own outbound routes, which is what the NAT'd guests allow.
Blocks still move the old way: pull-sync plus *leader wins, back to the
last compaction*. A node that changes role rebuilds from its store
(demotion) or runs the open block's missing tick (promotion); a replica
that gets a new primary reconciles its whole range against it once. A
replica forwards `/submit` and `/account` to the primary, so a cat's
co-located node is always a valid endpoint.

**Replay double-applied the tick, 2026-09-22 — found wiring election.**
Folding a block (a replica syncing, *or any node replaying its own store on
restart*) ran `on_initialize`'s `tick` locally **and** applied the
producer's recorded tick effects on top. `Directed` *increments*
`directive_nudges_used`, so every replica and every restarted primary burned
the leader's directive budget at double speed. `/tasks` never shows that
counter, which is why the "byte-identical" replica checks missed it. A
promoted replica would have failed parents early. Fixed with
`pallet_litter::Replaying` (tick skipped while folding; `gc` still runs,
since it drops rows without an effect). The test is
`a_folded_block_log_reproduces_the_producers_state_exactly`, whose control
arm shows the old behaviour diverging.

**Artifacts are budgeted in pages, 2026-09-22.** `MaxArtifact` was a flat
64 KiB, about 16k tokens: half a local model's whole 32k window. It is now
`ARTIFACT_PAGES` (4) × `ARTIFACT_PAGE_BYTES` (4 KiB ≈ 1k tokens) = 16 KiB, in
`miot-runtime`, and the leader's `ArtifactNeeded` prompt states the word
budget.

**Mesh-internal HTTP is authenticated and on HTTP/2, 2026-09-23
(`docs/MESH_AUTH.md`).** `/mesh/vote`, `/mesh/status`, `/chain/{head,blocks,
checkpoint}` used to trust whatever a reachable HTTP client claimed — no
relation to `/submit`'s signed extrinsics, which authorize a state change,
not a peer's identity. Every mesh node now carries its own keypair (`kot run
--as <name>` needs `<name>` in the roster or an explicit `--seed`/
`--seed-file`, unconditionally now, not just when an agent loop is
attached), and every mesh-internal request and response carries
`x-miot-signer`/`x-miot-sig` headers, verified against genesis `members` ∪
{root, leader}. Client-facing endpoints (`/tasks`, `/account`, `/submit`,
...) are untouched. Alongside it: `reqwest`'s client uses
`.http2_prior_knowledge()`, and axum's `http2` feature turns on
`hyper-util`'s connection-preface sniffing so `axum::serve` accepts it —
mesh traffic is a tight poll loop between the same peers, so one multiplexed
connection beats a handshake per call. Verified live (`kot run --as solo`,
curl with and without a valid signature) and via `cargo test --workspace`
(unchanged, 108+ tests including the 3-node election integration test);
**not yet redeployed to the fleet** — host-native only this pass.

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

- ~~**No election, no automatic failover.**~~ **Built, 2026-09-22** — see
  "Election" under Decisions. Honest residuals: membership is static
  (genesis + `MIOT_PEERS`), and a block no replica pulled before its
  primary died is lost to the rewind (no commit index).
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

- **The akuma box can stop spawning after a `kill`** (2026-09-22): right
  after `deploy.sh retire-old` killed the old `node5` process, sshd still
  authenticated but every exec returned status 241 with no output, for
  minutes. Not recovered remotely; needs a power cycle. Suspect the amd64
  no-slot-recycler class (`../akuma/docs/archive/AKUMA_AMD64_NO_SLOT_RECYCLER.md`);
  not root-caused. Batch ssh execs to that box, and prefer letting herd
  restart a service over killing it by hand.
- **`kot` as a *replica* on the akuma metal box wedges within minutes**
  (2026-09-22, reproduced across a reboot): listener refuses even from
  `127.0.0.1`, sync stops, threads all `R` and still accruing CPU. The old
  `node5` was durable only as a *primary*, with no outbound HTTP; a replica
  is an HTTP client and server at once, which is the one combination that
  has never stayed up on that kernel. Unconfirmed hypothesis, not
  root-caused. `../akuma` territory. See `docs/TOPOLOGY_TARGET.md`.
- **ParityDB on aarch64 Akuma (`akuma-guest`): fixed 2026-09-22, in
  `../akuma`.** Two aarch64-only kernel bugs, found by running `kot` there:
  (a) no `fadvise64` arm in the aarch64 dispatcher, so reopen failed with
  ENOSYS; (b) the amd64 fix's change to the *shared* `mmap::plan` sent
  writable `MAP_SHARED` down aarch64's lazy path, which never registered it
  for write-back, so a clean close lost every write (storeprobe reopened
  empty). Now `storeprobe` passes 7/7 on the guest, and `kot` survives
  `kill -9` with its whole store (753 blocks replayed, no peers to sync
  from). Full write-up: `../akuma/docs/archive/MIOT_MESH_ON_AKUMA.md`, which
  also covers the metal-box replica wedge (open) and PSTATS naming amd64
  syscalls from the aarch64 table (open). **Lesson:** a change to shared
  kernel code must be checked against both dispatchers.
- **amd64 Akuma: kot goes deaf in minutes, on metal *and* in a 1-vCPU
  Firecracker guest on ryzen (`192.168.1.50`, the `ryzen-fc` agent).** Not
  fixed. Handed off in `../akuma/docs/archive/AKUMA_AMD64_KOT_REPLICA_WEDGE.md`.
  Strongest lead: x86_64 `accept4` (288) has no amd64 syscall row, so tokio's
  accepts get ENOSYS. Until then akuma-metal and ryzen-fc can't hold a seat,
  and the mesh is ryzen-linux + mac-linux + mac-fc.
- **Akuma's sshd merges stderr into stdout.** Anything parsed from `ssh
  akuma '…'` output needs `2>/dev/null` on the far side.
- **The z.ai token is a coding-plan key**: `paas/v4` answers "insufficient
  balance"; only `coding/paas/v4` works. `Llm::glm` routes bare model names
  to `zai-coding::`.
- **Lima forwards guest ports to the mac's loopback only by default.** The LAN
  couldn't reach a node in `fc`. `../akuma/overlays/devbox-firecracker/host-setup.sh`
  now creates `fc` with 9944-9949 on `0.0.0.0` (`LIMA_LAN_PORTS`), and refuses
  an old instance without it.
- **`pkill -f <pattern>` over ssh can match the ssh session's own `bash -c`
  line and kill it** (exit 255). Kill by PID.

---

## Next, in order

0. **Future work on `kot`: the operator's roster name becomes `ken`, not
   `root`** (asked 2026-09-22; not started). It must stay configurable
   (e.g. `--operator-name` / `MIOT_OPERATOR_NAME`), and `ken` is the default
   for what it means in Japanese:
   - 賢 (ken): wise, clever (also common in names)
   - 権 (ken): authority, right (as in 人権 *jinken*, "human rights")

   This renames the *roster label only*. The authority itself stays what the
   pallet calls root (`MIOT_ROOT_PUBKEY`, `Authority::Root`, `set_root`,
   `clear_all`'s check), and the on-chain `from_root` flag on `said` stays
   as is. Nothing on chain carries a name. The places that spell it today:
   - `crates/kot/src/main.rs`: `DEV_ROSTER` (`root=1,…`), and the
     `miot-root` comment on the identity's `.pub` line.
   - `crates/kot/src/agent.rs`: the planner's worker list filters out
     `n != "root"`, so the operator is never handed a sub-task
     (`RootNotAssignable`). This filter must follow the configured name,
     not a literal.
   - `overlays/deploy/deploy.sh` `ids`: writes `root=pub:<acct>` into
     `mesh.env`'s `MIOT_ROSTER`. Regenerating `mesh.env` changes no key
     and no genesis. The roster is client-side lookup only, so this is
     just a redeploy.
   - Docs that say "root" when they mean the operator's *name* (`docs/CLI.md`
     §2 tags, `docs/runbooks/run-the-mesh.md`). Leave the ones that mean
     the *authority* alone.

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
6. **Open, observed 2026-09-23, not yet root-caused: `@name`-tagging a cat
   in the REPL doesn't reliably get a reaction.** Reported live against the
   deployed fleet — operator types `@sora ...`, sora doesn't respond. Traced
   the plumbing that *should* make this work and it looks correct at every
   layer checked: `Effect::wakes()` (`crates/miot-primitives/src/lib.rs`)
   returns `true` for a `Said` with `to: Some(_)`, and `agent.rs`'s event
   loop does filter `/events` on `wakes == its own hex account`
   (`crates/kot/src/agent.rs:289`). Neither end was changed this session.
   Didn't reproduce this live before writing it down (out of scope for the
   session that found it), so treat the above as "nothing obviously wrong
   in the code path," not "cause found." Things to check first, before
   assuming a deeper bug: whether the tagged cat's agent loop is actually
   running at all right now (a node with no `--llm`/`--glm` is silent by
   design, not broken — `docs/TOPOLOGY_TARGET.md`'s "blocked" agents), and
   whether `@name` in the REPL resolved to the account you expected
   (`parse_targets`/`Roster::account`, `crates/kot/src/client.rs`) —
   `mesh.env`'s `MIOT_ROSTER` was relabeled to cat names this same session
   (`root`/`meow`/`tama`/`kuro`/`sora`/`mimi`), so a stale roster string in
   an operator's shell env would silently resolve `@sora` to nothing or the
   wrong account rather than erroring.

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
- `docs/runbooks/run-the-mesh.md` — the everyday loop: build, `deploy.sh up`,
  `kot peers` to see who leads, logs, and what "stable" looks like.
- `overlays/deploy/deploy.sh` — the deployment blueprint: one script, three
  shapes (akuma/herd, linux/systemd, lima/systemd).
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
