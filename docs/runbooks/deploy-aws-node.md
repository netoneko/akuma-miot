# Deploy a new private network with one node on AWS

Written 2026-09-23. **A procedure, not a record: none of it has run yet.**
It is the plan `docs/KEY_MANAGEMENT.md` ("What adding an AWS node changes")
and `docs/MESH_AUTH.md` (the mTLS hard cutover) both point at, spelled out.

What you end up with: a **fresh genesis** with six mesh members. The five
home agents from `docs/TOPOLOGY_TARGET.md` stay where they are, with the same
hosts, models, personas and seeds. The sixth is a plain Linux EC2 instance
running `kot run`. Every link uses `https://` with mTLS pinned to genesis
keys. There is no VPN; `docs/MESH_AUTH.md` rejected Tailscale on purpose.

The example name used below is **`aws-linux-aarch64`**, a Graviton instance
running a new cat, persona **`yuki`**. Both names are placeholders; pick your
own and substitute them everywhere.

---

## 0. Decide these before touching anything

| decision | recommendation | why |
|---|---|---|
| Instance arch | Graviton (`t4g.small`, aarch64) | `dist/aarch64/kot` is already built for Lima `fc`. It's a static musl binary, so any distro works. On x86 (`t3.small`), use `dist/x86_64/kot` and `arch="x86_64"`. |
| Distro | Amazon Linux 2023 or Ubuntu 24.04 | Needs systemd and nothing else. `deploy.py`'s `linux` shape already works on it (`scp` + a systemd unit). |
| The cat's LLM | **GLM** (`llm="glm"`, the same z.ai token `dumpster-akuma-amd64` uses) | A small instance can't do useful local inference. GLM is one token file, and `cmd_up` already ships it (`~/.akuma/z.ai/token` → `/root/kot/zai.token`). Both cats then share one coding-plan quota, so watch for rate limits. |
| Litter leader | leave it as `meow` (`dumpster-akuma-amd64`) | `cmd_ids` picks "first row of `LIVE`" as leader. Keep AWS **last** in `LIVE` so the leader doesn't change by accident. |

**Quorum changes, and not in your favour.** Six members means a majority of
**4** (five members needed 3). An even count adds no fault tolerance: you can
still lose only 2 nodes. Count what actually shares a failure:

- The mac hosts two members (`mac-linux-aarch64`, `mac-akuma-aarch64`), and
  so does ryzen. Mac off the home LAN (the coffee-shop case in `CLAUDE.md`)
  leaves exactly 4, which is quorum with no slack. Losing one more node
  stops the mesh.
- A home ISP outage isolates AWS alone. The 5 home nodes keep quorum, and the
  AWS node catches up by pull-sync when the link returns.
- An AWS outage costs 1 of 6, so nothing changes.

If that's not acceptable, the alternative is a seventh member, not a
different procedure. Decide now: adding one later is another genesis.

---

## 1. Pre-flight: fix `deploy.py ids` before running it

**Don't run `ids` as it stands.** Checked against the code on 2026-09-23, it
would write a broken genesis in two ways:

1. **Duplicate members.** `cmd_ids` loops over `LIVE` (all five agents,
   including both fcguests since 2026-09-23). Then it *also* runs a
   hardcoded "staging" loop for `ryzen-akuma-amd64` and `mac-akuma-aarch64`
   (`deploy.py`, the `for name in ("ryzen-akuma-amd64", "mac-akuma-aarch64")`
   block). So both fcguests end up in `MIOT_MEMBERS`/`MIOT_ROSTER` twice.
   The current `mesh.env` was generated when `LIVE` had three entries, so it
   doesn't show this. `deploy.sh ids` has the same bug.
2. **Wrong roster labels.** `cmd_ids` labels roster entries with the
   *agent id* (`mac-akuma-aarch64=pub:…`). The live `mesh.env` was relabeled
   by hand to *persona* names (`mimi=pub:…`) on 2026-09-22, and `cmd_up`'s
   fcguest identity check matches on the persona name. A regenerated
   `mesh.env` would make that check fail for every fcguest, and `@mimi` in
   the REPL would resolve to nothing. That's the same bug class HANDOFF
   already paid for once.

Minimal fix, in `cmd_ids`:

- The staging loop should cover only fcguests that are *not* in `LIVE`
  (`for name in (n for n in (...) if n not in LIVE)`). A guest in `LIVE`
  already has its seed inside it, and `account_of` reads it there.
- Label roster entries with `agent(name).persona`, not `name`.
- Change the header comment from "5-agent mesh" to plain "the mesh".

Then check it without touching a host:

```bash
python3 overlays/deploy/deploy.py --dry-run ids   # prints the plan; the local staging still runs cargo
```

