# What has actually run

Evidence, not intentions. Every number here came off a real run on 2026-09-21;
nothing in this file is projected.

## The stack, as built

```
crates/
  miot-primitives    types, zero dependencies          no_std
  miot-tasks         the lifecycle, pure state machine no_std   38 tests
  pallet-litter      FRAME wrapper                     no_std   16 tests
  miot-runtime       construct_runtime!, 90 lines      no_std    2 tests
  miot-llm           Ollama provider, native tool calls std
  miot-sim           the binary: scripted + --live      std
```

`cargo test --workspace` → **61 tests**, host-native, no Docker.

## Native FRAME: what dropping wasm bought

The runtime is executed as compiled Rust — no wasm blob, no `sc-executor`, no
state trie. The Dockerfile is the receipt:

```dockerfile
RUN apt-get install -y build-essential clang pkg-config
```

| | measured |
|---|---|
| Docker build, cold, Linux | **1 m 39 s** |
| Binary | **1.6 MB** |
| Image | 138 MB (debian-slim + binary) |
| apt packages | **3** |

A `sc-service` node image is a 20–40 minute build and hundreds of MB, because
it drags wasmtime/cranelift, ParityDB or RocksDB, and libp2p. None of those are
here. That is also why the Akuma DB blocker stops applying: nothing in this
binary wants `mmap`'d sparse files or `PROT_EXEC`.

## Scripted run — the recovery path

`cargo run -p miot-sim`. Cats are scripted policies; every state transition is
the real pallet. `kuro` is deliberately dead and never answers.

```
   2   mimi  split t1 → tama, kuro
   3   tama  claimed t1.1
   4   tama  t1.1: 214 tests green
 102         t1.2 requeued (Unclaimed)      ← offer budget draining
 202         t1.2 requeued (Unclaimed)
 302         t1.2 requeued (Unclaimed)
 302         chain → mimi: [reassign-needed]
 302   mimi  re-homed t1.2 → sora
 303   sora  claimed t1.2
 306   mimi  committed the artifact for t1
```

The chain detects the stuck cat and asks for a re-home **by name**, unprompted,
with nobody submitting anything. That is Law I doing its job.

## Live run — real models

Same protocol, cats replaced by model turns. Identical on macOS and in Docker.

**macOS host**, `gemma4-yolo-4b:latest`:

> \# where is this litter running?
> The litter is running on a Darwin operating system, with kernel version
> 24.6.0, on an arm64 architecture, and it is not running within a container.

**Docker/Linux**, `qwen3:4b`:

> \# Where is this litter running?
> This litter runs on a Linux kernel 6.12.54-linuxkit, aarch64 architecture,
> within a Docker container.

Same binary, same protocol, and the cats correctly report their own different
environment. The artifact is read back out of chain state, not out of a log.

### Model comparison, identical protocol outcomes

| turn | gemma4-yolo-4b (8B) | qwen3:4b |
|---|---|---|
| `TaskPlan` | 230 tok / 6.3 s | 1545 tok / 26.4 s |
| `claim` | 96 tok / 3.1 s | 899 tok / 15.2 s |
| `clear` | 130 tok / 4.2 s | **1690 tok / 31.4 s** |
| `artifact` | 416 tok / 9.9 s | 1194 tok / 23.9 s |
| **whole task** | **~40 s** | **~160 s** |

4× the wall clock, 5–13× the tokens, same result. `clear` is the worst case —
1690 tokens to decide "yes, accept this."

### Zero malformed calls

Across every live run: every `TaskUpdate`/`TaskPlan` parsed, every task id
well-formed (`t1`, `t1.1`), every status a valid enum value, no refusals. The
litter's finding — *a small model picks a value more reliably than it picks
among similar tool names* — held on the first try with no prompt iteration.

### Timers are ~50× too conservative

`claim_window` 100 blocks and `lease` 150 were sized for the litter's measured
120–200 s turn, i.e. 20–34 blocks. A turn here is **3–31 s**, under one block.
Nothing is broken by it — the scripted run's 306 blocks were almost entirely
kuro's offer budget draining — but a live litter should use single-digit block
windows.

## The four-server swarm

`overlays/local/llama-swarm.sh` runs one `llama-server` per cat on 8081–8084,
all qwen3:4b. Since llama.cpp mmaps the GGUF, four instances share the weight
pages — the per-instance cost is KV cache, not 2.3 GB each. (Observed
directly: the first server to start took minutes to load cold; restarting it
after the other three were up took **4 seconds**.)

### Threads: one is enough, and that is measured

| config | 4 concurrent requests |
|---|---|
| `-t 3` — 12 threads on 12 physical cores | 16.54 s / 16.55 s |
| `-t 1` — 4 threads | 16.28 s / 16.61 s |

Identical. With `-ngl 99` the GPU does the matmuls and CPU threads only handle
sampling, so one thread per server is enough and leaves 8 cores free.
`llama-server` otherwise defaults to ~8 threads regardless of how many copies
are running, which puts 32 threads on a 12-core box for no gain.

### Four servers on one machine buy no throughput

The number that matters:

| | latency |
|---|---|
| one server, alone | **4.1 s** |
| four servers, concurrently | **16.5 s** |

