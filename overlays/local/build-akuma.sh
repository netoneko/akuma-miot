#!/usr/bin/env bash
# Build the Akuma-shippable binary: static aarch64 musl, no runtime deps.
#
# Cross-compiles from macOS. Needs:
#   brew install FiloSottile/musl-cross/musl-cross
#   rustup target add aarch64-unknown-linux-musl
#
# Why this works at all: the runtime is FRAME executed *natively*
# (crates/miot-runtime), so there is no wasm executor wanting JIT pages and no
# state trie wanting mmap'd sparse files. Those two are what make a `sc-service`
# node hard to put on a hobby kernel; neither is here.
set -euo pipefail

TARGET=aarch64-unknown-linux-musl
OUT="${1:-dist/miot}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

command -v aarch64-linux-musl-gcc >/dev/null || {
  echo "need aarch64-linux-musl-gcc (brew install FiloSottile/musl-cross/musl-cross)" >&2
  exit 1
}

CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc \
CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc \
cargo build --release -p miot --target "$TARGET"
# The ParityDB probe ships alongside: it answers whether a state database works
# on the guest, which this project spent two documents speculating about.
cargo build --release -p miot-store --bin storeprobe --target "$TARGET"

mkdir -p "$(dirname "$OUT")"
cp "target/$TARGET/release/miot" "$OUT"
cp "target/$TARGET/release/storeprobe" "$(dirname "$OUT")/storeprobe"
aarch64-linux-musl-strip "$OUT" "$(dirname "$OUT")/storeprobe"

echo
echo "  $OUT"
file "$OUT" | sed 's/^/  /'
echo "  $(du -h "$OUT" | cut -f1)"
if aarch64-linux-musl-readelf -d "$OUT" 2>/dev/null | grep -q NEEDED; then
  echo "  WARNING: not static" >&2
else
  echo "  static: no dynamic dependencies"
fi

cat <<'NOTE'

  Also built: dist/storeprobe — run it on the guest to find out whether
  ParityDB works there. Sparse mmap'd files are the open question (97 MB
  apparent, 4 MB allocated on Linux). Exit status = stages completed, of 7.

  Getting it onto a live Akuma guest — the traps, from AKUMA_BUILD.md:
    - NOT scp: this project's sshd has no SFTP subsystem, the client hangs.
    - NOT an ssh exec channel: reproducibly stalls at exactly 1,048,576 bytes,
      and this binary is larger than that.
    - HTTP works. QEMU's SLIRP always reaches the host at 10.0.2.2:

        (cd dist && python3 -m http.server 8765) &
        ssh -p 2222 root@localhost \
          'curl -s -o /usr/local/bin/miot http://10.0.2.2:8765/miot && chmod +x /usr/local/bin/miot'
NOTE