`deploy.sh ids` is left broken. Say so in its header, or stop using it for
genesis: `deploy.py` is the one to use from here on (`CLAUDE.md`).

---

## 2. Provision the instance

1. **Launch** the instance (arch and distro per §0). 8 GB gp3 is plenty:
   the binary is about 10 MB, and ParityDB compacts.
2. **Elastic IP.** Allocate one and associate it. Call it `$AWS_IP` below.
   Peer URLs are config, but the home side hardcodes this address, so it
   has to stay put.
3. **Security group**, inbound:
   - `22/tcp` from your home public IP `/32` only
   - `9944/tcp` from your home public IP `/32` only

   mTLS already refuses a stranger at the handshake (`certificate_required`,
   `docs/MESH_AUTH.md`). The source restriction is defence in depth. It also
   keeps the internet's scanners out of the node's logs. If you also want to
   reach the AWS node from off the home LAN (the coffee shop), add that
   network's IP when you need it. Don't open `0.0.0.0/0` for convenience.
4. **Root SSH.** `deploy.py`'s `linux` shape runs every command as the SSH
   user, with no `sudo`, against `/root/kot`, the same as `ryzen`. The
   smallest change that keeps it that way:

   ```bash
   ssh ec2-user@$AWS_IP   # ubuntu@ on Ubuntu
   sudo install -d -m 700 /root/.ssh
   sudo cp ~/.ssh/authorized_keys /root/.ssh/authorized_keys   # replaces the AMI's "please login as ec2-user" stub
   sudo sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
   sudo systemctl reload sshd   # `ssh` on Ubuntu
   ```

5. **SSH alias** in `~/.ssh/config`, in the same style as `ryzen`:

   ```
   Host aws-kot
       HostName <AWS_IP>
       User root
       IdentityFile ~/.ssh/id_ed25519
   ```

   Check with `ssh -o BatchMode=yes aws-kot true`. `deploy.py` uses
   `BatchMode`, so an interactive prompt counts as a failure.

Nothing else goes on the instance: no packages, no llama.cpp, no Docker.

---

## 3. Make the home nodes reachable from AWS

Election and replication run in **both directions**. Every node polls
`/mesh/status` on every peer, a candidate POSTs `/mesh/vote` to every peer,
and a replica pulls `/chain/*` from the primary. The AWS node therefore
has to reach **each** home member, not just one gateway. Outbound from home
to AWS already works. Inbound is what's missing.

The home nodes keep their LAN addresses between themselves, and only the
AWS node's view goes through the router, so no hairpin NAT is needed. You
need one port forward per home member on the home router:

