# The current home litter — topology, 2026-09-22

What's actually running right now, across one operator's machines and every
virtualization boundary between them. Not aspirational — every node below was
verified converged as of this date (see HANDOFF.md item 5 and its "real fork,
real reconciliation" writeup for how each replica was proven, not just
started) — **except `node4`, which is intermittent, not durably up, and
`node5`, which runs but cannot survive a restart; see their own sections
below before assuming either is currently running.**

```
macOS host (192.168.1.203)
│
├── Docker ──────────────────────────────────────────────────────────
│     node    (PRIMARY)  :9944  ── ticks blocks, accepts /submit
│     mimi / tama / sora (cats) ── all point at node
│
├── 4x llama-server (host, ports 8081-8084) ── never containerized;
│     Docker Desktop on macOS has no GPU passthrough
│
├── Lima VM `fc` (Linux, aarch64, vz + nested virt) ─────────────────
│     node3  (replica)          ── fc's own Linux userspace,
│                                  cross-compiled aarch64-unknown-
│                                  linux-musl, same binary shape as
│                                  dist/miot
│     kuro   (cat)               ── same placement, proves a cat can
│                                  run somewhere that isn't a
│                                  container
│     │
│     └── Firecracker guest `akuma-guest` (nested one level deeper) ──
│           Not Linux — the actual Akuma kernel (../akuma), booted via
│           overlays/devbox-firecracker/{host,guest}-setup.sh + build.sh
│           + run.sh. Reachable at 10.0.2.15 from inside fc.
│
│           node4 (replica) ── same aarch64-unknown-linux-musl binary
│                               as node3, running as a herd service.
│                               INTERMITTENT — see below.
│
├── ryzen (192.168.1.126, Pop!_OS, x86_64 — plain Linux) ────────────
│     node2  (replica)  :9944  ── systemd `miot-node2.service`, bare
│                                 /root/miot/bin/miot (static
│                                 x86_64-unknown-linux-musl, scp'd
│                                 over), MIOT_PEER points back at
│                                 mac's docker `node`. Replaced the
│                                 docker `node2` container same day.
│
└── akuma (192.168.1.123:2222, the real Akuma kernel on hardware) ──
      node5  (PRIMARY of its own chain — fresh genesis, not migrated
              into mac's log) :9944 ── bare /root/miot/bin/miot, same
              x86_64 musl binary, nohup'd with no supervisor. Runs,
              but is NOT durably up — see the storeprobe section.
```

Every replica (`node2`, `node3`, `node4`) pulls from *its own* primary the
same way, over the same HTTP endpoints (`/chain/head`, `/chain/blocks`,
`/chain/checkpoint`) — nothing about the protocol changes based on what's
running underneath it. `node2` and `node3` follow mac's docker `node`;
`node4` follows it too, at `192.168.5.2:9944`: out through `akuma-guest`'s
own NAT (`fc`'s tap0), through `fc`'s uplink, through Lima's gateway, to the
docker port `node` publishes on the host. Four hops, one HTTP GET. `node5`
has no peer — it is a primary of a *separate* chain with its own genesis
(same genesis hash, same accounts, but a disjoint log that starts today).

## `node2` on ryzen — plain Linux, plain plumbing

Moved off docker onto `ryzen` (192.168.1.126, Pop!_OS, x86_64) on
2026-09-22, mostly to exercise the x86_64-musl build path before trusting
it on akuma. Cross-compiled on the mac with
`x86_64-linux-musl-gcc` (brew musl-cross) + rustup target
`x86_64-unknown-linux-musl` — the x86_64 twin of what `build-akuma.sh`
already does for aarch64 — and `scp`'d over: ryzen is normal Linux, so
scp/rsync/https all just work, which after the Akuma transfer dance feels
like a superpower. Runs as a systemd unit (`miot-node2.service`,
`Restart=on-failure`) with `MIOT_ROLE=replica MIOT_PEER=http://192.168.1.203:9944
MIOT_DB=/root/miot/db/miot.db`; adopted the peer's compaction checkpoint at
block 1061 on first connect, and was verified two ways: `/tasks` and
`/events` diffed byte-identical against the primary, and a live `--say`
signed against the primary landed in the replica's event log within one
sync interval. The docker `node2` service was stopped, removed, and deleted
from `docker-compose.yml` (volume dropped too) — one replica identity, one
process, not two.

## `node5` on the real akuma host — runs, but the store has a wall

First time anything from this repo touched the physical `akuma` box
(`ssh akuma`, port 2222 — an x86_64 Akuma-kernel host, not the Firecracker
guest and not a Lima VM). `dist/storeprobe` finally ran somewhere, plus a
bare-syscall `mmapprobe` built on the spot to pin the finding down. The
facts, in order:

- **Transfer**: no scp (no SFTP subsystem, same as the guest), but HTTP out
  works — busybox `wget` pulling from a `python3 -m http.server` on the mac
  over the LAN moved both binaries fine. (Watch for a stale `http.server`
  already squatting on the port — 404s that look like a wrong path but
  aren't.)
- **`storeprobe` stages 1–5 pass**: fresh ParityDB open, 256 appends, read
  back, compact, rewind — on real hardware, not nested in anything.
- **Stage 6 fails**: reopen of the existing store dies with
  `Function not implemented (os error 38)`.
- **`mmapprobe`** (crates/miot-store/src/bin/mmapprobe.rs, raw `mmap`
  syscalls, 4 pages): anonymous RW ok, file `MAP_PRIVATE` RO ok, file
  `MAP_SHARED` RO ok, file `MAP_SHARED` RW — **segfault** (exit 139), not
  even a clean errno.
- **Root cause, in the kernel's own words** (../akuma
  `amd64/src/mm.rs`, the `sys_mmap` doc comment): a **writable
  `MAP_SHARED` file mapping is refused** — the one deliberate gap in
  akuma's mmap surface ("writes would need a coherent path back to the
  file"). ParityDB's `MmapMut` is exactly that shape, needed exactly at
  reopen/growth.

So a **fresh** `miot node` boots and serves on real akuma — real tokio
workers (`ps` shows `{tokio-rt-worker}` threads), HTTP up, blocks ticking,
a signed `--say` landing and persisting (DB grew, head advanced). But the
wall is close behind:

- **Restart over an existing DB is fatal**: the reopened ParityDB hits the
  same `os error 38` and the node exits. Fresh DB every boot, or no boot.
- **Catch-up sync wedges**: pointed at mac's primary as a replica with a
  fresh DB, `node5` adopted the checkpoint at block 1061, established its
  peer connection — and then froze for 90+ observed seconds with all
  threads reported `R` but **zero accumulated CPU time**, HTTP never bound,
  DB size stuck at 13.5K. Consistent with the writable-`MAP_SHARED` path
  wedging in the kernel rather than returning; not chased further, same
  call as node4's panic. (An akuma-kernel bug, if it is one, belongs in
  ../akuma, not here.)
