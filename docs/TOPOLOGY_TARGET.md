# Target topology — 5-agent mesh (not yet live)

This is the **planned** replacement for `docs/TOPOLOGY.md`, which documents
what is actually running today and says so explicitly ("not aspirational").
This doc is the opposite: aspirational by design, the thing `docs/CLEANUP.md`
exists to build toward. Don't read this as current state. Once the cleanup
lands, this content should replace `docs/TOPOLOGY.md`'s diagram (with real
IPs/ports filled in and a "verified" pass the way that doc's `node2`/`node5`
sections were verified), and this file can retire.

Background and rationale for every decision below: `docs/CLEANUP.md`. This
doc is the reference table on its own, without the narrative, for pulling up
quickly during actual deployment.

## The 5 agents

"Agent" = one `kot run` process: an election-capable mesh node plus that
host's agent loop, in one process, on one LLM backend. No `node1..node5`
numbering — named by placement instead.

| agent | host | environment | arch | LLM backend | node role |
|---|---|---|---|---|---|
| **akuma-metal** | akuma trashcan (`ssh akuma`, physical box) | bare metal, no Firecracker | amd64 | GLM via `~/.akuma/z.ai/token` (`kot run --glm`) | mesh member, election-eligible |
| **ryzen-linux** | ryzen (192.168.1.126, Pop!_OS) | bare process | amd64 | host `llama-server` on ryzen | mesh member, election-eligible |
| **ryzen-fc** | ryzen (same host) | Firecracker guest, real `/dev/kvm` — no nested virt, unlike the mac path | amd64 (guest kernel) | host `llama-server` on ryzen, reached from inside the guest | mesh member, election-eligible |
| **mac-linux** | macbook (this mac, arm64) → Lima VM `fc` | bare process inside `fc`'s Linux userspace | aarch64 | host `llama-server` on the mac | mesh member, election-eligible |
| **mac-fc** | macbook → `fc` → nested Firecracker guest `akuma-guest` | Firecracker guest on nested virt (Apple Silicon M3+, macOS 15+) | aarch64 (guest kernel) | host `llama-server` on the mac, four NAT hops out | mesh member, election-eligible — **blocked on the ParityDB index-growth panic, see below** |

Every agent is a full mesh member — no operator-designated primary/replica.
Leadership is elected; any agent can become primary if it wins, any agent
can serve reads/replicate as a non-leader. (The election protocol itself is
explicitly not designed yet — `docs/CLEANUP.md`'s "Deliberately not decided"
note.)

## Identity

Each agent gets **one real random seed, generated once, kept forever** —
not the `1,2,3,4,5` dev-seed convention `overlays/local`'s docker-compose
swarm uses. Generation mechanism: the same one `crates/miot/src/rpc.rs::
load_or_create_identity` already uses for the operator's own root identity
(`getrandom`, hex-encoded, 0600 permissions) — reuse that function (moved
into `kot` per the merge) rather than writing a second generator.

| agent | identity file (per-host) | public key shared as |
|---|---|---|
| akuma-metal | `/root/kot/id_ed25519.seed` on the akuma box | pasted into every agent's `MIOT_ROSTER`/membership config |
| ryzen-linux | `/root/kot/id_ed25519.seed` on ryzen (native path) | " |
| ryzen-fc | its own seed file *inside* the Firecracker guest's rootfs | " |
| mac-linux | its own seed file inside `fc`'s Linux userspace | " |
| mac-fc | its own seed file inside `akuma-guest`'s rootfs | " |

Root (the operator) is a sixth identity, not one of the 5 — `~/.akuma/miot/
id_ed25519.seed` on the mac, already generated (see `docs/CLEANUP.md`,
"Already true"). Every agent's `MIOT_ROOT_PUBKEY` must be set to this
identity's `.pub` content — that is the actual "root signs for real" change,
config only.

## Network

Static IPs/ports are **not yet assigned** — fill in below once each agent's
actual bind address is chosen (fixed per host, since placement is fixed):

| agent | address | notes |
|---|---|---|
| akuma-metal | `192.168.1.123:9944` (host already has this address; port matches today's `node5`) | direct, no NAT |
| ryzen-linux | `192.168.1.126:____` | direct, no NAT; today's `node2` used `:9944` — reassign if `ryzen-fc` also wants `9944` on the same host |
| ryzen-fc | `____` (guest address, behind ryzen's own NAT/tap — pattern TBD, no precedent yet; `run-on-firecracker.md`'s aarch64 path is the closest reference but is nested-virt-specific) | new — nothing has run an amd64 Firecracker Akuma guest yet |
| mac-linux | `fc`'s Lima-assigned address, reachable from the mac at `192.168.5.x` per today's `node3` pattern | matches today's `node3`/`kuro` placement |
| mac-fc | `10.0.2.15` inside `fc` (today's `akuma-guest` address), reachable from outside via `192.168.5.2:9944` through `fc`'s tap0 — same 4-hop path `node4` uses today | unchanged from today's `node4` networking |

`--peers` on each `kot run` invocation needs the other 4 agents' reachable
addresses — from *that* agent's vantage point, which differs for the two
guest agents (they reach out through NAT; they are not reached the same way
symmetrically). Confirm each direction works, not just one, before calling
the mesh assembled.

## Known blocker

**mac-fc cannot join durably yet.** `docs/TOPOLOGY.md`'s `node4` section:
ParityDB panics on `akuma-guest` once its index grows past some threshold
(`index.rs:237`, unresolved). This agent is the direct successor to `node4`
and inherits the same blocker — building the new topology does not fix it;
it needs its own investigation (bounding index growth via more frequent
compaction, or finding the underlying `ext2`/mmap difference) before mac-fc
can be trusted the way the other 4 can.

## Provenance

Derived from `docs/CLEANUP.md`'s "New topology" section (2026-09-22, the
same conversation that agreed the `kot` CLI interface and the merge-`miot`-
into-`kot` plan). Read that doc for the reasoning; this one is the
lookup table.
