# Deploy a new private network with one node on AWS

Written 2026-09-23. **A procedure, not a record: none of it has run yet.**
It's the plan `docs/KEY_MANAGEMENT.md` ("What adding an AWS node changes")
and `docs/MESH_AUTH.md` (the mTLS hard cutover) both point at, spelled out
against what exists today.

**Goal:** one fresh genesis with six members. The five home agents from
`docs/TOPOLOGY_TARGET.md` keep their hosts, seeds, personas and models. The
sixth is **one kot on the AWS stack that's already deployed**,
`../akuma-terraform/akuma-stack-aws`. Every link uses `https://` with mTLS
pinned to genesis keys, and there's no VPN.

Example names below: the AWS kot is **`yuki`**, a new persona. It's kot
#1 on the stack, so its public port is **9441**. Both are placeholders.

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

The stack was written against the **pre-mTLS, pre-roster-genesis**
`kot`. Against today's `main`, `kotctl` is wrong in four places, and all four
are fixed in §2:

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
- **Clients name accounts by the chain's roster.** A client reads `/roster`
  on connect and uses those names, so a stale local `MIOT_ROSTER` can't
  mislabel anyone. **A client still needs a roster locally**, though: its
  accounts are the client's mTLS trust set, and trust has to be decided
  before anything can be read. The local roster lost its say over *names*,
  not over *trust*.

`MIOT_LEADER` may now be a roster name (`meow`) as well as a hex account.

---

## 0. Decide these first

- **The kot's LLM:** GLM (`kotctl add`'s default). The box can't run
  inference: it has 406 MiB, and each kot is capped at 128 MiB. `push-kot.sh`
  already ships `~/.akuma/z.ai/token`, the same token `dumpster-akuma-amd64`
  uses, so the two cats share one coding-plan quota.
- **The litter leader** stays `meow`. The AWS kot is a worker.
- **Quorum gets worse, not better.** Six members means a majority of **4**
  (it was 3 of 5), so you can still only lose 2. The mac and ryzen each host
  two members. With the mac off the home LAN, 4 are left: exactly quorum, and
  one more loss stops the mesh. A home ISP outage isolates AWS alone; the
  five home nodes keep going, and AWS catches up by pull-sync. If that's not
  good enough, the fix is a seventh member (a second kot on the stack is one
  `kotctl add`), decided **now**: adding one later is another genesis.

---

## 1. Home side: make each home node reachable from AWS

Election and replication run both ways. Every node polls `/mesh/status` on
every peer, a candidate POSTs `/mesh/vote` to every peer, and a replica
pulls `/chain/*` from the primary. So the AWS kot needs a route to **each**
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
- Home nodes keep reaching each other by LAN address, so there's no hairpin
  NAT. Only the AWS kot's view goes through the router.
- **`<HOME_IP>`** below is the home connection's public address. If the ISP
  changes it, only the AWS kot's `MIOT_PEERS` goes stale. That's config, not
  genesis: fix `/etc/kot/peers` (§2) and run `kotctl sync`. `restart` alone doesn't
  re-render `kot.env`. A
  dynamic-DNS name works in those URLs too, because the verifier ignores
  the server name and pins only the key.

---

## 2. AWS side: bring `kotctl` up to date (`../akuma-terraform`)

These are edits to `akuma-stack-aws/files/kotctl` (which has an uncommitted
change of yours in the tree; these go on top of it) and to `README.md`:

1. **`render_nginx`: passthrough, no TLS.** Each kot's server block becomes

   ```nginx
   server {
       listen        $(port_of "$n");
       proxy_pass    $(ip_of "$n"):9944;
       proxy_timeout 1h;
   }
   ```

   Delete the `ssl_*` lines and the "no certificate yet, kot ports stay
   closed" gate: the kot ports no longer depend on Let's Encrypt at all. The
   cert keeps serving the website on 443. `kot.akuma.sh` stays as the name
   in the URL, and nothing validates it against a CA.
2. **`render_kot`: `https://` peers, plus external ones.** Change
   `http://$(ip_of "$p"):9944` to `https://…`. Then append every line of
   `/etc/kot/peers`, if that file exists: one URL per line, the home members
   as this host reaches them.
