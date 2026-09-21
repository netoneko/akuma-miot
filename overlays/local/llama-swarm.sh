#!/usr/bin/env bash
# Four llama-servers, one per cat.
#
# WHY one each: four cats against a single server serialize their turns, and
# the entire design rests on turns running concurrently while the chain ticks
# straight through them. A swarm that queues is not a swarm — it is one cat
# taking four times as long.
#
# The model is mmap'd, so four instances of the same GGUF share their weight
# pages. The per-instance cost is the KV cache, not 2.3 GB each.
#
#   ./llama-swarm.sh up      start 4 servers on 8081-8084
#   ./llama-swarm.sh down    stop them
#   ./llama-swarm.sh status
set -euo pipefail

MODEL="${MIOT_GGUF:-$HOME/.ollama/models/blobs/sha256-3e4cb14174460404e7a233e531675303b2fbf7749c02f91864fe311ab6344e4f}"
PORTS=(8081 8082 8083 8084)
CATS=(mimi tama kuro sora)
CTX="${MIOT_CTX:-8192}"
# Threads PER SERVER. llama-server defaults to ~8 regardless of how many of
# itself are running, so four servers take ~32 threads on a 12-core box and
# spend the difference context-switching. Divide the machine instead.
CORES="$(sysctl -n hw.physicalcpu 2>/dev/null || nproc)"
THREADS="${MIOT_THREADS:-$(( CORES / 4 > 0 ? CORES / 4 : 1 ))}"
NGL="${MIOT_NGL:-99}"
RUN="${TMPDIR:-/tmp}/miot-llama"

up() {
  [ -f "$MODEL" ] || { echo "no GGUF at $MODEL (set MIOT_GGUF)" >&2; exit 1; }
  mkdir -p "$RUN"
  for i in "${!PORTS[@]}"; do
    p="${PORTS[$i]}"; c="${CATS[$i]}"
    if curl -sf --max-time 1 "http://127.0.0.1:$p/health" >/dev/null 2>&1; then
      echo "  $c :$p already up"; continue
    fi
    # --jinja is REQUIRED for tool calling: without it llama-server serves the
    # plain chat template and silently never emits a tool_calls field.
    nohup llama-server -m "$MODEL" --host 127.0.0.1 --port "$p" \
      -c "$CTX" -ngl "$NGL" --jinja --parallel 1 -t "$THREADS" \
      > "$RUN/$c.log" 2>&1 &
    echo $! > "$RUN/$c.pid"
    echo "  $c :$p starting (pid $!, -t $THREADS)"
  done
  echo "waiting for models to load..."
  for p in "${PORTS[@]}"; do
    for _ in $(seq 1 120); do
      curl -sf --max-time 1 "http://127.0.0.1:$p/health" >/dev/null 2>&1 && break
      sleep 2
    done
  done
  status
}

down() {
  for c in "${CATS[@]}"; do
    [ -f "$RUN/$c.pid" ] || continue
    kill "$(cat "$RUN/$c.pid")" 2>/dev/null && echo "  $c stopped"
    rm -f "$RUN/$c.pid"
  done
}

status() {
  for i in "${!PORTS[@]}"; do
    p="${PORTS[$i]}"; c="${CATS[$i]}"
    if curl -sf --max-time 2 "http://127.0.0.1:$p/health" >/dev/null 2>&1; then
      echo "  $c :$p  ready"
    else
      echo "  $c :$p  DOWN"
    fi
  done
}

spec() {
  out=""
  for i in "${!PORTS[@]}"; do
    out="$out${out:+,}${CATS[$i]}=http://127.0.0.1:${PORTS[$i]}"
  done
  echo "$out"
}

case "${1:-status}" in
  up) up ;;
  down) down ;;
  status) status ;;
  spec) spec ;;
  *) echo "usage: $0 {up|down|status|spec}" >&2; exit 1 ;;
esac
