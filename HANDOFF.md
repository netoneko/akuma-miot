# Handoff

State of Akuma Miot as of 2026-09-21. What runs, what doesn't, what to do next,
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
cargo test --workspace                 # 91 tests, host-native, no docker

# models on the HOST (Metal). Docker on macOS has no GPU passthrough.
overlays/local/llama-swarm.sh up       # 4 llama-servers, ports 8081-8084

docker compose -f overlays/local/docker-compose.yml up -d   # node + 4 cats
curl -s localhost:9944/head
curl -s localhost:9944/meta                          # genesis hash + spec/tx version
# there is no more unauthenticated /call — every act is a signed extrinsic.
# --rpc is the same `miot` binary (shipped as dist/miot), pointed at a real
# node instead of driving an in-process chain:
cargo run -p miot -- --rpc http://localhost:9944 --identity-seed 1 \
  --open "your question here"
docker compose -f overlays/local/docker-compose.yml logs -f
curl -s localhost:9944/artifact/t1 | python3 -m json.tool
```

Single-process simulation (no networking, much faster to iterate on):

```bash
cargo run -p miot                  # scripted, shows the recovery path
cargo run -p miot -- --live --models "$(overlays/local/llama-swarm.sh spec)"
cargo run -p miot -- --chat --models "$(overlays/local/llama-swarm.sh spec)"
```

Akuma-shippable binaries:

```bash
overlays/local/build-akuma.sh          # dist/miot (5.1 MB), dist/storeprobe (0.8 MB)
```

---

## The crates

| crate | what | tests |
|---|---|---|
| `miot-primitives` | vocabulary: `TaskId`, `Act`, `Effect`, `Limits`, `Timers`. `no_std`. | 5 |
| `miot-tasks` | **the lifecycle, as a pure state machine.** No clock, no I/O. | 38 |
| `pallet-litter` | thin FRAME wrapper: `ensure_signed` → load → apply → store → emit | 16 |
| `miot-runtime` | `construct_runtime!`; `AccountId32`/`MultiSignature`, real `UncheckedExtrinsic` + `Executive`, **executed natively — no wasm** | 2 |
| `miot-store` | block log on ParityDB, compaction-boundary rewind, leader-wins | 14 |
| `miot-keys` | ed25519 identity: seeds for cats, the operator's SSH *public* key → `AccountId32`, hex wire encoding | 14 |
| `miot-llm` | provider layer on `genai` (15 providers, GLM included) | — |
| `miot-node` | the chain as a process: HTTP, real block lifecycle (`Executive`) on its own clock, `/submit` verifies before it dispatches | — |
| `miot-cat` | one cat, one container, signs its own extrinsics and talks to the node | — |
| `miot` | one binary (ships as `dist/miot`), four modes: scripted / `--live` / `--chat` (in-process) / `--rpc` (a real node, signed) | — |

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
wired — `miot-node` runs `frame_executive::Executive::initialize_block` /
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
having `miot-node` call `inc_providers` for every account in `MIOT_MEMBERS`
(default `1,2,3,4,5`, the same seed convention as `MIOT_ROSTER`) at genesis —
named `catnip` in `miot-node/src/main.rs`, because an account that hasn't had
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
its own process; four llama-servers; ParityDB (built, tested, still not wired
into the node — see below).

**Not yet real:**

- **`miot-store` is wired to nothing.** Built, tested, probe ships; the node
  keeps state in memory and loses it on restart. **This is now the single
  most important gap** — signed extrinsics closed the previous one.
- **No consensus.** One node owns the chain. `rewind_for_fork` has never run
  against a real disagreement because there is nothing to disagree with.
- **Akuma is untested.** Every claim in the docs about Akuma is inference.
  `dist/storeprobe` exists to replace one of those paragraphs with a fact.
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

---

## Next, in order

1. ~~**Signed extrinsics.**~~ **Done, 2026-09-21.** `AccountId` is
   `AccountId32`; `/call` is gone; `/submit` takes a signed
   `UncheckedExtrinsic` and `miot-node` verifies it for real through
   `frame_executive::Executive` before dispatch. `miot-cat` and `miot --rpc`
   both sign through the shared `miot_runtime::client::sign`.
2. **Wire `miot-store` into `miot-node`.** Persist blocks, replay on start.
   Now the actual most important gap.
3. **Run `dist/storeprobe` on an Akuma guest.** Seven stages, exit status =
   stages completed. Replaces a paragraph of speculation with a fact. Note:
   `overlays/local/build-akuma.sh` cross-compiles for
   `aarch64-unknown-linux-musl`; the real reachable `akuma` host
   (`ssh akuma`, port 2222) answers `x86_64 GNU/Linux` — fix the target before
   this step, or the binary won't run there.
4. **Ship `dist/miot` to Akuma** and run a cat there against a host model.
   Needs a host `llama-server` reachable from that box — today
   `llama-swarm.sh` binds `127.0.0.1` only, so this also needs a deliberate
   decision about exposing an inference port on the LAN, not just a bind-flag
   change.
5. **A second node** — only then does `rewind_for_fork` get exercised.
   Depends on 2: there's nothing to rewind without a real store.

---

## Where to read

- `README.md` — the diagrams and the agent/CLI split
- `docs/MAPPING_REPORT.md` — design of record: the findings table (§1.1), the
  misconception the port introduced (§1.2), what was deliberately not rebuilt
- `docs/RESULTS.md` — **what actually ran, with numbers.** Evidence, not
  intentions. Read this before trusting any claim elsewhere.
- `docs/CLI.md` — `miot-cli` requirements. Scrollback is sacred.
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
