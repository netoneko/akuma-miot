# Cleanup & topology redo — handoff, 2026-09-22

Written for whoever (agent or operator) picks this up next. Read
`HANDOFF.md` first anyway — this is a scoped follow-on to its "Not yet
real"/"Next, in order" sections, not a replacement for it. Nothing here has
been implemented yet; this is the agreed shape, not a report of work done.

## The ask, in one paragraph

Redo the fleet from today's ad-hoc `node1..node5` naming into a fixed
5-member mesh with **real leader election** (today's `MIOT_ROLE` env var is
an operator's manual switch, not election — HANDOFF item 5 Part 2, never
built) and **a stable network** (two concrete instability findings already
on record — see "Stability bar" below). Alongside that, merge `crates/miot`
entirely into `crates/kot` — one binary per host doing both node and agent
duties, per `docs/CLI.md` §5a's design (`miot run --as tama` = "a full node
in-process + the agent loop"), which was written but never built. `crates/miot`
is deleted once the merge lands, not kept alongside it.

## Already true — do not re-derive or re-generate

- **Root signing is already wired.** `crates/miot/src/rpc.rs::load_or_create_identity`
  (lines 41–109) reads/creates `~/.akuma/miot/id_ed25519.seed` (0600, hex,
  64 chars) and writes its public half to `~/.akuma/miot/id_ed25519.pub` in
  `authorized_keys` format, ready to paste as `MIOT_ROOT_PUBKEY`. This file
  already exists on this machine (created 2026-09-22). **HANDOFF.md's "No
  OpenSSH private-key signing" gap is not what this solves and is still
  real** — this is a project-native hex seed file, not an OpenSSH private
  key — but it *does* mean "root actually signs" is a config step (point
  every node's `MIOT_ROOT_PUBKEY` at this file's `.pub` content), not new
  code. Carry this path over unchanged into `kot` — it is not tied to the
  `miot` binary name.
- **The z.ai/GLM token exists**: `~/.akuma/z.ai/token`. `miot-llm` already
  has a GLM provider (HANDOFF: "provider layer on `genai` (15 providers,
  GLM included)") — confirm it's wired to read this token file specifically,
  since that wasn't checked in this session.
- **No per-agent identities have been pre-generated yet beyond root's own.**
  The operator wants "the same pre-generated keys this time" (i.e.: generate
  each of the 5 agents' identities *once*, keep them stable across every
  future redeploy, rather than regenerating on each deploy the way the
  `1,2,3,4,5` dev-seed convention implicitly invites). **Open task**:
  generate 5 real random seeds (same mechanism as `load_or_create_identity`
  — `getrandom`, not small ints), one per agent below, and commit to keeping
  them. Where they live per-host is part of the deployment blueprint, below.

## Stability bar — what "stable network" has to clear

Two concrete failures are already on record; election alone does not fix
either, and calling the network "stable" without addressing them would be
re-asserting a claim `HANDOFF.md` already retracted once:

1. **`node4`'s ParityDB index-growth panic** (`docs/TOPOLOGY.md`, "What
   `node4` actually proves"): `parity-db-0.5.6/src/index.rs:237` panics once
   the index needs to grow past some threshold, on the Firecracker-guest
   aarch64 path specifically. Not root-caused. This is exactly the
   akuma-aarch64 agent's role in the new topology (#5 below) — it cannot be
   "the" akuma-aarch64 agent if it still can't stay up.
2. **Replica catch-up sync from mac's log has never been re-tested on the
   post-fix Akuma kernel** (`docs/TOPOLOGY.md`, `node5` section) — it wedged
   mid-sync pre-fix (the writable-`MAP_SHARED` mmap gap); the fix landed in
   `../akuma` the same day but the retest never happened. Election's whole
   point is a replica taking over as primary — an unverified replica-sync
   path undermines that regardless of how leadership is decided.

Both need a real re-test pass, not just a design for election, before the
new topology can be called durable.

## New topology — 5 agents, 3 hosts

"Agent" = the operator's own term: one `kot run` process, packaging a mesh
node (election-capable) together with its agent loop against one LLM
backend. No more `node1`/`node2`/... numbering — name agents by role/host,
not by creation order.

