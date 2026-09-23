# Deploy a new private network with one node on AWS

Written 2026-09-23. **A procedure, not a record: none of it has run yet.**
It's the plan `docs/KEY_MANAGEMENT.md` ("What adding an AWS node changes")
and `docs/MESH_AUTH.md` (the mTLS hard cutover) both point at, spelled out
against what exists today.

**Goal:** one fresh genesis with seven members. The five home agents from
`docs/TOPOLOGY_TARGET.md` keep their hosts, seeds, personas and models. The
other two are **two kots on the AWS stack that's already deployed**,
`../akuma-terraform/akuma-stack-aws`. Every link uses `https://` with mTLS
pinned to genesis keys, and there's no VPN.

The AWS kots are **`yuki`** (雪, snow) and **`shiro`** (白, white), both new
personas (`crates/kot/personas/{yuki,shiro}.md`). Added in that order, they
are kot #1 and #2 on the stack, so their public ports are **9441** and
**9442**. Both are workers.

---

## What's already there (checked 2026-09-23, not assumed)

`bin/tf.sh output` and `ssh ubuntu@51.84.169.84` show the following:

| | |
|---|---|
| host | one `t4g.nano` (Ubuntu 24.04 arm64, 406 MiB + 1 GiB swap), `il-central-1`, EIP **`51.84.169.84`** |
| DNS | `akuma.sh`, `www.akuma.sh`, **`kot.akuma.sh`**, all A records to the EIP, Route53 |
| kot model | one `systemd-nspawn` container per kot (`kotctl`). kot N is `10.77.0.(10+N):9944` on `br-kot` (NAT out), public as `kot.akuma.sh:(9440+N)` through an nginx **stream** proxy |
| security group | `22` from `87.71.28.157/32` (whatever network last ran `apply`), `80`/`443` open, **`9441-9460` from `0.0.0.0/0`** |
| TLS | a Let's Encrypt cert for all three names (`/etc/letsencrypt/live/akuma.sh`) |
| state | `bootstrap done`, **no koty registered, no `/opt/kot/bin/kot` pushed yet**, so nothing needs migrating |

The stack was written against the **pre-mTLS, pre-roster-genesis** `kot`.
The deployed `kotctl` is wrong for today's `main` in four places. All four
are fixed in the `akuma-terraform` tree (§2), but not yet on the box:

