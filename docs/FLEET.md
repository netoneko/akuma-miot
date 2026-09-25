# Fleet — hosts and model split

## As running, 2026-09-25

`overlays/deploy/deploy.py`'s `AGENTS` table is the source. tama and kuro
were checked live when this was written (their `kot.env` and the startup
line's `llm=`); mimi's row is the llama-server actually running on :8084;
meow, sora and the AWS pair are from `deploy.py`/HANDOFF, not re-checked.

| cat | agent | host | model | served by |
|---|---|---|---|---|
| meow | `dumpster-akuma-amd64` | akuma (metal) | `glm-5.3` | z.ai coding plan (`--glm`) |
| tama | `ryzen-linux-amd64` | ryzen | `glm-5.3` | z.ai coding plan (`--glm`) — was Qwen3-4B on ryzen's llama-server until 2026-09-25 |
| kuro | `mac-linux-aarch64` | Lima `fc` on the mac | `gemma4-yolo-4b` | **Ollama** on the mac, `192.168.5.2:11434` — was `qwen3:4b` on llama-server :8083 until 2026-09-25 |
| sora | `ryzen-akuma-amd64` | Firecracker guest on ryzen | Qwen3-4B-Instruct-2507 Q4_K_M | ryzen's shared llama-server (:8081, via the `192.168.1.49:8082` proxy socket) |
| mimi | `mac-akuma-aarch64` | akuma-guest nested in `fc` | `qwen3:4b` (the Ollama blob `sha256-3e4cb…`, 2.5 GB) | llama-server on the mac, :8084 |
| yuki, shiro | AWS | `kot.akuma.sh` | OpenRouter | out of credit as of 2026-09-25 (HANDOFF, "Outages") |

What changed on 2026-09-25 and why:

- **tama moved to GLM** so a cat with a real toolchain next to it can try
  building and changing `kot` itself. ryzen now has rustup (stable, root) and
  a checkout at `/root/src/akuma-miot`; the build runs as a transient
  systemd unit (`kot-build`, `MemoryMax=6G`, no swap, `jobs = 4` in
  `/root/.cargo/config.toml`) so a codegen spike kills the build, not the
  laptop — the box already ran out of memory once that day. A native glibc
  build, not `build.sh`'s static musl one. Disk is the constraint: 14 GB free
  before the first build. ryzen's llama-server stays up for sora alone.
- **kuro moved to `gemma4-yolo-4b`, on Ollama** — Kirill's call, to see how
  it does at reviews. This is the one exception to "never ollama in the
  fleet": Ollama's Gemma 4 blob doesn't load in llama-server
  (`done_getting_tensors: wrong number of tensors; expected 2131, got 720`
  — Ollama's own tensor layout), and it's the largest Gemma 4 on the mac's
  disk. `gemma4-yolo-4b` is `gemma4:e4b` plus `num_ctx 131072` and
  temperature 1 / top-k 64 / top-p 0.95; 14 GB resident. Ollama is started
  by `../yolo/run-ollama.sh` (`OLLAMA_KEEP_ALIVE=-1`, flash attention, q8
  KV cache) — a bare `ollama serve` unloads the model after 5 minutes idle
  and every wake pays the reload. Nothing restarts it after a mac reboot.
  The 26B-A4B `../yolo/Modelfile` names isn't pulled; the mac had 10 GB free.
  kuro's old llama-server on :8083 was stopped.

## The plan of 2026-09-21 (history)

Kept for the reasoning; the table above is what runs. Three boxes,
`llama-server` (not Ollama — native GGUF, OpenAI-compatible endpoint,
`--jinja` for tool-call parsing), one model per cat.

### Hosts

| host | RAM | free disk (2026-09-21) | GPU | role |
|---|---|---|---|---|
| **mac** (this box) | 48 GB unified | **15 GB** — nearly full, no external volume | Metal | leader + one worker, once disk is sorted |
| **ryzen** | 13 GB | 24 GB | Radeon 780M iGPU, shares system RAM (no dedicated VRAM carve-out seen) | two workers on **one shared llama-server** (2 slots × 8192, `MemoryMax=7G`) since 2026-09-25 — two servers ran it out of memory; swap is zram, i.e. RAM. + a GLM experiment |
| **akuma** (trashcan) | 16 GB physical, **~10 GB planned budget** — reserved for the Rust toolchain (`rustc`'s LLVM codegen spikes hard, multiplied by `cargo`'s parallelism, during kernel builds) | — | — | **not local inference.** Calls z.ai's GLM API for feature-writing / kernel-compile work. |

### Cats → host → model

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

### Status, 2026-09-21

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
  `Akuma akuma 0.0.8 x86_64 GNU/Linux` — Kirill's own kernel, not stock
  Linux. `whoami` fails (`unknown uid 0`, no `/etc/passwd`) — the userland
  is minimal. **Update 2026-09-22, late:** the syscall surface a Rust
  network-plus-database service needs is now *verified on the metal* — a
  real `miot` node runs there durably (tokio workers, axum HTTP, ParityDB
  surviving restart — `docs/TOPOLOGY.md` `node5`), after three kernel fixes
  (writable `MAP_SHARED` mmap, ext2 truncate-extend, `fadvise64`). `mmap`
  coherence is write-back-shaped, not page-cache-shaped: fine for a
  single-process DB owner like the node, the caveat a second mapper of the
  same file would hit. Local inference on akuma is still a someday-maybe
  for other reasons (RAM budget, GLM-over-API already works there).
- **`--jinja` tool-call parsing with Qwen3/GLM on `llama-server` is unverified
  live.** RESULTS.md's zero-malformed-calls finding is against Ollama +
  gemma4-yolo-4b/qwen3:4b, not against `llama-server`'s parser.