| # | placement | what it runs | LLM backend |
|---|---|---|---|
| 1 | **akuma trashcan** (bare metal, amd64, physical `ssh akuma` box) | `kot run`, bare metal, no Firecracker | GLM via `~/.akuma/z.ai/token` (`--glm`) |
| 2 | **ryzen** (native Linux, amd64) | `kot run`, bare process, no Firecracker | host `llama-server` on ryzen |
| 3 | **ryzen** (same host) | `kot run` inside a Firecracker guest running the Akuma kernel — real `/dev/kvm`, no nested virt needed (`../akuma/docs/runbooks/run-on-firecracker.md` §1: nested virt is only the Apple-Silicon workaround; a native x86_64 Linux host has real KVM) | host `llama-server` (reachable from the guest, same NAT pattern `node4` already uses) |
| 4 | **macbook** (this mac, arm64, via Lima VM `fc`) | `kot run`, bare process inside `fc`'s own Linux userspace — same placement as today's `node3`/`kuro` | host `llama-server` on the mac |
| 5 | **macbook** → nested Firecracker guest inside `fc` (`akuma-guest`, same nesting `node4` uses today) | `kot run` on the actual Akuma kernel, aarch64 | host `llama-server` (four hops out through NAT, as today) |

Agent #1 is closest to today's `node5` (already durably up, bare metal,
herd-supervised) — the work there is adding the agent loop and GLM backend
on top of what's already running, not standing up the node side from
scratch. Agent #5 is blocked on the stability item above. Agents #2–4 are
close to today's `node2`/`node3` placement; #3 (Firecracker-on-ryzen) is
new — nothing has run a Firecracker Akuma guest on real x86_64 KVM yet, only
the nested-virt aarch64 path. Read `../akuma/docs/runbooks/run-on-firecracker.md`
in full before building it: it's written around the aarch64/nested-virt
case throughout (§1's host check is an Apple-Silicon-specific nested-virt
probe), and the amd64/real-KVM path, while it should need less
workaround (no nested virt to verify, no `vz`/Lima layer), is not what that
runbook was written against — check `../akuma/docs/README.md`'s symptom
matrix and the AWS-metal doc it cross-references (`docs/archive/
AKUMA_FIRECRACKER_TERRAFORM.md`) for the closest verified amd64+KVM
precedent before assuming the aarch64 runbook's steps transfer as-is.

## Work items, in order

### 1. Deployment blueprint — bare metal is the template

One blueprint, three shapes, per the operator's framing ("bare metal is a
good blueprint, linux can just run the miot [now `kot`] process only, should
be enough"):