3. **Imported genesis.** If `/etc/kot/genesis.env` exists, `cmd_sync` takes
   `MIOT_ROOT_PUBKEY`, `MIOT_LEADER` and `MIOT_ROSTER` from it verbatim
   instead of computing them from the registry. Without it, it behaves as
   today, a standalone litter. Either way, **stop writing `MIOT_MEMBERS`**.
   `kotctl roster` prints the imported roster when there is one (that's
   what `bin/kot.sh` hands the Mac's client).
4. **Refuse to start a kot that isn't in the genesis it's about to run.**
   Before `cmd_restart`, check that every registered kot's
   `account_of "$n"` appears in the roster as `$n=pub:<that account>`.
   Otherwise, die with its name and account. With a `genesis.env` in place,
   that makes `kotctl add yuki` safe to run before the home side has
   regenerated genesis: it mints the identity, prints the account, and
   refuses to start `yuki` until `genesis.env` names it. (Without one, the
   local roster always contains the kot, so the check passes. That's why §3
   installs the current genesis first.)
5. **README:** fix the "Talking to a kot" and "Security notes" sections.
   Reads are signed, the kot ports carry the kot's own mTLS, and "plain
   HTTP" / "open by design" are gone.

Then narrow the kot ports to home (defence in depth; mTLS already refuses a
stranger at the handshake):

```bash
cd ../akuma-terraform/akuma-stack-aws
echo 'kot_api_cidrs = ["<HOME_IP>/32"]' >> terraform.tfvars
bin/tf.sh plan    # must touch only aws_vpc_security_group_ingress_rule.kot_api
bin/tf.sh apply
bin/push-host.sh  # ships the edited kotctl; re-runs bootstrap, which is idempotent
```

**Check the plan before applying.** Anything that replaces
`aws_instance.host` wipes every kot's identity and block log (`compute.tf`).
A security-group rule change doesn't do that.

---

## 3. Build, then mint the AWS kot's identity

```bash
cargo test --workspace                     # includes the new genesis/roster tests
overlays/local/build.sh all                # dist/aarch64/kot is the one AWS runs
cargo build --release -p kot               # the Mac's own client, for bin/kot.sh

# crates/kot/personas/yuki.md — same shape as the other five. push-kot.sh ships the whole dir.

cd ../akuma-terraform/akuma-stack-aws
bin/push-kot.sh --no-sync                  # binary + personas + z.ai token, restart nothing
# The home genesis as it stands, without yuki, *before* add. With no genesis.env, kotctl
# is a standalone litter: it would start yuki on a genesis of its own and leave it a
# block log that the real genesis then refuses.
bin/ssh.sh 'sudo tee /etc/kot/genesis.env >/dev/null' < ../../akuma-miot/overlays/deploy/mesh.env
bin/kotctl.sh add yuki                     # mints /var/lib/machines/kot-yuki/kot/id_ed25519.seed,
                                           # prints its account; sync refuses to start it (§2 item 4)
```

The seed never leaves the box (0600, created by `kot id`, never
regenerated). Note the 64-hex account `add` printed. `bin/kotctl.sh list`
shows it again.

---

## 4. Home side: generate the new genesis

`overlays/deploy/deploy.py`. `deploy.sh` isn't touched; its `ids` still
has the old bugs, so don't use it for genesis.

1. **The AWS kot, as an external member.** `deploy.py` doesn't deploy it
   (`kotctl` does), but genesis and every home peer list have to name it.
   Add a table next to `LIVE`:

   ```python
   # Members deployed by something else (akuma-stack-aws's kotctl): in the
   # roster and every peer list, never shipped to. name -> (account, url).
   EXTERNAL = {"yuki": ("<yuki's 64 hex>", "https://kot.akuma.sh:9441")}
   ```

   `cmd_ids`: after the agents, append `f"{name}=pub:{acct}"` for each
   `EXTERNAL` entry to `roster`, keeping it **last** so the leader
   (`accounts[0]`) doesn't move. `env_for`: append each `EXTERNAL` url to
   `peers`.
2. **`route()`: every URL becomes `https://`,** the two `special` entries
   included. This is the mTLS cutover `docs/MESH_AUTH.md` left for the
   redeploy. A leftover `http://` peer fails to connect, and nothing else
   tells you.

Check without touching a host, then generate:

```bash
for a in dumpster-akuma-amd64 ryzen-linux-amd64 mac-linux-aarch64 mac-akuma-aarch64 ryzen-akuma-amd64; do
  python3 overlays/deploy/deploy.py env $a | grep PEERS   # 5 peers, all https, yuki's last
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

Expect exactly **one added line**, `yuki=pub:…`. Anything else means
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

`yuki` has never started, so it has no log to move.

**Give the AWS kot its genesis and peers:**

```bash
cd ../akuma-terraform/akuma-stack-aws
bin/ssh.sh 'sudo tee /etc/kot/genesis.env >/dev/null' < ../../akuma-miot/overlays/deploy/mesh.env
printf 'https://<HOME_IP>:%s\n' 9944 9945 9946 9947 9948 | bin/ssh.sh 'sudo tee /etc/kot/peers >/dev/null'
```

`mesh.env` holds public keys only (`KEY_MANAGEMENT.md`).

**Start:**

```bash
python3 overlays/deploy/deploy.py up all            # the five home agents
(cd ../akuma-terraform/akuma-stack-aws && bin/kotctl.sh sync)   # yuki: genesis check passes now, it starts
```

---

## 6. Verify every direction, not just one

```bash
R="$(grep ^MIOT_ROSTER overlays/deploy/mesh.env | cut -d= -f2-)"
for n in https://192.168.1.123:9944 https://192.168.1.126:9944 https://192.168.1.203:9944 \
         https://192.168.1.203:9945 https://192.168.1.50:9944  https://kot.akuma.sh:9441; do
  echo "== $n"; kot --node $n --roster "$R" peers
done
```

**Always pass `--roster "$R"`.** Names now come from the chain, but the local
roster is still the client's TLS trust set. Leave it out and the client
trusts only `DEV_ROSTER`'s dev seeds, and every real node is refused.

- **Stable**, as `run-the-mesh.md` defines it: all six views agree on one
  `leader`, heads within a block or two, and the term not climbing.
- **AWS's own view proves the port forwards:** `bin/kot.sh yuki peers` from
  `akuma-stack-aws`. All five home members should answer, with none
  `STALE` or `never answered`.
- **The chain's roster:** the `litter` block of `peers` shows seven names
  (root plus six), read from `/roster`, not from `$R`.
- **`yuki` works as a cat:** `bin/kot.sh yuki say --to yuki "hi"`, then
  `bin/kotctl.sh logs yuki -f`. A targeted `say` wakes it; an untargeted one
  doesn't (known gap, `CLAUDE.md`).
- **A stranger gets nothing.** From anywhere else,
  `openssl s_client -connect kot.akuma.sh:9441` should time out (security
  group) or, from home, end in a TLS1.3 `certificate_required` alert. What
  it must **not** show is the Let's Encrypt cert: that would mean nginx is
  still terminating (§2 item 1).
- **A partition heals:** `bin/kotctl.sh restart yuki`. It replays, pulls and
  follows, with the term flat or bumped once.

---

## 7. Afterwards

- **Docs:** `docs/TOPOLOGY_TARGET.md` (a sixth row, plus the quorum note),
  `docs/runbooks/run-the-mesh.md` (every `http://…:9944` becomes
  `https://`), `docs/FLEET.md` (the AWS host), and `CLAUDE.md` (the
  redeploy paragraph; "Mesh membership is static" now names `MIOT_ROSTER`,
  not `MIOT_MEMBERS`). Record the verification in `docs/RESULTS.md`.
