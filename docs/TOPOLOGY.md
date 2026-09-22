# The current home litter — topology, 2026-09-22

What's actually running right now, on one operator's machine, across every
virtualization boundary that machine has. Not aspirational — every node
below was verified converged as of this date (see HANDOFF.md item 5 and its
"real fork, real reconciliation" writeup for how each replica was proven,
not just started) — **except `node4`, which is intermittent, not durably
up; see its own section below before assuming it's currently running.**

```
macOS host
│
├── Docker ──────────────────────────────────────────────────────────
│     node    (PRIMARY)  :9944  ── ticks blocks, accepts /submit
│     node2   (replica)  :9945  ── pulls node over HTTP
│     mimi / tama / sora (cats) ── all point at node
│
├── 4x llama-server (host, ports 8081-8084) ── never containerized;
│     Docker Desktop on macOS has no GPU passthrough
│
└── Lima VM `fc` (Linux, aarch64, vz + nested virt) ─────────────────
      node3  (replica)          ── fc's own Linux userspace,
                                     cross-compiled aarch64-unknown-
                                     linux-musl, same binary shape as
                                     dist/miot
      kuro   (cat)               ── same placement, proves a cat can
                                     run somewhere that isn't a
                                     container
      │
      └── Firecracker guest `akuma-guest` (nested one level deeper) ──
            Not Linux — the actual Akuma kernel (../akuma), booted via
            overlays/devbox-firecracker/{host,guest}-setup.sh + build.sh
            + run.sh. Reachable at 10.0.2.15 from inside fc.

            node4 (replica) ── same aarch64-unknown-linux-musl binary
                                as node3, running as a herd service
                                (/etc/herd/enabled/miot.conf,
                                restart=true) — Akuma's own supervisor
                                auto-starts it on boot, same as sshd
                                and httpd. INTERMITTENT — see below,
                                not durably up.
```

Every replica (`node2`, `node3`, `node4`) pulls from `node` the same way,
over the same HTTP endpoints (`/chain/head`, `/chain/blocks`,
`/chain/checkpoint`) — nothing about the protocol changes based on what's
running underneath it. `node4` reaches `node` at `192.168.5.2:9944`: out
through `akuma-guest`'s own NAT (`fc`'s tap0), through `fc`'s uplink, through
Lima's gateway, to the docker port `node` publishes on the host. Four hops,
one HTTP GET.

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
machine, one level deeper, not another name for the same thing. Don't
conflate them; a previous draft of this session's notes briefly did, and it
was confusing enough to call out here on purpose.

## Where this came from

- HANDOFF.md item 5 — the primary/replica replication protocol every node
  above speaks.
- `docs/references/storage.md` — the wire mechanism (`/chain/*` endpoints)
  and the compaction/checkpoint machinery `node4` adopted on its first sync.
- `../akuma/docs/runbooks/run-on-firecracker.md` and
  `overlays/devbox-firecracker/README.md` — how `akuma-guest` actually boots;
  read there before re-deriving any of the Firecracker/tap/DHCP setup.