Exactly 4×. They **fully serialize on the single GPU**. Running four servers on
one Mac buys independent endpoints and per-cat model choice — which is the
deployment shape we want to develop against — and *not* parallelism. Real
concurrency needs separate machines.

This also means the earlier single-ollama timings are not directly comparable:
a lone cat on a quiet machine is fast; four cats sharing a GPU are not.

## Findings this produced

1. **`gemma3:4b` cannot be a cat.** Ollama refuses the request outright:
   `400: "gemma3:4b does not support tools"`. Not a prompt problem. Tool
   support is a hard gate on model choice.
2. **The chain caught a hole in the agent, not the other way round.** The live
   loop had `Directive::ReassignNeeded => continue` — it dropped the directive
   on the floor, and a parent stalled forever while the *protocol* had done
   everything right. Now wired, plus a `TaskReassign` tool for the leader.
   Still unexercised against a live model, because with every cat on a working
   model nothing gets stuck.
3. **Reasoning models do not loop here, and it is structural.** Every prompt
   names the verb to call, so there is no "what should I do next?" to spiral
   on — the search space is the argument, not the action. And turns are
   stateless (a fresh two-message conversation each time), so there is no
   accumulating context to re-litigate. What is *not* guarded: a model that
   burns its budget and returns no tool call. That path exists, self-heals via
   re-offer, and costs a full nag cycle. A `num_predict` cap would bound it.
4. **Heterogeneous personas are worth having.** Lifted from
   `meow/litter/personas/`. Four identical cats give four identical answers and
   the leader learns nothing from having asked twice.
5. **A stateless turn means the chain is the only memory — and the prompts have
   to use it.** Two consecutive live runs on a mounted document produced
   confident, well-formed reports about *the wrong topic*. Causes, both real:
   - a leftover prompt from an earlier experiment still told workers to report
     "what you found about the host you run on"; with a different document
     mounted the cats resolved "the host" to something in *that* text;
   - more importantly, **the parent question never reached the workers or the
     artifact stage**. Each stage saw only its immediate input: a worker got
     its sub-task text, and the leader synthesised the report from sub-task
     *results* alone. The question was in chain storage the entire time and
     nothing read it.

   The fix is to refetch the parent's text per effect and carry it into the
   assignment, the nudge, the clearance and the artifact prompt. Worth stating
   as a rule: **a turn carries no history, so anything the model must not lose
   has to be re-read from the chain and restated every single time.**
6. **The protocol was not at fault either time.** Both failures produced a
   clean parent → plan → claim → done → clear → artifact run with zero refused
   calls and zero malformed ids. The litter did exactly what it was told; it
   was told the wrong thing. That is worth noticing, because it is the failure
   mode a protocol cannot catch for you.

## Chat: talking to the litter

`miot-sim --chat` sends a line as a root-signed `say` extrinsic; every cat it
wakes takes a turn and replies with `SendMessage`, which is another extrinsic.
Everything on screen went through the chain.

```
root ▸ introduce yourselves

 mimi  Hello, I'm Mimi, the leader of Akuma Miot. We are a coordinated team of
       AI agents that split tasks…                             (3044 tok, 173s)
 tama  I am Tama, a worker… I execute tasks as extrinsics.        (576 tok, 35s)
 kuro  I am Kuro… I deduce actions from evidence to ensure tasks are executed
       with precision and skepticism.                            (484 tok, 28s)
 sora  Hello, I am Sora… I pick up dropped work and redo tasks from evidence.
                                                                 (803 tok, 45s)
```

The personas are doing real work here: kuro is skeptical, sora describes itself
as the one who picks up dropped work. Four identical cats would have produced
four identical sentences.

**The spread is the notable number.** mimi spent 3044 tokens and 173 s on the
same question tama answered in 576 tokens and 35 s — 5× the tokens, 5× the wall
clock, for a sentence. Partly cold-start (mimi went first), partly that a
reasoning model given a vague prompt reasons about the vagueness.

### Waking is decided by the protocol, not the CLI

`Effect::wakes()` is the only rule the chat loop consults:

| | wakes |
|---|---|
| addressed to one cat (`@tama`) | yes |
| from the operator, to the litter | yes — root speaking is an instruction |
| a peer talking to the litter at large | **no** — else one remark becomes four turns |

## Honest gaps

- **No signatures yet.** The live loop calls `RuntimeOrigin::signed(x)`
  directly, as the tests do. `ensure_signed` is genuinely exercised; nothing
  verifies a signature. Until `UncheckedExtrinsic` + real keys land, "root is
  the key to the cat house" is notional.
- **No networking, no block production, no persistence.** One in-process state
  machine stepping blocks in a loop. See `MAPPING_REPORT.md` §6.
- **Two live debate runs on a mounted document did not finish.** The first two
  answered the wrong question (see finding 5). The third, with the question
  carried into every prompt, was still running at 20 minutes and was killed: a
  12 KB README in every system prompt is ~3 000 tokens of prompt processing per
  turn, times four cats sharing one GPU. Mounting a document is cheap to
  implement and **not** cheap to run.
- **The cats are told which verb to use.** That is by design and it is what
  makes a 4B model viable — but it means the protocol has not yet been tested
  against a model choosing *wrongly* rather than choosing badly.