- **WAN cost:** a 1 s `/mesh/status` poll and a 2 s sync to five peers,
  constant. If AWS egress shows up, `MIOT_POLL_MS`/`MIOT_SYNC_MS` are
  per-node config (not genesis) and can be raised on `yuki` alone.
- **What this doesn't give you:** a replicated commit (`CLAUDE.md`, "Election
  ≠ replication"). If `yuki` is primary when the WAN drops, blocks no home
  node pulled are lost to the rewind.
- **Still open:** the mesh-level `VoteRequest.candidate` isn't
  cross-checked against the signer (`docs/MESH_AUTH.md`, "Scope not
  covered"). The node now has a name→account roster to do it with. But the
  home agents' mesh names (`MIOT_NAME=ryzen-linux-amd64`) aren't their
  roster names (`tama`), so a check would first need those unified.
  `kotctl` already uses one name for both.

---

## 8. Rollback

1. Stop everything (§5). Run `bin/kotctl.sh rm yuki`: its rootfs and seed
   are kept, harmless because no genesis names it. Then
   `bin/ssh.sh sudo rm /etc/kot/genesis.env /etc/kot/peers`.
2. `cp overlays/deploy/mesh.env.pre-aws overlays/deploy/mesh.env` and empty
   `EXTERNAL`. Keep `https://` in `route()`: the new binary needs it, with or
   without AWS.
3. On each home host, move `db` aside and put `db.pre-aws-2026-09-23` back.
   The old `mesh.env` has a `MIOT_MEMBERS` line, which is now ignored. Its
   roster is the same six members, so the restored log opens: it has no
   fingerprint and is adopted with the warning.
4. `python3 overlays/deploy/deploy.py up all`.
