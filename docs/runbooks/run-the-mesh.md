# Run the mesh

The everyday loop for the 5-agent mesh (`docs/TOPOLOGY_TARGET.md`). Replaces
`run-local-swarm.md`, which was the docker-compose litter. Docker is gone.

## Build and ship

```bash
overlays/local/build.sh all                 # dist/aarch64/kot, dist/x86_64/kot
overlays/deploy/deploy.sh up all            # every live agent: binary, persona, env, service
overlays/deploy/deploy.sh up ryzen-linux    # or one
```

`up` restarts the agent's service; a restarted node replays its store and
rejoins the mesh as a follower (or wins an election, if nobody leads). No
other agent needs restarting — there is no "restart the cats after the node"
step any more, the cat *is* in the node's process.

Supervisors: systemd `kot.service` on ryzen and inside `fc`; herd
`/etc/herd/enabled/kot.conf` → `/root/kot/start.sh` on the akuma box.
Models: `llama-<agent>.service` on ryzen (`deploy.sh llama ryzen-linux`),
`overlays/local/llama-swarm.sh up` on the mac (reached from `fc` at
`192.168.5.2:808x`). Never ollama.

## Look at it

```bash
kot --node https://192.168.1.126:9944 peers     # the roster comes from the node; no --roster
```

`peers` prints the litter roster (who's litter leader) and the mesh as the
connected node sees it: each member's role, term, head, who it thinks
leads, and how long since it answered. **Stable** means: one `leader`,
everyone else `follower` naming it, heads within a block or two, the term
not climbing. A term that keeps climbing is an election that keeps failing
— look for a member whose link flaps.

Logs: `journalctl -u kot -f` (ryzen; `limactl shell fc -- sudo journalctl -u
kot -f` for mac-linux). The lines that matter are `[mesh] … elected
primary`, `[mesh] following …`, `[node] sync: diverged … rewound`, and a
cat's `block N … — thinking` / tool-call lines.

## Talk to it

Any node will do — a replica forwards writes to the primary.

```bash
kot --node https://192.168.1.126:9944 say --to kuro "hi"          # a targeted say wakes that cat
kot --node https://192.168.1.126:9944 task open "the question"    # the litter leader plans it
kot --node https://192.168.1.126:9944                             # REPL
```

An untargeted `say` wakes nobody, by design (a broadcast that woke every cat
would cost four turns per line).

## Before blaming the code

- `kot peers` from two different nodes. If they disagree about who leads,
  it's a reachability problem between those two, not an election bug.
- A write refused `no primary right now` during an election is expected;
  retry. One that's refused with a pallet error is the chain's answer.
- The akuma box: HANDOFF traps. Don't `kill` things there by hand if herd
  can do it; batch ssh execs.
