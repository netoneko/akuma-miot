# Target topology — 5-agent mesh (not yet live)

> **Superseded (2026-09-24):** the mesh that actually runs has seven members,
> five home agents plus two on AWS, and is called the teahouse. See
> `docs/TEAHOUSE.md`. This page is kept for the per-agent rationale.

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
| **dumpster-akuma-amd64** | akuma trashcan (`ssh akuma`, physical box) | bare metal, no Firecracker | amd64 | GLM via `~/.akuma/z.ai/token` (`kot run --glm`) | mesh member, election-eligible |
| **ryzen-linux-amd64** | ryzen (192.168.1.126, Pop!_OS) | bare process | amd64 | host `llama-server` on ryzen | mesh member, election-eligible |
| **ryzen-akuma-amd64** | ryzen (same host) | Firecracker guest, real `/dev/kvm` — no nested virt, unlike the mac path | amd64 (guest kernel) | host `llama-server` on ryzen, reached from inside the guest | mesh member, election-eligible |
| **mac-linux-aarch64** | macbook (this mac, arm64) → Lima VM `fc` | bare process inside `fc`'s Linux userspace | aarch64 | host `llama-server` on the mac | mesh member, election-eligible |
| **mac-akuma-aarch64** | macbook → `fc` → nested Firecracker guest `akuma-guest` | Firecracker guest on nested virt (Apple Silicon M3+, macOS 15+) | aarch64 (guest kernel) | host `llama-server` on the mac, four NAT hops out | mesh member, election-eligible — **blocked on the ParityDB index-growth panic, see below** |

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
| dumpster-akuma-amd64 | `/root/kot/id_ed25519.seed` on the akuma box | pasted into every agent's `MIOT_ROSTER`/membership config |
| ryzen-linux-amd64 | `/root/kot/id_ed25519.seed` on ryzen (native path) | " |
| ryzen-akuma-amd64 | its own seed file *inside* the Firecracker guest's rootfs | " |
| mac-linux-aarch64 | its own seed file inside `fc`'s Linux userspace | " |
| mac-akuma-aarch64 | its own seed file inside `akuma-guest`'s rootfs | " |

Root (the operator) is a sixth identity, not one of the 5 — `~/.akuma/miot/
id_ed25519.seed` on the mac, already generated (see `docs/CLEANUP.md`,
"Already true"). Every agent's `MIOT_ROOT_PUBKEY` must be set to this
identity's `.pub` content — that is the actual "root signs for real" change,
config only.

## Network

Addresses are fixed per host; `overlays/deploy/deploy.sh`'s `route()` is
the source of truth for who reaches whom at what URL. The mesh is **three
members** (quorum 2) until the Firecracker agents exist. All five
identities are in genesis already (`overlays/deploy/mesh.env`), so adding
them is a `MIOT_PEERS` change, not a new chain:

| agent | address | notes |
|---|---|---|
| dumpster-akuma-amd64 | `192.168.1.120:9944` (DHCP moved it from `.123` on 2026-09-24; no reservation yet) | direct, no NAT. GLM: `zai-coding::glm-5.3`. **Joined, then wedged within minutes** (2026-09-22, twice, once per boot): replays its store, follows the primary, the agent connects, then `:9944` refuses even from `127.0.0.1` and replica sync stops, while every thread sits in `R` accruing CPU. This is CLEANUP's stability item #2 (replica catch-up on this box), and it belongs to `../akuma`, not here. **Did not reproduce, 2026-09-23**: redeployed, joined as a replica (term 25, synced), stayed reachable for several minutes with the agent loop running real turns (`Bash`/`SendMessage`, including both in one turn). Not root-caused or proven fixed — one clean run against an intermittent kernel-level issue — but back in `deploy.sh`'s `LIVE` on the strength of it |
| ryzen-linux-amd64 | `192.168.1.126:9944` | direct, no NAT. **Live 2026-09-22.** Model: `llama-ryzen-linux-amd64.service`, 127.0.0.1:8081, Qwen3-4B-Instruct-2507 Q4_K_M, 6 threads (CPU; ollama disabled) |
| ryzen-akuma-amd64 | `____` (guest address, behind ryzen's own NAT/tap — pattern TBD, no precedent yet; `run-on-firecracker.md`'s aarch64 path is the closest reference but is nested-virt-specific) | new — nothing has run an amd64 Firecracker Akuma guest yet |
| mac-linux-aarch64 | `192.168.1.203:9944` from the LAN (Lima forwards `fc:9944-9949` on `0.0.0.0`; `fc` itself is `192.168.5.15`) | **Live 2026-09-22.** Model: the mac's llama-server at `192.168.5.2:8083` (qwen3:4b). systemd `kot.service` inside `fc`, no longer started by hand |
| mac-akuma-aarch64 | `10.0.2.15:9944` inside `fc`; mac-linux-aarch64 reaches it there directly, the LAN via `192.168.1.203:9945` (socat relay `kot-relay-mac-akuma-aarch64.service` in `fc`, exposed by Lima) | **Live 2026-09-22**, in dumpster-akuma-amd64's seat. 10 min as primary, 11 min as replica, no wedge. ParityDB survives crash and clean close since two aarch64 kernel fixes in `../akuma` (`../akuma/docs/archive/MIOT_MESH_ON_AKUMA.md`) |

`--peers` on each `kot run` invocation needs the other 4 agents' reachable
addresses — from *that* agent's vantage point, which differs for the two
guest agents (they reach out through NAT; they are not reached the same way
symmetrically). Confirm each direction works, not just one, before calling
the mesh assembled.

## Known blocker

**mac-akuma-aarch64 cannot join durably yet.** `docs/TOPOLOGY.md`'s `node4` section:
ParityDB panics on `akuma-guest` once its index grows past some threshold
(`index.rs:237`, unresolved). This agent is the direct successor to `node4`
and inherits the same blocker — building the new topology does not fix it;
it needs its own investigation (bounding index growth via more frequent
compaction, or finding the underlying `ext2`/mmap difference) before mac-akuma-aarch64
can be trusted the way the other 4 can.

## Provenance

Derived from `docs/CLEANUP.md`'s "New topology" section (2026-09-22, the
same conversation that agreed the `kot` CLI interface and the merge-`miot`-
into-`kot` plan). Read that doc for the reasoning; this one is the
lookup table.