| router WAN port | → LAN target | member |
|---|---|---|
| `9944` | `192.168.1.123:9944` | dumpster-akuma-amd64 |
| `9945` | `192.168.1.126:9944` | ryzen-linux-amd64 |
| `9946` | `192.168.1.203:9944` | mac-linux-aarch64 (Lima's `0.0.0.0` forward of `fc:9944`) |
| `9947` | `192.168.1.203:9945` | mac-akuma-aarch64 (`kot-relay-mac-akuma-aarch64` socat in `fc`) |
| `9948` | `192.168.1.50:9944` | ryzen-akuma-amd64 (the guest's own LAN address, proxy-ARP) |

- **Pin the LAN addresses** with DHCP reservations on the router, the mac's
  `192.168.1.203` especially: it's the one most likely to drift. Only the
  router changes here. Nothing new goes on `ryzen`, `akuma` or `fc`, so no
  new taps or NAT (see the memory rule: reuse what's there).
- **Restrict the source** to `$AWS_IP` if the router supports it.
- **Home public IP.** Call it `$HOME_IP`. If your ISP changes it, the AWS
  node's `MIOT_PEERS` goes stale. That's config, not genesis, so the fix is
  editing `route()` and running `deploy.py up aws-linux-aarch64`, with no
  new chain. A dynamic-DNS name works just as well in the URL: the TLS
  verifier ignores the server name and pins only the key (`tls.rs`,
  `_server_name`). Also update the security group's source IP.

**Check each forward before going further.** From the instance:
`curl -sk https://<HOME_IP>:9945/ -o /dev/null -w '%{http_code}\n'` should
fail at the TLS layer with a certificate-required alert, not time out. A
timeout means the forward or a host firewall is wrong. This doesn't work
until the home nodes are on the mTLS binary (§6). Before that, a
plain-`http` curl proves the same reachability.

---

## 4. Edit `overlays/deploy/deploy.py`

All in one file. `deploy.sh` isn't touched (§1).

1. **The agent row**, appended to `AGENTS`:

   ```python
   Agent("aws-linux-aarch64", "linux", "aws-kot", "aarch64", "yuki", "glm", "glm-5.3"),
   ```

2. **`LIVE`**: append `"aws-linux-aarch64"` **last** (§0, leader choice).
3. **`route()`**, both parts:
   - **Every URL becomes `https://`.** That covers all the `http://`
     literals in `route()`, including the two `special` entries. This is
     the mTLS cutover `docs/MESH_AUTH.md` left for the redeploy. A
     leftover `http://` peer fails to connect, and nothing else tells you.
   - **AWS rows:**

     ```python
     if frm == "aws-linux-aarch64":
         return {
             "dumpster-akuma-amd64": "https://<HOME_IP>:9944",
             "ryzen-linux-amd64":    "https://<HOME_IP>:9945",
             "mac-linux-aarch64":    "https://<HOME_IP>:9946",
             "mac-akuma-aarch64":    "https://<HOME_IP>:9947",
             "ryzen-akuma-amd64":    "https://<HOME_IP>:9948",
         }[to]
     if to == "aws-linux-aarch64":
         return "https://<AWS_IP>:9944"
     ```

     Put the `frm == aws` block **first** in `route()`, above the
     `to == …` rules, which are all LAN addresses the AWS node can't use.
4. **Persona:** write `crates/kot/personas/yuki.md` in the shape of the
   other five. `ship_binary` refuses to ship without it.

Then read every env file before anything ships:

```bash
for a in dumpster-akuma-amd64 ryzen-linux-amd64 mac-linux-aarch64 \
         mac-akuma-aarch64 ryzen-akuma-amd64 aws-linux-aarch64; do
  echo "== $a"; python3 overlays/deploy/deploy.py env $a | grep -E 'PEERS|NAME|LLM|GLM'
done
```

Each one should list 5 peers, all `https://`. The home nodes' peer lists
should include `https://<AWS_IP>:9944`, and AWS's should be all
`<HOME_IP>`.

---

## 5. Build, test, generate the new genesis

Do this **on the home LAN**: `ids` SSHes to every live host.

```bash
cargo test --workspace                     # the mTLS tests + election.rs's 3-node mesh
overlays/local/build.sh all                # dist/{aarch64,x86_64}/kot, with mTLS

cp overlays/deploy/mesh.env overlays/deploy/mesh.env.pre-aws   # rollback, §8
python3 overlays/deploy/deploy.py ids
```

`ids` ships `kot` to each host and runs `kot id` there. On the five
existing hosts, this is a **no-op for the key**: `load_or_create_identity`
never overwrites an existing seed. Only `aws-linux-aarch64` mints a new
one (real `getrandom`, `0600`, `/root/kot/id_ed25519.seed`, and it never
leaves the instance).

**Check the output. Don't trust it:**

```bash
diff <(tr , '\n' < <(grep ^MIOT_ROSTER overlays/deploy/mesh.env.pre-aws | cut -d= -f2-)) \
     <(tr , '\n' < <(grep ^MIOT_ROSTER overlays/deploy/mesh.env | cut -d= -f2-))
```

The only difference should be **one added line**, `yuki=pub:<64 hex>`.
Anything else is a problem: a changed key for an existing cat, an agent-id
label instead of a persona name, or a duplicate. If you see one, stop. §1
wasn't applied, or a seed went missing on a host. `MIOT_MEMBERS` should
list 7 accounts (root + 6), and `MIOT_LEADER` should be unchanged.

`mesh.env` holds public keys only and is safe to commit (`KEY_MANAGEMENT.md`).

---

## 6. Coordinated restart onto the new genesis

**Stop everything, wipe every chain log, then start everything.** A rolling
upgrade doesn't work for two reasons:

- **A node can't detect an old log under a new genesis.** `genesis()` in
  `node.rs` sets the genesis hash to a constant (`H256::zero()`) instead of
  deriving it from `members`. A node restarted on the new `mesh.env` over
  its old `/root/kot/db` would replay the old chain, and adopt an old
  checkpoint, without complaint, while its peers run a different one. Move
  the old logs aside by hand; nothing will refuse them for you.
- **mTLS is a hard cutover** (`docs/MESH_AUTH.md`). Old binaries speak
  `http://` and new ones `https://`, so the two can't talk.

**Stop:**

```bash
ssh ryzen  'systemctl stop kot.service'
limactl shell fc -- sudo systemctl stop kot.service
# akuma + both fcguests are herd: disable first, or herd restarts it within seconds
ssh akuma  'rm -f /etc/herd/enabled/kot.conf; for p in $(ps | grep "/root/kot/bin/kot run" | grep -v grep | awk "{print \$1}"); do kill $p; done'
# ...then the same rm + kill on mac-akuma-aarch64 (ssh -p 4444 root@localhost)
#    and ryzen-akuma-amd64 (ssh -p 2222 -i <amd64 test key> root@192.168.1.50)
```

On the akuma box, send it as **one** SSH exec, the way it's written above
(HANDOFF traps: batch SSH execs there, and let herd do the killing where
it can).

**Move the old logs aside on all five home hosts.** Don't delete them; §8
needs them.

```bash
mv /root/kot/db /root/kot/db.pre-aws-2026-09-23 && mkdir -p /root/kot/db
```

The AWS instance has no log yet.

**Start:**

```bash
python3 overlays/deploy/deploy.py up all
```

`up all` goes through `LIVE` in order. It re-enables herd on the akuma
shapes and writes and starts `kot.service` on the Linux ones. For AWS it
also ships the z.ai token, and prints `curl https://<AWS_IP>:9944/mesh/peers`
at the end. That curl gets refused without a client cert, which is correct.

---

## 7. Verify: every direction, not just one

```bash
R="$(grep ^MIOT_ROSTER overlays/deploy/mesh.env | cut -d= -f2-)"
for n in https://192.168.1.123:9944 https://192.168.1.126:9944 \
         https://192.168.1.203:9944 https://192.168.1.203:9945 \
         https://192.168.1.50:9944  https://<AWS_IP>:9944; do
  echo "== $n"; kot --node $n --roster "$R" peers
done
```

The client signs as root (`~/.akuma/miot/id_ed25519.seed`), which is a
trusted signer. **Always pass `--roster "$R"`.** Leave it out and `kot`
falls back to `DEV_ROSTER` (the footgun in `KEY_MANAGEMENT.md`).

**Stable** looks the same as `run-the-mesh.md` describes: all six views
agree on one `leader`, heads within a block or two, and the term not
climbing. Also check:

- **From AWS's own vantage.** This is the one that proves the port forwards:
  `ssh aws-kot "/root/kot/bin/kot --seed-file /root/kot/id_ed25519.seed --node https://127.0.0.1:9944 --roster '$R' peers"`.
  All five home members should answer, with no "last heard" climbing.
- **The logs:** `ssh aws-kot journalctl -u kot -f` shows `[mesh] following …`
  (or `elected primary`), block sync, and `yuki`'s agent loop connecting.
- **A cat on AWS actually works:**
  `kot --node https://192.168.1.126:9944 --roster "$R" say --to yuki "hi"`.
  A targeted `say` wakes it; an untargeted one wouldn't (known gap,
  `CLAUDE.md`).
- **A stranger gets nothing.** From a host that isn't in genesis,
  `openssl s_client -connect <AWS_IP>:9944` should end in a TLS1.3
  `certificate_required` alert. From outside the security group's `/32`,
  it should just time out.
- **A partition heals.** Stop `kot` on AWS for a minute, then start it
  again. It should replay, pull, and follow, with the term unchanged or
  bumped once, not climbing.

---

## 8. Rollback

Every step keeps what it replaced:

1. Stop everything (§6, **Stop**).
2. `cp overlays/deploy/mesh.env.pre-aws overlays/deploy/mesh.env`, then
   remove `aws-linux-aarch64` from `LIVE`. Keep the `https://` in `route()`:
   the new binary needs it, with or without AWS.
3. On each home host, move `db` aside and put `db.pre-aws-2026-09-23` back.
4. `python3 overlays/deploy/deploy.py up all`.

That restores the old genesis on the new binary. The AWS seed stays on the
instance, where it's harmless: no genesis names it.

---

## 9. Afterwards

- **Docs:** `docs/TOPOLOGY_TARGET.md` (a sixth row plus the quorum note),
  `docs/runbooks/run-the-mesh.md` (every `http://…:9944` becomes `https://`),
  `docs/FLEET.md` (the AWS host), and the redeploy paragraph in `CLAUDE.md`.
  Record the verification in `docs/RESULTS.md` if it's worth numbers.
- **Cost of the link:** mesh traffic is a 1 s `/mesh/status` poll and a 2 s
  `/chain/*` sync to five peers across the WAN. That's small but constant.
  If AWS egress shows up on the bill, `MIOT_POLL_MS`/`MIOT_SYNC_MS` are
  per-node config (not genesis) and can be raised on AWS alone.
- **What this doesn't give you:** a replicated commit (`CLAUDE.md`,
  "Election ≠ replication"). If AWS becomes primary and dies before a home
  node pulls its last blocks, those records are lost to the rewind, the
  same as for any other member, but more likely across a WAN.
