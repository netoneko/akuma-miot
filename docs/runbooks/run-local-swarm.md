# Running the local swarm

The first runbook in this directory — modeled on the sibling `../akuma` repo's
`docs/runbooks/`. This one covers the everyday loop: bring the local litter
up, talk to it, redeploy it after a code change, and diagnose "a cat isn't
responding" before assuming it's an application bug.

Today's topology: `mimi`/`tama`/`sora` + `node` (primary) + `node2` (a
passive read replica, HANDOFF item 5 — see its own section below) in docker
(`overlays/local/docker-compose.yml`), `kuro` on the Lima VM (`fc`) instead of
docker — see `HANDOFF.md`'s "What is real" section for why. Models
(`llama-server` ×4) run on the host, never in a container (no GPU passthrough
on Docker Desktop for macOS). Two more replicas exist outside this
docker-compose file entirely — `node3` on `fc`'s own Linux userspace and
`node4` on the actual Akuma kernel, nested one level deeper via Firecracker —
started manually, not yet scripted into this runbook's everyday loop.
`docs/TOPOLOGY.md` has the full diagram.

## Bring it up from nothing

```bash
overlays/local/llama-swarm.sh up          # 4 llama-servers on the HOST, ports 8081-8084
docker build -t akuma-miot:net .          # see "Rebuilding" below — compose does NOT build this
docker compose -f overlays/local/docker-compose.yml up -d
curl -s localhost:9944/head               # sanity: node is answering
```

The node's chain state persists in the `node-db` named volume (`MIOT_DB=/data/miot.db`
in its container). `docker compose down` alone keeps it; `docker compose down -v`
deletes it — that's a real, deliberate genesis reset, not an accident.

`kuro` on the Lima VM is separate — see "kuro on Lima" below. If `fc` isn't
running yet: `limactl start fc` (or `limactl list` to check).

## Talking to it

```bash
cargo run -p miot -- --rpc http://localhost:9944 --identity-seed 1 \
  --open "your question here"
cargo run -p miot -- --rpc http://localhost:9944 --identity-seed 1 --chat
```

**`--identity-seed 1` is not optional the first time.** Without it, `--chat`
generates and persists a brand-new random identity
(`~/.akuma/miot/id_ed25519.seed`) that the node's genesis never granted a
provider to, and every single submit comes back `refused: rejected:
Invalid(Payment)` — the `catnip` gate in `HANDOFF.md`, not a bug. Seed `1` is
root by the default `MIOT_MEMBERS=1,2,3,4,5` convention; `2,3,4,5` are
mimi/tama/kuro/sora. The error message itself now explains this
(`crates/miot/src/rpc.rs::submit`).

In `--chat`:

- Plain text broadcasts to the whole litter.
- `@tama` (or `@kuro`, etc.) addresses one cat; `@tama @kuro` addresses both,
  each as its own `say` extrinsic, replies print in tag order as they arrive.
- `@all` / `@cats` / `@litter` are explicit broadcast, same as no tag.
- An unrecognized `@name` prints a warning and falls back to broadcast — it
  never silently drops the message.
- `/tasks` — one line per live task (id, status, holder/assignee, lease).
- `/clear` — fails every open parent (operator/root only).
- Blank line, `/quit`, or `/exit` leaves.
- Replies print **as they land**, in the background — the prompt does not
  block waiting for one. A cat's real turn is 30-150+ seconds (`BLOCK_MS` is
  6000 in `miot-node`, so that's a dozen-plus blocks); don't mistake "no
  reply yet" for "broken" for at least a couple of minutes.

## Rebuilding and redeploying after a code change

**The compose file does not build the image** — every service just says
`image: akuma-miot:net`, so `docker compose build` silently does nothing.
The image comes from the repo-root `Dockerfile`:

```bash
docker build -t akuma-miot:net .
docker compose -f overlays/local/docker-compose.yml up -d --force-recreate
```

Do this after touching `crates/miot-node`, `crates/miot-cat`,
`crates/pallet-litter`, `crates/miot-runtime`, or `crates/miot-primitives` —
even a small addition (e.g. a new dispatchable) means the running node's
`RuntimeCall` enum doesn't have it, and calling it will fail oddly rather than
clearly.

**`kuro` no longer needs restarting just because the node did — fixed
2026-09-22, `miot-store` is wired in now.** A node restart (`restart node` or
even `up -d --force-recreate node`) replays its persisted log and comes back
with the *same* state and a *continuing* `seq` count, not a reset to
genesis — so a `kuro` process that was already running still has a valid
cursor and keeps working without intervention. (This used to be the single
most likely explanation for "a cat stopped responding" — it no longer is.
Still restart `kuro` when its own binary changed, same as any code update.)
If the node's data volume is ever actually wiped (`docker compose down -v`,
or deleting `MIOT_DB`'s path outside docker), *that* genuinely resets to
genesis and cats do need restarting again, same as before.

Quick staleness check, given a report that something isn't working:

```bash
git log -1 --format=%cd -- crates/miot-node crates/miot-cat crates/pallet-litter
docker inspect akuma-miot:net --format '{{.Created}}'
```