- **node4's exact panic** (`parity-db index.rs:237 slice range`) did **not**
  reproduce here — the host hits its own wall earlier, in mmap, before any
  index growth question can come up. Useful asymmetry: same kernel, two
  environments, two different ParityDB failure modes, one shared suspect.

**Verdict: `node5` is up and reachable (`http://192.168.1.123:9944`) but
not durably so** — nohup'd, no supervisor (the box has no herd here), and
one restart away from the mmap wall. "Durable node on real Akuma hardware"
needs either akuma gaining writable `MAP_SHARED` file mappings, or
`miot-store` growing a plain-file-I/O fallback behind a feature flag so
ParityDB never mmaps. Neither is small; both are now precisely scoped,
which is what the probe was for.

## What `node4` actually proves

Not "Akuma can boot" — `../akuma`'s own docs already established that
(`docs/runbooks/run-on-firecracker.md`: SSH, DHCP, the boot suite, all
verified before this). What's new here: a real Rust binary with a
**multi-threaded tokio runtime**, an **axum HTTP server**, **ParityDB**
(sparse mmap'd files — the specific thing `dist/storeprobe` exists to test
and had never actually been run), and a **reqwest HTTP client**, all running
together, unmodified, on Akuma's userspace. Confirmed live: `ps` on
`akuma-guest` shows a real `{tokio-rt-worker}` thread; `curl
localhost:9944/chain/head` answers from inside the guest; a task submitted
against the real docker `node` shows up in `node4`'s `/tasks` a few seconds
later.

**Caveat, stated plainly**: this is one boot, one binary, one narrow set of
syscalls actually exercised (thread spawn, TCP listen/accept/connect, mmap'd
file I/O, basic file read/write). It is evidence the surface `miot`
needs works *on this build*, not a general claim about Akuma — the same
caution `HANDOFF.md` already applies to every Akuma claim applies here too.

**Known limitation, found live, not chased to root cause**: ParityDB panics
on `akuma-guest` once its index needs to grow past some threshold —
`thread 'main' panicked ... parity-db-0.5.6/src/index.rs:237: range start
index 512 out of range for slice of length 1`. Reproduced twice: once
syncing ~800 blocks of history from scratch, once tailing normally after
starting from a pre-populated store copied over from `fc` (which only
delayed it, not avoided it) — so it's about index growth itself, not the
catch-up path or the `/tmp` vs `/var/lib` question investigated along the
way (ruled out: both are the same `ext2` root, `mount` confirms no separate
tmpfs). `node4` is therefore **intermittent, not durably up** — it works
for a while after a fresh DB, then crashes once enough new blocks
accumulate, and `restart=true`/`max_retries=0` in its herd config does not
bring it back (crashes again on the regrown index). Not investigated
further per an explicit call to stop chasing it; a real fix would mean
either bounding ParityDB's index growth (compaction running often enough
that it never needs to grow this far) or finding out what Akuma's `ext2`/
mmap implementation actually does differently once an index page beyond
the first needs allocating.

## Naming, so it doesn't become a problem

`fc` is the Lima VM's own name (Linux). `akuma-guest` is the Firecracker
guest nested *inside* `fc`, running the real Akuma kernel — a different
machine, one level deeper, not another name for the same thing. `akuma` (the
ssh alias) is a *third* thing: the physical x86_64 Akuma-kernel box on the
LAN, where `node5` runs. Same kernel as `akuma-guest`, different machine —
`node4`'s and `node5`'s failures are not interchangeable evidence. Don't
conflate any of them; a previous draft of this session's notes briefly did,
and it was confusing enough to call out here on purpose.

## Where this came from

- HANDOFF.md item 5 — the primary/replica replication protocol every node
  above speaks.
- `docs/references/storage.md` — the wire mechanism (`/chain/*` endpoints)
  and the compaction/checkpoint machinery `node4` adopted on its first sync.
- `../akuma/docs/runbooks/run-on-firecracker.md` and
  `overlays/devbox-firecracker/README.md` — how `akuma-guest` actually boots;
  read there before re-deriving any of the Firecracker/tap/DHCP setup.
