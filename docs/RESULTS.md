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

## Honest gaps

- **No signatures yet.** The live loop calls `RuntimeOrigin::signed(x)`
  directly, as the tests do. `ensure_signed` is genuinely exercised; nothing
  verifies a signature. Until `UncheckedExtrinsic` + real keys land, "root is
  the key to the cat house" is notional.
- **No networking, no block production, no persistence.** One in-process state
  machine stepping blocks in a loop. See `MAPPING_REPORT.md` §6.
- **The cats are told which verb to use.** That is by design and it is what
  makes a 4B model viable — but it means the protocol has not yet been tested
  against a model choosing *wrongly* rather than choosing badly.
