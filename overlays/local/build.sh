#!/usr/bin/env bash
# Build the shippable binaries: static musl, no runtime deps, for one arch.
#
#   overlays/local/build.sh aarch64   # Lima `fc`, akuma-guest (mac-linux, mac-fc)
#   overlays/local/build.sh x86_64    # ryzen, the akuma metal box, ryzen-fc
#   overlays/local/build.sh all
#
# Output: dist/<arch>/kot (+ storeprobe, mmapprobe — the ParityDB-on-this-
# kernel diagnostics). Needs, from macOS:
#   brew install FiloSottile/musl-cross/musl-cross
#   rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl
#
# One binary per host now: `kot run` is the node and the agent loop, and
# every other `kot` verb is a client. It replaces the old `miot` (node +
# client) and `kot` (agent) pair.
#
# Why this works at all: the runtime is FRAME executed natively
# (crates/miot-runtime) — no wasm executor, no state trie.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

build() {
  local arch="$1" target="$1-unknown-linux-musl" cc="$1-linux-musl-gcc"
  command -v "$cc" >/dev/null || { echo "need $cc (brew install FiloSottile/musl-cross/musl-cross)" >&2; exit 1; }
  local up; up="$(echo "$target" | tr 'a-z-' 'A-Z_')"
  env "CARGO_TARGET_${up}_LINKER=$cc" "CC_${target//-/_}=$cc" \
    cargo build --release --target "$target" -p kot -p miot-store --bins
  local out="dist/$arch"
  mkdir -p "$out"
  for b in kot storeprobe mmapprobe; do
    cp "target/$target/release/$b" "$out/$b"
    "$1-linux-musl-strip" "$out/$b"
  done
  echo "  $out/kot  $(du -h "$out/kot" | cut -f1)  $(file -b "$out/kot" | cut -d, -f1-2)"
  if "$1-linux-musl-readelf" -d "$out/kot" 2>/dev/null | grep -q NEEDED; then
    echo "  WARNING: not static" >&2
  fi
}

case "${1:-}" in
  aarch64|x86_64) build "$1" ;;
  all) build aarch64; build x86_64 ;;
  *) echo "usage: $0 aarch64|x86_64|all" >&2; exit 2 ;;
esac

cat <<'NOTE'

  Getting it onto an Akuma host (the metal box, or a Firecracker guest):
    - NOT scp: no SFTP subsystem, the client hangs.
    - NOT an ssh exec channel: stalls at exactly 1,048,576 bytes.
    - HTTP works: serve dist/<arch> and wget/curl it from the guest.
  overlays/deploy/ has the per-shape scripts that do this.
NOTE
