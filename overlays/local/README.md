# `overlays/local` — a litter on one laptop

Design of record. **Nothing here is built yet** — the binaries it launches do
not exist until Phase 2 (`miot-node`) and Phase 3 (`miot-cli`). This file is
the topology those phases target, written down now so the shape does not get
decided by accident later.

## Stage 1 — everything is Linux, inside one Lima VM

Start here. No Firecracker, no TAPs, no disk images, no Akuma at all.

```
  macOS host
  └── lima VM  (plain Linux, plain processes)
      ├── miot-node ×3     ports 9944 / 9945 / 9946
      ├── llama-server     port 8080, shared
      └── miot-cli ×N      one per cat: tama, kuro, mimi
```

Everything talks over localhost. A validator needs a keystore and a port; an
agent needs an account seed and an RPC endpoint. That is the whole setup.

**The chain is still the only channel between agents** — even on one host, no
agent reads another's local store. That invariant is what makes Stage 2 a
deployment change rather than a redesign, so it is worth enforcing from the
first run even when nothing would stop you breaking it.

## Stage 2 — agents move into Akuma guests

Only once Stage 1 runs a parent task end to end. Then the agents — and only the
agents — become Firecracker microVMs, following the pattern
`akuma/overlays/devbox-firecracker/run.sh` already establishes: on macOS the
KVM host is the Lima VM (`--via-lima`; kernel and disk are copied *into* it
because Lima's virtiofs mount is read-only), on metal the machine itself is the
KVM host (`--local`). One TAP per guest.

```
  lima VM (the KVM host)
      ├── miot-node ×3         still ordinary processes — a validator does
      ├── llama-server         not need a microVM, it needs a port
      │
      ├── firecracker: tap0 ──► akuma guest "tama"   miot-cli
      ├── firecracker: tap1 ──► akuma guest "kuro"   miot-cli
      └── firecracker: tap2 ──► akuma guest "mimi"   miot-cli
```

Each guest reaches the Lima host over its TAP: RPC to a validator, HTTP to
`llama-server`. Nothing else changes — same binary, same config, same chain.

## Why three validators and N agents

Three is the smallest set where GRANDPA survives losing one, which is the only
consensus failure worth rehearsing. Agents are *clients*, so their count is
independent: a four-cat litter against three validators is an ordinary
configuration, not a mismatch.

## Files (planned)

| File | Does |
|---|---|
| `up.sh` | `--stage1` (default): lima → chainspec → 3 validators → llama-server → N agents. `--stage2` adds taps and Akuma guests. |
| `down.sh` | reverse, including tap teardown when stage 2 ran |
| `chainspec.local.json` | generated; committed only if it stops being reproducible |
| `agents/<name>.toml` | per-agent: account seed, RPC endpoint, model, aggregation policy |

## What it is for

The failure drills, as a script rather than a ceremony:

- kill an agent mid-claim → the lease expires and the work requeues
- partition an agent → it fails fast, never hangs a turn
- stall an agent inside a turn past its lease → the late-submit path runs
- kill a validator → the chain keeps producing

Each is a line in §6 Phase 6 of `docs/MAPPING_REPORT.md`, and none is testable
by hand. All four work in Stage 1; none of them needs Akuma.

## Traps that only apply to Stage 2

Recorded now so they are not rediscovered:

- Do not touch `disk.img` while a VM has it open — that corrupts it.
- Getting a binary into a running guest: **not** scp (no SFTP subsystem, the
  client hangs), **not** an SSH exec channel (reproducibly stalls at exactly
  1,048,576 bytes). HTTP from the host works.
- Building inside Akuma is blocked: cargo's concurrent spawn path hits
  `EFAULT` after ~8 spawns. Cross-compile from the host.
