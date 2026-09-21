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
cargo test --workspace                 # 86 tests, host-native, no docker

# models on the HOST (Metal). Docker on macOS has no GPU passthrough.
overlays/local/llama-swarm.sh up       # 4 llama-servers, ports 8081-8084

docker compose -f overlays/local/docker-compose.yml up -d   # node + 4 cats
curl -s localhost:9944/head
curl -s -X POST localhost:9944/call -H 'content-type: application/json' \
  -d '{"kind":"open","who":1,"text":"your question here"}'
docker compose -f overlays/local/docker-compose.yml logs -f
curl -s localhost:9944/artifact/t1 | python3 -m json.tool
```

Single-process simulation (no networking, much faster to iterate on):

```bash
cargo run -p miot-sim                  # scripted, shows the recovery path
cargo run -p miot-sim -- --live --models "$(overlays/local/llama-swarm.sh spec)"
cargo run -p miot-sim -- --chat --models "$(overlays/local/llama-swarm.sh spec)"
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
| `miot-runtime` | `construct_runtime!`, 90 lines, **executed natively — no wasm** | 2 |
| `miot-store` | block log on ParityDB, compaction-boundary rewind, leader-wins | 14 |
| `miot-keys` | the operator's SSH key as an `AccountId32` | 11 |
| `miot-llm` | provider layer on `genai` (15 providers, GLM included) | — |
| `miot-node` | the chain as a process: HTTP, block loop on its own clock | — |
| `miot-cat` | one cat, one container, talks to the node | — |
| `miot-sim` | single-process harness: scripted / `--live` / `--chat` | — |

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
all of it plus mortality and spec-version binding. What survived is
`account_from_ssh` — reading an OpenSSH public key, which polkadot-sdk does not
do — so root is the operator's existing key, *"no new secret to manage"*.

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
natively; `ensure_signed` deciding authority; artifacts stored and read back
from chain state; five containers on a network with the chain ticking in its
own process; four llama-servers; ParityDB.

**Not yet real:**

- **No signatures on the wire.** `miot-node` takes JSON naming an account and
  trusts it. `miot-keys` proves the property in isolation — a forged sender is
  an invalid signature — but until calls are `UncheckedExtrinsic`, root
  authority is notional. **This is the single most important gap.**
- **`miot-store` is wired to nothing.** Built, tested, probe ships; the node
  keeps state in memory and loses it on restart.
- **No consensus.** One node owns the chain. `rewind_for_fork` has never run
  against a real disagreement because there is nothing to disagree with.
- **Akuma is untested.** Every claim in the docs about Akuma is inference.
  `dist/storeprobe` exists to replace one of those paragraphs with a fact.

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

1. **Signed extrinsics.** `AccountId` from `u64` → `AccountId32`;
   `UncheckedExtrinsic` + `MultiSignature` + `CheckNonce`/`CheckGenesis`/
   `CheckMortality` over the existing `POST /call`. The wire already moves;
   the signature goes on it. This is what makes the whole design mean
   something.
2. **Wire `miot-store` into `miot-node`.** Persist blocks, replay on start.
3. **Run `dist/storeprobe` on an Akuma guest.** Seven stages, exit status =
   stages completed. Replaces a paragraph of speculation with a fact.
4. **Ship `dist/miot` to Akuma** and run a cat there against a host model.
5. **A second node** — only then does `rewind_for_fork` get exercised.

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