If the image predates the last relevant commit, rebuild before debugging
anything else.

## `kuro` on the Lima VM

`kuro` was moved off docker onto `fc` (`docs/references/storage.md` /
`HANDOFF.md`, 2026-09-22) to prove a cat can run somewhere that isn't a
container without the node noticing. `docker-compose.yml` still *defines* a
`kuro` service — **do not `up` it** while the Lima one is running, or you'll
have two processes signing as the same account (`MIOT_SEED=4`) against the
same node, racing on nonces. If `docker compose up` ever recreates it by
accident: `docker compose -f overlays/local/docker-compose.yml stop kuro &&
docker compose -f overlays/local/docker-compose.yml rm -f kuro`.

To (re)start `kuro` on `fc`:

```bash
limactl shell fc -- bash -lc '
  pkill -f "./miot-cat"
  cd /home/netoneko.guest
  export MIOT_NAME=kuro MIOT_SEED=4 MIOT_MODEL=qwen3:4b \
         MIOT_NODE=http://192.168.5.2:9944 \
         MIOT_LLM=http://192.168.5.2:8083 \
         MIOT_PERSONA=/home/netoneko.guest/kuro.md
  setsid nohup ./miot-cat > /home/netoneko.guest/miot-cat.log 2>&1 < /dev/null &
'
```

`192.168.5.2` is the Lima gateway address — it reaches the macOS host's
`127.0.0.1`-bound services (llama-server, the node's published port) from
inside the VM. **Use `setsid`, not just `& disown`** — plain `disown` was
observed to not reliably survive the `limactl shell` session closing;
`setsid` fully detaches the process from the controlling terminal. Tail
`~/miot-cat.log` on the VM (`limactl shell fc -- tail -f
/home/netoneko.guest/miot-cat.log`) to see its turns and any `llm error:` /
`refused:` lines — this is the *only* place those show up; they never reach
your host terminal.

If `kuro`'s binary itself needs updating (a `miot-cat`-affecting change),
rebuild for the VM's target and copy it over — see `HANDOFF.md`'s "Target
mismatch" note and `overlays/local/build-akuma.sh` for the
`aarch64-unknown-linux-musl` cross-compile, then `limactl copy` onto `fc`.

## Checking the models

```bash
for p in 8081 8082 8083 8084; do curl -s localhost:$p/health; echo; done
```

All four should answer `{"status":"ok"}`. If one is down, whichever cat is
assigned that port (`mimi`→8081, `tama`→8082, `kuro`→8083, `sora`→8084 per
`docker-compose.yml`) will fail its turns with `llm error: ...` — visible in
`docker compose logs <cat>` for the docker cats, or `kuro`'s log on `fc` per
above.

## Reading the chain directly

```bash
curl -s "http://localhost:9944/events?since=0" | python3 -m json.tool   # full log
curl -s http://localhost:9944/tasks | python3 -m json.tool              # one row per live task
curl -s http://localhost:9944/head                                      # block, leader, seq
```

Useful for confirming what actually happened on chain when a client's own
printer is in doubt — the event log's `wakes` field is the ground truth for
"was this cat actually told to act," independent of whether it did.

## `node2`, a read replica — HANDOFF item 5

`overlays/local/docker-compose.yml` also brings up `node2`, running
`miot-node` as `MIOT_ROLE=replica MIOT_PEER=http://node:9944` — it pulls
`node`'s block log over HTTP (`docs/references/storage.md` has the wire
mechanism) and mirrors it, but no cat talks to it and it refuses `/submit`.
It comes up with the rest of `docker compose up -d`; nothing extra to do.

Confirm it's actually mirroring `node` (a few seconds after any change,
`MIOT_SYNC_MS` defaults to the block time, 6s):

```bash
diff <(curl -s localhost:9944/tasks) <(curl -s localhost:9945/tasks) && echo MATCH
diff <(curl -s localhost:9944/events) <(curl -s localhost:9945/events) && echo MATCH
curl -s localhost:9944/chain/head; curl -s localhost:9945/chain/head   # heads should agree
```

`node2` restarts the same way `node` does (`docker compose restart node2`)
and resumes from its own persisted log, then keeps tailing `node` — same
"cats don't need restarting" property item 2 gave `node` itself, now true of
a replica catching back up too.

**`adopted peer's checkpoint at block N` is normal, not an error** — it
prints every time `node` compacts (root ran `/clear`) and `node2` picks up
the new checkpoint on its next sync tick. Expected, routine, no action
needed.

If `node2`'s log ever shows `sync: diverged from peer above block N, rewound
to M (...)` (`M` is the last checkpoint if one exists, genesis/`0` if not),
that means its local log disagreed with `node`'s somewhere above block `N`
and it rebuilt itself from `M` to match — expected behavior if `node2` was
ever run independently (e.g. `MIOT_ROLE=primary` against its own `MIOT_DB`,
for testing), not something to debug. It should never happen in ordinary
operation, where `node2` only ever appends blocks
`node` gave it.
