# The current home litter — topology, 2026-09-22

What's actually running right now, on one operator's machine, across every
virtualization boundary that machine has. Not aspirational — every node
below is up and verified converged as of this date (see HANDOFF.md item 5
and its "real fork, real reconciliation" writeup for how each replica was
proven, not just started).

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
                                (/etc/herd/enabled/miot-node.conf,
                                restart=true) — Akuma's own supervisor
                                auto-starts it on boot, same as sshd
                                and httpd.
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
file I/O, basic file read/write). It is evidence the surface `miot-node`
needs works *on this build*, not a general claim about Akuma — the same
caution `HANDOFF.md` already applies to every Akuma claim applies here too.

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
