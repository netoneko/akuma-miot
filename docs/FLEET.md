# Fleet — hosts and model split

**2026-09-21.** Three boxes, `llama-server` (not Ollama — native GGUF, OpenAI-
compatible endpoint, `--jinja` for tool-call parsing), one model per cat.

## Hosts

| host | RAM | free disk (2026-09-21) | GPU | role |
|---|---|---|---|---|
| **mac** (this box) | 48 GB unified | **15 GB** — nearly full, no external volume | Metal | leader + one worker, once disk is sorted |
| **ryzen** | 13 GB | 24 GB | Radeon 780M iGPU, shares system RAM (no dedicated VRAM carve-out seen) | two workers, + a GLM experiment |
| **akuma** (trashcan) | 16 GB physical, **~10 GB planned budget** — reserved for the Rust toolchain (`rustc`'s LLVM codegen spikes hard, multiplied by `cargo`'s parallelism, during kernel builds) | — | — | **not local inference.** Calls z.ai's GLM API for feature-writing / kernel-compile work. |

## Cats → host → model

Mimi is the leader (`TaskPlan`/`TaskReassign`); Kuro, Sora, Tama are workers
(`TaskUpdate` only). Sized asymmetrically — the leader gets the bigger model
because splitting/judging work needs more reasoning than picking a status
value.

| cat | role | host | model | quant | file size |
|---|---|---|---|---|---|
| Mimi | leader | mac (deferred) | Qwen3-14B-Instruct | Q4_K_M | ~9 GB |
| Tama | worker | mac (deferred) | Qwen3-4B-Instruct-2507 | Q4_K_M | 2.33 GB |
| Kuro | worker | ryzen | Qwen3-4B-Instruct-2507 | Q4_K_M | 2.33 GB |
| Sora | worker | ryzen | Qwen3-4B-Instruct-2507 | Q4_K_M | 2.33 GB |

Kuro and Sora share the same weights file, run as two separate `llama-server`
processes (or one process with `--parallel 2`) so they can act concurrently.

GGUF sources (verified against the HF API before download, not guessed):

- `unsloth/Qwen3-4B-Instruct-2507-GGUF` — `Qwen3-4B-Instruct-2507-Q4_K_M.gguf`
- `bartowski/glm-4-9b-chat-GGUF` — `glm-4-9b-chat-Q4_K_M.gguf` (6.25 GB) —
  downloaded to ryzen as an experiment per Kirill: "who knows how it's gonna
  perform." Not assigned to a cat. Note below on whether it coexists in RAM.

## Status, 2026-09-21

- **Downloading now, ryzen only**: `~/models/gguf/` — the Qwen3-4B Q4_K_M
  (Kuro/Sora) and the GLM-4-9B-chat Q4_K_M experiment.
- **Mac side deferred.** 15 GB free can't safely hold Mimi's 14B (~9 GB) +
  Tama's 4B (~2.33 GB) — would leave under 4 GB free. Blocked on Kirill
  freeing disk (or choosing a smaller quant for Mimi); revisit once resolved.
- `miot-llm`'s `Ollama` struct (`crates/miot-llm/src/lib.rs`) still only
  speaks Ollama's `/api/chat` shape. A `llama-server` provider (OpenAI-
  compatible `/v1/chat/completions`, different `tool_calls`/`usage` envelope)
  is not written yet — needed before any of this is wired into `miot`.

## Honest gaps

- **Ryzen RAM is tight if the GLM experiment runs alongside the workers.**
  2× Qwen3-4B (~5 GB) + GLM-4-9B (~5.8 GB) ≈ 10.8 GB against ~9.6 GB currently
  free (13 GB total, Firecracker and other services already resident). Not
  measured live — the download doesn't confirm it runs concurrently.
- **z.ai reachability from akuma across reboot is unimplemented.** Kirill
  wants akuma to reread history and resume after the reboots its own kernel
  builds cause. No mechanism for that exists yet — this doc only covers the
  local inference split, not that persistence design.
- **akuma isn't a normal Linux userland — checked, not assumed.** `uname -a`:
  `Akuma akuma 0.0.8 d565985c-release-smp-shared x86_64 GNU/Linux`. This is
  Kirill's own kernel, not stock Linux. `whoami` fails (`unknown uid 0`, no
  `/etc/passwd`) — the userland is minimal. Whether it has the pthread/mmap/
  socket surface `llama-server` needs is **unverified**, on top of the ~10 GB
  budget already being spoken for by the Rust toolchain during kernel builds.
  Another reason local inference on akuma stays a someday-maybe, not that RAM
  size was ever the only blocker.
- **`--jinja` tool-call parsing with Qwen3/GLM on `llama-server` is unverified
  live.** RESULTS.md's zero-malformed-calls finding is against Ollama +
  gemma4-yolo-4b/qwen3:4b, not against `llama-server`'s parser.