1. **nginx terminates TLS on the kot ports** (`render_nginx`: `listen … ssl`
   with the Let's Encrypt cert). `kot` now terminates **its own** mTLS
   (`crates/kot/src/tls.rs`). A client pins the kot's self-signed ed25519
   cert, not a CA cert, and the kot requires a client cert. A TLS-terminating
   proxy in between breaks both directions. Those ports have to become plain
   TCP passthrough.
2. **Peers are `http://`** (`render_kot`). They have to be `https://`: reqwest
   only runs the TLS connector for that scheme.
3. **Genesis comes only from the local registry** (`cmd_sync`: members and
   roster are just this host's koty). A kot joining the home mesh needs the
   home mesh's genesis instead.
4. **The README's "read endpoints are open by design" is no longer true.**
   Every read needs a genesis-member signature (`docs/MESH_AUTH.md`, closed
   2026-09-23). `curl http://<ip>:9441/tasks` gets nothing.

---

## What changed in `kot` for this (2026-09-23)

**The roster is genesis now, committed to chain state.** There's no separate
members list.

- `kot run` takes membership from `MIOT_ROSTER` alone. `--members`/
  `MIOT_MEMBERS` is gone; a leftover `MIOT_MEMBERS=` line in an env file is
  ignored.
- The roster is written into `pallet_litter::Roster` at genesis, names
  included, and served as `GET /roster`. It's signed-read-only, like every
  other read.
- **`kot run` refuses a roster that can't be a genesis:** a name listed
  twice, one key under two names, or a `root=` entry that isn't
  `MIOT_ROOT_PUBKEY`. That catches the fcguest double-listing
  `deploy.py ids` used to produce (fixed too: it labels by persona and stages
  a guest's seed only if the guest isn't live).
- **A block log remembers its genesis.** On first open, the store saves a
  fingerprint of root, leader and roster in its aux space. Opened later under
  a different genesis, `kot run` refuses: *"built under a different genesis
  … move that directory aside"*. The chain's own genesis hash can't catch
  this, because it's `H256::zero()` for every chain. A log from before this
  change has no fingerprint, so it's adopted with a warning. **The fleet's
  current logs are like that, and moving them aside (§5) is still on you.**
- **A client needs no roster at all.** It reads `/roster` from the node it
  connects to and uses those names. It doesn't pin the node's cert: it's a
  private chain the operator runs, so whichever node answers is trusted.
  The node still pins the *client*, so only genesis members get in
  (`crates/kot/src/tls.rs`, `client_config_any_node`). Known gap: something
  on the path can pose as a node to the client (fake answers, seeing what
  you send), but it can't relay you to a real one, since that needs a
  member's key.

`MIOT_LEADER` may now be a roster name (`meow`) as well as a hex account.

---

## 0. Decide these first

- **The kots' LLM:** GLM (`kotctl add`'s default). The box can't run
  inference: it has 406 MiB, and each kot is capped at 128 MiB (two kots plus
  nginx fit). `push-kot.sh` already ships `~/.akuma/z.ai/token`, the same token
  `dumpster-akuma-amd64` uses, so **three cats share one coding-plan quota**.
- **The litter leader** stays `meow`. `yuki` and `shiro` are workers.
- **Quorum:** seven members means a majority of **4**, so the mesh survives
  losing 3 (five survived 2). But members share hosts: the mac holds two, ryzen
  two, and the AWS box two. Mac off the home LAN leaves 5. An AWS outage takes
  out both AWS kots at once and leaves 5. Mac away *and* AWS down leaves 3,
  which is below quorum. A home ISP outage splits the mesh 5 | 2: home keeps
  going, and the two AWS kots can't elect anyone. They catch up by pull-sync
  when the link returns. Adding or removing a member later is another genesis.

---

## 1. Home side: make each home node reachable from AWS

Election and replication run both ways. Every node polls `/mesh/status` on
every peer, a candidate POSTs `/mesh/vote` to every peer, and a replica
pulls `/chain/*` from the primary. So the AWS kots need a route to **each**
home member. Outbound from the container already works (br-kot NAT, open
egress). Inbound to home needs one forward per member on the home router:

| router WAN port | → LAN target | member |
|---|---|---|
| `9944` | `192.168.1.123:9944` | dumpster-akuma-amd64 (`meow`) |
| `9945` | `192.168.1.126:9944` | ryzen-linux-amd64 (`tama`) |
| `9946` | `192.168.1.203:9944` | mac-linux-aarch64 (`kuro`), Lima's `0.0.0.0` forward of `fc:9944` |
| `9947` | `192.168.1.203:9945` | mac-akuma-aarch64 (`mimi`), the `kot-relay` socat in `fc` |
| `9948` | `192.168.1.50:9944` | ryzen-akuma-amd64 (`sora`), the guest's own LAN address |

- Only the router changes here: DHCP reservations for those five addresses
  (the mac's `.203` especially), plus the forwards. Nothing new goes on
  `ryzen`, `akuma` or `fc`.
- Restrict the forwards' source to `51.84.169.84` if the router can do it.
- Home nodes keep reaching each other by LAN address, and the two AWS kots
  reach each other over `br-kot`, so there's no hairpin NAT. Only the
  AWS→home direction goes through the router.
- **`<HOME_IP>`** below is the home connection's public address. If the ISP
  changes it, only the AWS kots' `MIOT_PEERS` go stale. That's config, not
  genesis: fix `/etc/kot/peers` (§2) and run `kotctl sync`. `restart` alone doesn't
  re-render `kot.env`. A
  dynamic-DNS name works in those URLs too, because the verifier ignores
  the server name and pins only the key.

---

## 2. AWS side: ship the new `kotctl` (`../akuma-terraform`)

**Done in the tree, not yet on the box (2026-09-23).** `files/kotctl` is now
Python, with the same commands and output, so `bin/*.sh`, `bootstrap.sh` and
`akuma-tls` are unchanged. It includes your uncommitted `make_rootfs`
change, and the four fixes:

1. **nginx passes TCP straight through on the kot ports.** There's no `ssl`
   and no "no cert yet, ports stay closed" gate, so the kot's own mTLS runs
   end to end. The Let's Encrypt cert serves only the website.
2. **Peers are `https://`**, the local koty on br-kot plus every line of
   **`/etc/kot/peers`** (external members, one URL per line).
3. **A joined litter:** if **`/etc/kot/genesis.env`** exists,
   `MIOT_ROOT_PUBKEY`, `MIOT_LEADER` and `MIOT_ROSTER` come from it verbatim.
   Without it, the litter is standalone, as before. No `MIOT_MEMBERS` either way.
   `kotctl roster` prints whichever genesis is in force; that's what
   `bin/kot.sh` gives the Mac's client.
4. **`sync` refuses to start a kot the genesis doesn't name** as
   `name=pub:<its account>`. Nothing is rendered, enabled or restarted, and it
   prints the line(s) to add. With a `genesis.env` in place, that makes
   `kotctl add yuki` and `add shiro` safe before the home side has
   regenerated genesis: each mints an identity, prints the accounts still
   missing, and waits. (A standalone litter
   always contains its own koty, which is why §3 installs the current genesis
   *first*.) Units are now enabled by `sync` after the check, not by `add`,
   so a refused kot doesn't start on the next boot either.

README, `bin/kot.sh`, the `kot_api_cidrs` description, `example.tfvars` and
the certbot hook no longer claim reads are open or that nginx does TLS.
Checked on the Mac against the real `kot` binary with paths redirected to a
scratch dir: a refused `add`, the rendered `kot.env`/nginx/nspawn files, a
standalone litter, and `kot run` accepting the rendered env (then refusing
the same log under a genesis without `yuki`). Only one kot was tested this
way, not two side by side. **It hasn't run on the box
yet.** `systemctl`, `nginx` and `machinectl` were stubbed.

Then narrow the kot ports to home (defence in depth; mTLS already refuses a
stranger at the handshake):

```bash
cd ../akuma-terraform/akuma-stack-aws
echo 'kot_api_cidrs = ["<HOME_IP>/32"]' >> terraform.tfvars
bin/tf.sh plan    # must touch only aws_vpc_security_group_ingress_rule.kot_api
bin/tf.sh apply
bin/push-host.sh  # ships the new kotctl (and every host file); re-runs bootstrap, which is idempotent
```

**Check the plan before applying.** Anything that replaces
`aws_instance.host` wipes every kot's identity and block log (`compute.tf`).
A security-group rule change doesn't do that.

---

## 3. Build, then mint the AWS kots' identities

```bash
cargo test --workspace                     # includes the new genesis/roster tests
overlays/local/build.sh all                # dist/aarch64/kot is the one AWS runs
cargo build --release -p kot               # the Mac's own client, for bin/kot.sh

# crates/kot/personas/{yuki,shiro}.md are in the tree already. push-kot.sh ships the whole dir.

cd ../akuma-terraform/akuma-stack-aws
bin/push-kot.sh --no-sync                  # binary + personas + z.ai token, restart nothing
# The home genesis as it stands, without yuki or shiro, *before* add. With no genesis.env,
# kotctl is a standalone litter: it would start them on a genesis of their own and leave
# them block logs that the real genesis then refuses.
bin/ssh.sh 'sudo tee /etc/kot/genesis.env >/dev/null' < ../../akuma-miot/overlays/deploy/mesh.env
bin/kotctl.sh add yuki                     # #1, port 9441: mints kot-yuki/kot/id_ed25519.seed;
                                           # sync refuses to start it (§2 item 4)
bin/kotctl.sh add shiro                    # #2, port 9442: same; the refusal now lists both
bin/kotctl.sh list                         # both, with ports 9441/9442 and account prefixes
```

The seeds never leave the box (0600, created by `kot id`, never
regenerated). Copy the two `name=pub:<64 hex>` lines from the refusal after
`add shiro`. `list` shows only a prefix of each account.

**Add them in this order.** `kotctl` numbers kots by the lowest free slot,
and the number fixes the port. `yuki` first means `yuki` is 9441 and `shiro`
is 9442, which is what §4's URLs assume. `bin/kotctl.sh list` confirms it.

---

## 4. Home side: generate the new genesis

`overlays/deploy/deploy.py`. `deploy.sh` isn't touched; its `ids` still
has the old bugs, so don't use it for genesis.

Both edits are **already in `deploy.py`** (2026-09-23): every `route()` URL
is `https://` (the mTLS cutover `docs/MESH_AUTH.md` left for the redeploy),
and there's an `EXTERNAL` table for members that something else deploys. Its
entries go into the roster, after the agents so the leader doesn't move, and
into every agent's peer list. All that's left is filling it in:

```python
EXTERNAL: dict[str, tuple[str, str]] = {
    "yuki":  ("<yuki's 64 hex>",  "https://kot.akuma.sh:9441"),
    "shiro": ("<shiro's 64 hex>", "https://kot.akuma.sh:9442"),
}
```

Note that `https://` in `route()` applies to the **next** `deploy.py up` of
any agent. That's fine as part of this procedure, but don't run `up` on one
agent in isolation before §5: it would ship the mTLS binary to one node of a
mesh still running plain HTTP.

Check without touching a host, then generate:

```bash
for a in dumpster-akuma-amd64 ryzen-linux-amd64 mac-linux-aarch64 mac-akuma-aarch64 ryzen-akuma-amd64; do
  python3 overlays/deploy/deploy.py env $a | grep PEERS   # 6 peers, all https, yuki's and shiro's last
done

cp overlays/deploy/mesh.env overlays/deploy/mesh.env.pre-aws     # rollback, §8
python3 overlays/deploy/deploy.py ids                            # on the home LAN: ssh to every live host
```

`ids` runs `kot id` on each host. For the five existing agents this is a
no-op on the key: `load_or_create_identity` never overwrites a seed.
**Check the result against the old one:**

```bash
diff <(grep ^MIOT_ROSTER overlays/deploy/mesh.env.pre-aws | cut -d= -f2- | tr , '\n') \
     <(grep ^MIOT_ROSTER overlays/deploy/mesh.env         | cut -d= -f2- | tr , '\n')
```

Expect exactly **two added lines**, `yuki=pub:…` then `shiro=pub:…`. Anything else means
stop: a changed key for an existing cat, or an agent-id label instead of a
persona name. `MIOT_LEADER` should be unchanged, and there should be no
`MIOT_MEMBERS` line. `kot run` would refuse duplicates anyway, but the
diff is the cheap place to see them.

---

## 5. Coordinated restart onto the new genesis

**Stop everything, move every home block log aside, then start everything.**
A rolling upgrade doesn't work: old binaries speak `http://` and new ones
`https://`, and the membership differs.

**Stop** (commands as in `docs/runbooks/run-the-mesh.md`):

```bash
ssh ryzen 'systemctl stop kot.service'
limactl shell fc -- sudo systemctl stop kot.service
# akuma + both fcguests are herd: remove the enable first, or herd restarts it within seconds. One ssh exec on the akuma box.
ssh akuma 'rm -f /etc/herd/enabled/kot.conf; for p in $(ps | grep "/root/kot/bin/kot run" | grep -v grep | awk "{print \$1}"); do kill $p; done'
# ...then the same on mac-akuma-aarch64 (ssh -p 4444 root@localhost)
#    and ryzen-akuma-amd64 (ssh -p 2222 -i <amd64 test key> root@192.168.1.50)
```

**Move the old logs aside on all five home hosts.** They predate the
genesis fingerprint, so the new binary would adopt them with only a
warning. Keep them for §8.

```bash
mv /root/kot/db /root/kot/db.pre-aws-2026-09-23 && mkdir -p /root/kot/db
```

`yuki` and `shiro` have never started, so they have no logs to move.

**Give the AWS kots their genesis and peers:**

```bash
cd ../akuma-terraform/akuma-stack-aws
bin/ssh.sh 'sudo tee /etc/kot/genesis.env >/dev/null' < ../../akuma-miot/overlays/deploy/mesh.env
printf 'https://<HOME_IP>:%s\n' 9944 9945 9946 9947 9948 | bin/ssh.sh 'sudo tee /etc/kot/peers >/dev/null'
```

`mesh.env` holds public keys only (`KEY_MANAGEMENT.md`).

**Start:**

```bash
python3 overlays/deploy/deploy.py up all            # the five home agents
(cd ../akuma-terraform/akuma-stack-aws && bin/kotctl.sh sync)   # genesis check passes now; yuki and shiro start
```

---

## 6. Verify every direction, not just one

```bash
for n in https://192.168.1.123:9944 https://192.168.1.126:9944 https://192.168.1.203:9944 \
         https://192.168.1.203:9945 https://192.168.1.50:9944  https://kot.akuma.sh:9441 https://kot.akuma.sh:9442; do
  echo "== $n"; kot --node $n peers
done
```

- **Stable**, as `run-the-mesh.md` defines it: all seven views agree on one
  `leader`, heads within a block or two, and the term not climbing.
- **AWS's own view proves the port forwards:** `bin/kot.sh yuki peers` and
  `bin/kot.sh shiro peers` from `akuma-stack-aws`. All five home members and
  the other AWS kot should answer, with none `STALE` or `never answered`.
- **The chain's roster:** the `litter` block of `peers` shows eight names
  (root plus seven members), read from `/roster`.
- **Both work as cats:** `bin/kot.sh yuki say --to yuki "hi"` and the same
  for `shiro`, then `bin/kotctl.sh logs yuki -f` / `logs shiro -f`. A targeted `say` wakes it; an untargeted one
  doesn't (known gap, `CLAUDE.md`).
- **A stranger gets nothing.** From anywhere else,
  `openssl s_client -connect kot.akuma.sh:9441` (and `:9442`) should time out (security
  group) or, from home, end in a TLS1.3 `certificate_required` alert. What
  it must **not** show is the Let's Encrypt cert: that would mean nginx is
  still terminating (§2 item 1).
- **A partition heals:** `bin/kotctl.sh restart yuki`. It replays, pulls and
  follows, with the term flat or bumped once. Then `bin/kotctl.sh restart`
  with no name restarts both at once, which is the AWS-box-down case. 5 of 7
  remain, so the mesh should keep its leader throughout.

---

## 7. Afterwards

- **Docs:** `docs/TOPOLOGY_TARGET.md` (two AWS rows, plus the quorum note),
  `docs/runbooks/run-the-mesh.md` (every `http://…:9944` becomes
  `https://`), `docs/FLEET.md` (the AWS host), and `CLAUDE.md` (the
  redeploy paragraph; "Mesh membership is static" now names `MIOT_ROSTER`,
  not `MIOT_MEMBERS`). Record the verification in `docs/RESULTS.md`.
- **WAN cost:** a 1 s `/mesh/status` poll and a 2 s sync to five peers,
  constant. If AWS egress shows up, `MIOT_POLL_MS`/`MIOT_SYNC_MS` are
  per-node config (not genesis) and can be raised on the AWS kots alone.
- **What this doesn't give you:** a replicated commit (`CLAUDE.md`, "Election
  ≠ replication"). If an AWS kot is primary when the WAN drops, blocks no
  home node pulled are lost to the rewind.
- **Still open:** the mesh-level `VoteRequest.candidate` isn't
  cross-checked against the signer (`docs/MESH_AUTH.md`, "Scope not
  covered"). The node now has a name→account roster to do it with. But the
  home agents' mesh names (`MIOT_NAME=ryzen-linux-amd64`) aren't their
  roster names (`tama`), so a check would first need those unified.
  `kotctl` already uses one name for both.

---

## 8. Rollback

1. Stop everything (§5). Run `bin/kotctl.sh rm yuki` and `rm shiro`: their
   rootfs and seeds are kept, harmless because no genesis names them. Then
   `bin/ssh.sh sudo rm /etc/kot/genesis.env /etc/kot/peers`.
2. `cp overlays/deploy/mesh.env.pre-aws overlays/deploy/mesh.env` and empty
   `EXTERNAL` again. Keep `https://` in `route()`: the new binary needs it, with or
   without AWS.
3. On each home host, move `db` aside and put `db.pre-aws-2026-09-23` back.
   The old `mesh.env` has a `MIOT_MEMBERS` line, which is now ignored. Its
   roster is the same six members, so the restored log opens: it has no
   fingerprint and is adopted with the warning.
4. `python3 overlays/deploy/deploy.py up all`.