- **Bare metal Akuma** (agent #1, and the akuma-side half of #3/#5's guest
  once built): the template. `docs/TOPOLOGY.md`'s `node5` section already
  has the working shape — herd service, `/root/miot/bin/<binary>`, `MIOT_DB`
  under `/root/miot/db/`, HTTP transfer (no scp, no SFTP; the SSH exec
  channel stalls at exactly 1,048,576 bytes per HANDOFF's traps list).
  Generalize this into a repeatable script rather than the one-off manual
  steps `TOPOLOGY.md` narrates.
- **Firecracker on Akuma** (agents #3, #5): the bare-metal template plus one
  layer underneath — build the Akuma kernel for the target arch, boot it
  under Firecracker (`../akuma/overlays/devbox-firecracker/{host,guest}-setup.sh`
  + `build.sh` + `run.sh` is the aarch64/nested-virt precedent; amd64 needs
  its own host-setup since there's no nested-virt step, real KVM instead),
  then the *same* bare-metal deployment steps run inside the guest once it's
  up. Don't fork the deployment logic per-arch — only the boot/host-prep
  step differs.
- **Plain Linux** (agents #2, #4): the bare-metal template minus the
  Firecracker/kernel layer entirely — just the binary, a service
  supervisor (systemd on ryzen per today's `node2`; whatever `fc`'s Lima
  userspace uses for #4, currently nothing — `kuro`/`node3` are started by
  hand today per `docs/runbooks/run-local-swarm.md`, worth fixing here
  rather than carrying forward).

### 2. Merge `miot` into `kot`

**Interface agreed with the operator** (2026-09-22) — implement this shape,
not a redesign:

```
kot run --as <name>
  --peers <addr,addr,...>     # mesh membership for election — replaces
                               # MIOT_ROLE/MIOT_PEER; a node's role is
                               # elected, not operator-set
  --port <port>                # default 9944
  --db <path>                  # every mesh member is durable now, not just
                               # ones that opted into MIOT_DB
  --llm <url> | --glm          # host llama-server, or GLM via
                               # ~/.akuma/z.ai/token (agent #1 only)
  --model <name>
  --persona <path>
  --roster name=seed,...

kot task open "<text>"
kot task list
kot artifact <id>
kot say "<body>" [--to <name>]
kot clear                      # root-only
kot peers                      # litter roster + mesh status: role,
                                # term/leader, head height, last-sync — new;
                                # needed to actually observe "stable" once
                                # election exists
kot log [--task <id>] [--follow]

kot                             # bare: interactive REPL (today's rpc::chat),
                                 # same slash commands docs/CLI.md §5 lists,
                                 # /peers gains the same mesh-status fields
```

Connection resolution: `--node <url>`/`MIOT_NODE`, falling back to a
configured list (`MIOT_NODES`) so "any swarm node will do" (`docs/CLI.md`
§5a) — a dead one means trying the next, not failing. Not built today
(`--rpc` takes exactly one URL); build it here.

Signing: one-shot/REPL commands default to the persisted identity at
`~/.akuma/miot/id_ed25519.seed` (already wired, see above — carries over
unchanged). `--as <name>` overrides to a roster seed to act as a specific
cat instead of root.

Env vars remain the config source for a service unit/herd conf (`MIOT_NAME`,
`MIOT_PEERS`, `MIOT_PORT`, `MIOT_DB`, `MIOT_LLM`, `MIOT_GLM_TOKEN_FILE` or
similar, `MIOT_MODEL`, `MIOT_PERSONA`, `MIOT_ROSTER`); flags are for
one-off/interactive use, same split `miot --rpc` already has today.

**Delete `crates/miot` once this lands** — `run_node`, `rpc.rs`'s `run`/
`chat`, the manual `args.iter().position(...)` argv scanning all move into
`crates/kot`, restructured behind a real arg parser. Don't keep `miot` as a
compatibility shim; the operator was explicit about removing it entirely.

**Deliberately not decided in this doc**: the election protocol itself —
what `--peers` actually does on the wire (terms, heartbeats, quorum size for
a 5-member mesh, how a partition or a single unreachable peer is
distinguished from "that peer lost the election"). This needs its own design
pass, informed by `docs/MAPPING_REPORT.md` §1.1's findings table (this
project's track record is "behavioural findings only learnable by running
the thing" — don't design election in the abstract without expecting at
least one surprise the same way `DirectiveNag`'s missing cap or the
tip-only-divergence-check bug were surprises). Keep the "one operator's
trusted swarm, no Byzantine tolerance" framing `docs/TOPOLOGY.md`'s
leader-wins/rewind design already established — election here is about
*availability* (promote a replica when the primary dies), not about
distrust between mesh members.

### 3. Topology cleanup

Once agents #1–5 are up under the new naming and `kot` has replaced `miot`:

- Decommission every `node1..node5`-named process/container/service —
  `docker-compose.yml`'s already-commented-out `node2` entry is the pattern
  to finish, not a one-off. Check `overlays/local/docker-compose.yml`, the
  ryzen `systemd` units, and the akuma/`fc`/`akuma-guest` herd configs for
  leftover references to the old naming before considering this done.
- Static IPs and keys: assign each of the 5 agents a fixed address (host is
  already fixed per the table above; pin the port/bind address too so
  `--peers` lists don't drift), and use the once-generated identities from
  "Already true," above — not the `1,2,3,4,5` dev-seed convention, which is
  fine for `overlays/local`'s docker-compose swarm but was never meant to be
  a real deployment's actual key material.
- `MIOT_ROOT_PUBKEY` on every one of the 5 nodes must point at the real
  operator identity's `.pub` (see "Already true") — this is the actual
  "root signs for real" change; no code, just making sure every node's
  config agrees on which account is root.

## Where to read

- `HANDOFF.md` — full project state; this doc is a scoped follow-on to its
  "Not yet real" and "Next, in order" sections, not a replacement.
- `docs/TOPOLOGY.md` — today's (soon to be former) topology; `node4`/`node5`
  sections are the stability findings cited above.
- `docs/FLEET.md` — model/host assignments; check dates before trusting
  anything there, it predates this redo.
- `docs/CLI.md` §5/§5a — the interface this doc's `kot` proposal
  implements; read it in full, this doc only excerpts the relevant parts.
- `../akuma/docs/runbooks/run-on-firecracker.md` — the aarch64/nested-virt
  Firecracker path; read before building the amd64/real-KVM one on ryzen,
  since it's written specifically around the Apple-Silicon workaround this
  new path doesn't need.
- `../akuma/docs/README.md` — symptom matrix; check before forming a theory
  about anything that looks like a kernel-level oddity while building
  agent #3 or debugging #5's stability item.
