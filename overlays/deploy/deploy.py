#!/usr/bin/env python3
"""Deploy the 5-agent mesh (docs/TOPOLOGY_TARGET.md). One blueprint, three
shapes, and only the transport and the supervisor differ between them:

  akuma    bare-metal Akuma: HTTP pull (no scp, no big ssh exec), herd service
  linux    plain Linux over ssh: scp, systemd unit
  lima     plain Linux inside Lima `fc`: limactl copy, systemd unit
  fcguest  Akuma in Firecracker inside Lima `fc` (akuma-guest): the akuma
           shape, reached through fc — ssh on the mac's :4444 (fc's socat to
           10.0.2.15:22), HTTP pulled through the guest's NAT from the mac's
           loopback (192.168.5.2). Boot it first: ../akuma
           overlays/devbox-firecracker/{guest-setup,build,run}.sh.
           ryzen-akuma-amd64 (the same on ryzen's real KVM) is staged only.

  overlays/deploy/deploy.py ids           create each agent's identity once, write mesh.env
  overlays/deploy/deploy.py up <agent>    ship kot + persona + config, (re)start it
  overlays/deploy/deploy.py up all
  overlays/deploy/deploy.py llama <agent> its own llama-server (linux shapes)
  overlays/deploy/deploy.py retire-old    stop and disable the node1..node5-era services
  overlays/deploy/deploy.py env <agent>   print the agent's env file, don't ship it
  --dry-run anywhere: print what would run on the remote host instead of
  running it, and skip every ship/`put`. Nothing that touches a remote host
  or the local filesystem outside a throwaway temp dir actually happens.

Identities are generated ONCE and never regenerated: `kot id` only creates
a seed file that doesn't exist. Seeds stay on their own host (0600). The
two Firecracker agents' seeds are staged on the mac (~/.akuma/kot/) until
their guests exist. Only public accounts land in mesh.env, which is safe to
commit.

Python rewrite of the original deploy.sh (2026-09-23), following the
pattern `../akuma/scripts/box/` already uses for the bare-metal box's own
build wrappers (see that dir's README): the per-shape startup wrapper,
service unit and herd conf are checked-in templates (`templates/*.tmpl`)
filled in and shipped, not heredocs authored inline — and every wrapper
that runs on a box sources one generated env file, because an sshd session
on Akuma inherits none. The concrete reliability win over the shell version
this buys, on the specific shape `CLAUDE.md` calls "the least reliable
part": every remote command here is an argv list handed straight to
subprocess, never a hand-quoted string re-interpreted by a second shell.
"""
from __future__ import annotations

import argparse
import datetime
import functools
import hashlib
import http.server
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
TEMPLATES = HERE / "templates"
MESH_ENV = HERE / "mesh.env"
MAC_LAN = "192.168.1.203"
HTTP_PORT = 8765  # the mac serves a staged file to the akuma/fcguest boxes on this
# `scp`/`limactl copy` in `put()` had no timeout at all until 2026-09-25:
# ryzen (linux shape), same night as the akuma-shape fix above, measured
# ~120 KB/s over what's normally a fast home-LAN hop — the 13 MB kot took
# over a minute and scp was still going. Bound it instead of hanging
# forever on a slow or wedged link.
PUT_TIMEOUT = 600
AKUMA_REPO = Path(os.environ.get("AKUMA_REPO", str(ROOT.parent / "akuma")))

DRY_RUN = False


def say(msg: str) -> None:
    print(f"\033[1;36m[deploy]\033[0m {msg}", file=sys.stderr)


def die(msg: str) -> NoReturn:
    print(f"\033[1;31m[deploy] {msg}\033[0m", file=sys.stderr)
    sys.exit(1)


def template(name: str, **vars) -> str:
    return (TEMPLATES / name).read_text().format(**vars)


# ---- the table --------------------------------------------------------------
# The litter leader is the first row. Its persona is meow-chan; dumpster-akuma-amd64
# runs GLM, the strongest model in the litter, and planning is the hardest job.
@dataclass(frozen=True)
class Agent:
    name: str
    shape: str  # akuma | linux | lima | fcguest
    host: str
    arch: str  # x86_64 | aarch64
    persona: str
    llm: str  # a base URL, or "glm"
    model: str


AGENTS: list[Agent] = [
    Agent("dumpster-akuma-amd64", "akuma", "akuma", "x86_64", "meow", "glm", "glm-5.3"),
    Agent("ryzen-linux-amd64", "linux", "ryzen", "x86_64", "tama", "http://127.0.0.1:8081", "qwen3-4b"),
    Agent("mac-linux-aarch64", "lima", "fc", "aarch64", "kuro", "http://192.168.5.2:8083", "qwen3:4b"),
    Agent("ryzen-akuma-amd64", "fcguest", "ryzen", "x86_64", "sora", "http://192.168.1.49:8082", "qwen3-4b"),
    Agent("mac-akuma-aarch64", "fcguest", "fc", "aarch64", "mimi", "http://192.168.5.2:8084", "qwen3:4b"),
]
AGENTS_BY_NAME = {a.name: a for a in AGENTS}

# dumpster-akuma-amd64's replica wedge (HANDOFF traps) did not reproduce on a
# fresh redeploy, 2026-09-23 — stayed up as a replica for several minutes,
# agent loop ran real turns. Not root-caused, not proven fixed, just back in
# rotation on the strength of that run; watch for a recurrence.
LIVE = ["dumpster-akuma-amd64", "ryzen-linux-amd64", "mac-linux-aarch64", "mac-akuma-aarch64", "ryzen-akuma-amd64"]

# Members this script never ships to: deployed by something else, but in the
# genesis roster and every live agent's peer list all the same. The AWS kot
# kots live here — ../akuma-terraform/akuma-stack-aws's kotctl deploys them and
# mints their seeds on that box (docs/runbooks/deploy-aws-node.md). Roster name
# -> (account hex, URL every home agent reaches it at). Appended after the
# agents, so the leader (the first agent) never moves.
#   "yuki":  ("<64 hex from `kotctl add yuki`>",  "https://kot.akuma.sh:9441"),
#   "shiro": ("<64 hex from `kotctl add shiro`>", "https://kot.akuma.sh:9442"),
EXTERNAL: dict[str, tuple[str, str]] = {
    # Minted by `kotctl add` on the AWS box, 2026-09-23. Seeds stay there.
    "yuki": ("9ca559eca70a9e0352176e1a21ca74c629024d954698ad30159dad0008c4f346", "https://kot.akuma.sh:9441"),
    "shiro": ("105cf12aa96159bae61fe7ba84c7c38e8982df8436e4420953a32b7f9b89e56c", "https://kot.akuma.sh:9442"),
}

# The litter leader (who plans), by roster name. Genesis: changing it is a new
# chain. yuki since 2026-09-23 — the chain started on AWS with yuki and shiro
# alone, the home agents joining it later (docs/runbooks/deploy-aws-node.md).
LEADER = "yuki"

# agent -> (gguf path on its host, port, threads, bind address, slots)
#
# ryzen's two cats share ONE server since 2026-09-25: a server each (~5 GB
# apiece at -c 8192) ran the 13.7 GB box out of memory — both ended up in
# zram, which is RAM too, and it wedged for 20 minutes. Two slots, -c split
# across them, so each cat keeps 8192. Capped (`MemoryMax`, no swap) so a
# runaway server is killed and restarted instead of taking the box down.
LLAMAS: dict[str, tuple[str, int, int, str, int]] = {
    "ryzen-linux-amd64": ("/root/models/gguf/Qwen3-4B-Instruct-2507-Q4_K_M.gguf", 8081, 8, "127.0.0.1", 2),
}
# agent -> (the agent whose server it shares, the address:port the agent
# dials). sora's config still says 192.168.1.49:8082 (ryzen's tap0), so a
# socket there forwards to the shared server — no redeploy into the guest.
# `FreeBind` lets it bind before tap0 exists.
LLAMA_PROXIES: dict[str, tuple[str, str]] = {
    "ryzen-akuma-amd64": ("ryzen-linux-amd64", "192.168.1.49:8082"),
}
LLAMA_MEMORY_MAX = "7G"
LLAMA_CTX_PER_SLOT = 8192


def agent(name: str) -> Agent:
    a = AGENTS_BY_NAME.get(name)
    if a is None:
        die(f"unknown agent {name}")
    return a


# Every node's view of every *other* live node. Differs per vantage point:
# the fc VM reaches the LAN directly, and the LAN reaches mac-linux-aarch64 through
# Lima's 0.0.0.0 forward of fc:9944 (../akuma host-setup.sh LIMA_LAN_PORTS).
# mac-linux-aarch64 and mac-akuma-aarch64 share fc and talk over its tap0 (10.0.2.2 is fc,
# 10.0.2.15 the guest); the LAN reaches mac-akuma-aarch64 through fc:9945, a socat relay
# (kot-relay-mac-akuma-aarch64.service) that Lima exposes as mac:9945.
def route(frm: str, to: str) -> str:
    special = {
        ("mac-akuma-aarch64", "mac-linux-aarch64"): "https://10.0.2.2:9944",
        ("mac-linux-aarch64", "mac-akuma-aarch64"): "https://10.0.2.15:9944",
    }
    if (frm, to) in special:
        return special[(frm, to)]
    if to == "dumpster-akuma-amd64":
        # .123 until 2026-09-24, when DHCP moved the box to .120 (ARP:
        # vaporwave.lan). No reservation yet, so check `arp -a` if it moves again.
        return "https://192.168.1.120:9944"
    if to == "ryzen-linux-amd64":
        return "https://192.168.1.126:9944"
    if to == "mac-linux-aarch64":
        return f"https://{MAC_LAN}:9944"
    if to == "mac-akuma-aarch64":
        return f"https://{MAC_LAN}:9945"
    if to == "ryzen-akuma-amd64":
        # Akuma/amd64 in Firecracker on ryzen's real KVM, on ryzen's existing
        # guest network: tap0 + proxy-ARP, the guest's pinned lease is a real
        # LAN address, so everyone reaches it directly. No relay.
        return "https://192.168.1.50:9944"
    die(f"no route to {to} yet")


# ---- transports ---------------------------------------------------------------
def on(a: Agent, cmd: str) -> str:
    """Run `cmd` as root on `a`'s host over its shape's transport, return stdout."""
    if a.shape == "akuma":
        argv = ["ssh", "-o", "BatchMode=yes", "akuma", cmd]
        # Generous, same reason as fcguest below: measured live 2026-09-25,
        # a 13 MB x86_64 kot into the bare-metal box's wget ran at ~100-
        # 150 KB/s — 60s wasn't enough, and three `deploy.py up` attempts in
        # a row all timed out mid-transfer looking exactly like the box's
        # documented "can't spawn a second process" wedge (a plain ssh
        # command still answered instantly). A live `wget -O ... ; echo
        # done` run to completion, watched, showed it was just slow, not
        # stuck — 30% at 35s, climbing steadily. Don't mistake a slow
        # `put` on this box for that wedge again without watching a live
        # transfer first.
        timeout = 600
    elif a.shape == "linux":
        argv = ["ssh", "-o", "BatchMode=yes", a.host, cmd]
        timeout = 60
    elif a.shape == "lima":
        argv = ["limactl", "shell", "fc", "--", "sudo", "sh", "-c", cmd]
        timeout = 60
    elif a.shape == "fcguest":
        opts = ["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]
        # Generous: an 11 MB kot takes ~2 min into the amd64 guest (~90 KB/s
        # inbound, measured 2026-09-22) and the ssh session carries the wget.
        timeout = 600
        if a.name == "mac-akuma-aarch64":
            argv = ["ssh", *opts, "-p", "4444", "root@localhost", cmd]
        elif a.name == "ryzen-akuma-amd64":
            # The amd64 image's sshd trusts mkdisk.sh's test key only.
            key = AKUMA_REPO / "target/x86_64-unknown-none/release/amd64-ssh-test-key"
            argv = ["ssh", *opts, "-p", "2222", "-i", str(key), "root@192.168.1.50", cmd]
        else:
            die(f"{a.name}: fcguest shape has no route defined")
    else:
        die(f"{a.name}: shape {a.shape} not deployable yet")

    if DRY_RUN:
        say(f"[dry-run] on {a.name}: {cmd}")
        return ""
    r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    if r.returncode != 0:
        die(f"{a.name}: {' '.join(argv[:2])}...: {cmd!r} failed (exit {r.returncode}): {r.stderr.strip() or r.stdout.strip()}")
    return r.stdout


def _port_free(port: int, bind: str) -> bool:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.bind((bind if bind != "0.0.0.0" else "", port))
        return True
    except OSError:
        return False
    finally:
        s.close()


def _put_via_http(a: Agent, src: Path, dst: str) -> None:
    # HTTP only: no SFTP subsystem, and an ssh exec channel stalls at
    # exactly 1 MiB. Serve a staging dir holding just this file. The guest
    # reaches the mac's loopback as 192.168.5.2 (Lima), so it needs no
    # LAN-facing listener; the metal box needs the LAN one.
    bind, frm = "0.0.0.0", MAC_LAN
    # mac-akuma-aarch64 reaches the mac's loopback as 192.168.5.2 (Lima); ryzen-akuma-amd64
    # comes out through ryzen's NAT onto the LAN like the metal box does.
    if a.name == "mac-akuma-aarch64":
        bind, frm = "127.0.0.1", "192.168.5.2"

    if not _port_free(HTTP_PORT, bind):
        die(f"port {HTTP_PORT} already has a listener (a stale http.server?)")

    stage = Path(tempfile.mkdtemp())
    try:
        data = src.read_bytes()
        (stage / "f").write_bytes(data)
        want = hashlib.md5(data).hexdigest()

        handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(stage))
        httpd = http.server.ThreadingHTTPServer((bind, HTTP_PORT), handler)
        httpd_thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        httpd_thread.start()
        try:
            out = on(a, f"wget -q -O {dst}.new http://{frm}:{HTTP_PORT}/f && md5sum {dst}.new")
            if not DRY_RUN and want not in out:
                die(f"transfer of {src} to {a.name} failed or corrupted")
        finally:
            httpd.shutdown()
            httpd.server_close()
    finally:
        shutil.rmtree(stage, ignore_errors=True)
    on(a, f"chmod +x {dst}.new 2>/dev/null; mv {dst}.new {dst}")


def put(a: Agent, src: Path, dst: str) -> None:
    """Copy local file `src` to `dst` on `a`'s host, atomically (via `dst.new` + rename)."""
    if DRY_RUN:
        say(f"[dry-run] put {a.name}: {src} -> {dst}")
        return
    if a.shape == "linux":
        r = subprocess.run(["scp", "-q", str(src), f"{a.host}:{dst}.new"], capture_output=True, text=True, timeout=PUT_TIMEOUT)
        if r.returncode != 0:
            die(f"{a.name}: scp {src} failed: {r.stderr.strip()}")
        on(a, f"mv {dst}.new {dst}")
    elif a.shape == "lima":
        staged = f"/tmp/{Path(dst).name}.new"
        r = subprocess.run(["limactl", "copy", str(src), f"fc:{staged}"], capture_output=True, text=True, timeout=PUT_TIMEOUT)
        if r.returncode != 0:
            die(f"{a.name}: limactl copy {src} failed: {r.stderr.strip()}")
        on(a, f"mv {staged} {dst}")
    elif a.shape in ("akuma", "fcguest"):
        _put_via_http(a, src, dst)
    else:
        die(f"{a.name}: shape {a.shape} not deployable yet")


# ---- identities -----------------------------------------------------------------
def ship_binary(a: Agent) -> None:
    kot_bin = ROOT / "dist" / a.arch / "kot"
    if not DRY_RUN and not (kot_bin.exists() and kot_bin.stat().st_mode & 0o111):
        die(f"no dist/{a.arch}/kot — overlays/local/build.sh {a.arch}")
    on(a, "mkdir -p /root/kot/bin /root/kot/db")
    put(a, kot_bin, "/root/kot/bin/kot")
    on(a, "chmod 755 /root/kot/bin/kot")
    persona = ROOT / "crates/kot/personas" / f"{a.persona}.md"
    put(a, persona, "/root/kot/persona.md")


def account_of(a: Agent) -> str:
    """The first line `kot id` prints is the account hex.

    2>/dev/null on the far side: Akuma's sshd merges stderr into stdout.
    """
    out = on(a, "/root/kot/bin/kot --seed-file /root/kot/id_ed25519.seed id --comment " + a.name + " 2>/dev/null")
    return out.splitlines()[0].strip() if out else ""


def _kot_id_local(seed_file: Path, comment: str) -> str:
    r = subprocess.run(
        ["cargo", "run", "-q", "--release", "-p", "kot", "--", "--seed-file", str(seed_file), "id", "--comment", comment],
        capture_output=True, text=True, cwd=ROOT,
    )
    if r.returncode != 0:
        die(f"kot id --seed-file {seed_file} failed: {r.stderr.strip()}")
    return r.stdout.splitlines()[0].strip()


def cmd_ids() -> None:
    accounts: list[tuple[str, str]] = []
    for name in LIVE:
        a = agent(name)
        say(f"{name}: shipping kot, creating identity if missing")
        ship_binary(a)
        acct = account_of(a)
        if not DRY_RUN and not (len(acct) == 64 and all(c in "0123456789abcdef" for c in acct)):
            die(f"{name}: kot id printed {acct!r}")
        accounts.append((name, acct))

    # A guest that's live already holds its seed (moved in by `up`) and was
    # read above; staging it again would list the same key twice.
    for name in (n for n in ("ryzen-akuma-amd64", "mac-akuma-aarch64") if n not in LIVE):
        say(f"{name}: staging identity on the mac (moves into its guest rootfs later)")
        seed_file = Path.home() / ".akuma/kot" / f"{name}.seed"
        acct = _kot_id_local(seed_file, name)
        if not (len(acct) == 64 and all(c in "0123456789abcdef" for c in acct)):
            die(f"{name}: kot id printed {acct!r}")
        accounts.append((name, acct))

    root_pub_path = Path.home() / ".akuma/miot/id_ed25519.pub"
    root_seed_path = Path.home() / ".akuma/miot/id_ed25519.seed"
    root_line = root_pub_path.read_text().strip()
    root_acct = _kot_id_local(root_seed_path, "miot-root")

    # Labeled by persona (the cat's name, what `@name` resolves), not by
    # agent id: the roster is genesis state now, names and all, and
    # `kot run` refuses a roster listing one key twice.
    roster = ["root=pub:" + root_acct] + [f"{agent(name).persona}=pub:{acct}" for name, acct in accounts]
    for name, (acct, _) in EXTERNAL.items():
        if not (len(acct) == 64 and all(c in "0123456789abcdef" for c in acct)):
            die(f"EXTERNAL {name}: {acct!r} is not a 64-hex account")
        roster.append(f"{name}=pub:{acct}")
    names = [e.split("=", 1)[0] for e in roster]
    if LEADER not in names:
        die(f"LEADER {LEADER!r} is not in the roster ({', '.join(names)})")
    # By name: `kot run` resolves MIOT_LEADER through the roster.
    leader = LEADER

    lines = [
        f"# Genesis for the mesh — generated by deploy.py ids, {datetime.date.today().isoformat()}.",
        "# Public keys only. Every node must agree on all of this: it is genesis.",
        "# The roster is the membership: there is no separate members list.",
        f'MIOT_ROOT_PUBKEY="{root_line}"',
        f"MIOT_LEADER={leader}",
        f"MIOT_ROSTER={','.join(roster)}",
        "",
    ]
    MESH_ENV.write_text("\n".join(lines))
    say(f"wrote {MESH_ENV}")
    print(MESH_ENV.read_text(), file=sys.stderr, end="")


# ---- config + service -----------------------------------------------------------
def _read_mesh_env() -> dict[str, str]:
    if not MESH_ENV.exists():
        die("no mesh.env — run: deploy.py ids")
    out: dict[str, str] = {}
    for line in MESH_ENV.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        out[k] = v.strip('"')
    return out


def env_for(name: str) -> str:
    """The agent's env file: genesis + its own row + peers from its vantage."""
    a = agent(name)
    peers = ",".join([route(name, p) for p in LIVE if p != name] + [url for _, url in EXTERNAL.values()])
    mesh = _read_mesh_env()

    lines = [
        f"MIOT_NAME={a.name}",
        "MIOT_PORT=9944",
        "MIOT_DB=/root/kot/db/kot.db",
        f"MIOT_PEERS={peers}",
        "MIOT_SEED_FILE=/root/kot/id_ed25519.seed",
        "MIOT_PERSONA=/root/kot/persona.md",
        f"MIOT_MODEL={a.model}",
    ]
    if a.llm == "glm":
        lines += ["MIOT_GLM=true", "MIOT_GLM_TOKEN_FILE=/root/kot/zai.token"]
    else:
        lines.append(f"MIOT_LLM={a.llm}")
    lines += [
        f'MIOT_ROOT_PUBKEY="{mesh["MIOT_ROOT_PUBKEY"]}"',
        f'MIOT_LEADER={mesh["MIOT_LEADER"]}',
        f'MIOT_ROSTER={mesh["MIOT_ROSTER"]}',
    ]
    return "\n".join(lines) + "\n"


_TMP_FILES: list[Path] = []


def _write_tmp(content: str) -> Path:
    fd, name = tempfile.mkstemp()
    Path(name).write_text(content)
    _TMP_FILES.append(Path(name))
    return Path(name)


def _cleanup_tmp() -> None:
    for p in _TMP_FILES:
        p.unlink(missing_ok=True)
    _TMP_FILES.clear()


def cmd_up(name: str) -> None:
    a = agent(name)
    if not MESH_ENV.exists():
        die("no mesh.env — run: deploy.py ids")

    say(f"{a.name}: shipping")
    ship_binary(a)
    env_content = env_for(a.name)

    if a.llm == "glm":
        put(a, Path.home() / ".akuma/z.ai/token", "/root/kot/zai.token")
        on(a, "chmod 600 /root/kot/zai.token")

    if a.shape == "fcguest":
        # Its identity was generated once on the mac (`ids`); move it in,
        # never over an existing one.
        has_seed = True
        if not DRY_RUN:
            r = on(a, "test -s /root/kot/id_ed25519.seed && echo yes || echo no")
            has_seed = r.strip() == "yes"
        if not has_seed:
            put(a, Path.home() / ".akuma/kot" / f"{a.name}.seed", "/root/kot/id_ed25519.seed")
            on(a, "chmod 600 /root/kot/id_ed25519.seed")

        # mesh.env's MIOT_ROSTER keys by persona name (mimi, sora, ...), not
        # by this script's agent id — relabeled 2026-09-22 (HANDOFF item 6);
        # match on that, not on the agent id, or this always refuses.
        mesh = _read_mesh_env()
        expected = ""
        for entry in mesh.get("MIOT_ROSTER", "").split(","):
            if entry.startswith(f"{a.persona}=pub:"):
                expected = entry.split(":", 1)[1]
        got = account_of(a)
        if not DRY_RUN and got != expected:
            die(f"{a.name}: identity in the guest does not match mesh.env")

        # Only the Lima guest needs a relay: Lima exposes only sockets
        # listening in fc. ryzen-akuma-amd64 has a LAN address of its own.
        if a.name == "mac-akuma-aarch64":
            relay = _write_tmp(template("kot-relay.service.tmpl", agent=a.name))
            if DRY_RUN:
                say(f"[dry-run] limactl copy {relay} -> fc:/etc/systemd/system/kot-relay-{a.name}.service, enable --now")
            else:
                subprocess.run(["limactl", "copy", str(relay), "fc:/tmp/kot-relay.service"], check=True)
                subprocess.run(
                    ["limactl", "shell", "fc", "--", "sudo", "sh", "-c",
                     f"mv /tmp/kot-relay.service /etc/systemd/system/kot-relay-{a.name}.service && "
                     "systemctl daemon-reload && systemctl enable --now kot-relay-" + a.name + ".service >/dev/null 2>&1"],
                    check=True,
                )
            relay.unlink(missing_ok=True)

    if a.shape in ("akuma", "fcguest"):
        # herd has `env =` lines, but a wrapper script is the version-proof
        # shape (docs/TOPOLOGY.md, node5) — the env file is exported with
        # `export` prefixes so a quoted ssh-key line survives.
        exported = "\n".join(f"export {line}" for line in env_content.splitlines())
        start_sh = template("start.sh.tmpl", agent=a.name, env_lines=exported)
        put(a, _write_tmp(start_sh), "/root/kot/start.sh")
        on(a, "chmod 755 /root/kot/start.sh")

        conf = template("kot.conf.tmpl")
        on(a, "mkdir -p /etc/herd/available")
        put(a, _write_tmp(conf), "/etc/herd/available/kot.conf")

        if "NO_ENABLE" not in os.environ:
            on(a, "rm -f /etc/herd/enabled/kot.conf; herd enable kot")
            # Restart = kill: herd restarts it (restart = true), and
            # re-reads /etc/herd/enabled every 20s on its own, so a new
            # conf needs nothing else.
            # Akuma's ps lists every thread: after the first kill ends the
            # process, the rest are gone. `|| true` so that isn't a failure.
            # And after `exec`, Akuma's ps keeps showing the *wrapper's*
            # command line — the running kot is listed as
            # `/bin/sh /root/kot/start.sh`. Matching only `kot run` missed it
            # entirely (found 2026-09-24): nothing was killed, and the old
            # kot kept running next to the new binary on disk.
            on(a, "for p in $(ps | grep -E '/root/kot/bin/kot run|/root/kot/start.sh' | grep -v grep | awk '{print $1}'); do kill $p 2>/dev/null || true; done")
        else:
            say(f"{a.name}: NO_ENABLE set — staged, not enabled/restarted")

    elif a.shape in ("linux", "lima"):
        put(a, _write_tmp(env_content), "/root/kot/kot.env")
        unit = template("kot.service.tmpl", agent=a.name)
        put(a, _write_tmp(unit), "/etc/systemd/system/kot.service")
        on(a, "systemctl daemon-reload && systemctl enable kot.service >/dev/null 2>&1; systemctl restart kot.service")

    _cleanup_tmp()
    say(f"{a.name}: up — curl {route('-', a.name)}/mesh/peers")


# ---- models -----------------------------------------------------------------
def cmd_llama(name: str) -> None:
    a = agent(name)
    if a.name in LLAMA_PROXIES:
        return _llama_proxy(a)
    row = LLAMAS.get(a.name)
    if row is None:
        die(f"{a.name} has no llama-server row")
    gguf, port, threads, bind, slots = row

    have = on(a, "test -x /root/llama.cpp/build/bin/llama-server && echo yes || echo no")
    if not DRY_RUN and have.strip() != "yes":
        die(f"{a.name}: build llama.cpp first (/root/llama.cpp/build/bin/llama-server)")

    unit = template(
        "llama.service.tmpl", agent=a.name, gguf=gguf, port=port, threads=threads, bind=bind,
        slots=slots, ctx=LLAMA_CTX_PER_SLOT * slots, ctx_per_slot=LLAMA_CTX_PER_SLOT, memory_max=LLAMA_MEMORY_MAX,
    )
    put(a, _write_tmp(unit), f"/etc/systemd/system/llama-{a.name}.service")

    # ollama nowhere: it loads whatever it's asked for with its own thread
    # policy, which is exactly the unreserved sharing this setup avoids.
    on(a, "systemctl disable --now ollama.service 2>/dev/null; systemctl daemon-reload && "
          f"systemctl enable llama-{a.name}.service 2>/dev/null; systemctl restart llama-{a.name}.service")
    _cleanup_tmp()
    say(f"{a.name}: llama-server on {bind}:{port} ({threads} threads, {slots} slot(s) x {LLAMA_CTX_PER_SLOT})")


def _llama_proxy(a: Agent) -> None:
    """A socket on the address `a` dials, forwarding to the server it shares."""
    owner_name, listen = LLAMA_PROXIES[a.name]
    owner = agent(owner_name)
    _, port, _, bind, _ = LLAMAS[owner_name]
    unit = f"llama-{a.name}-proxy"
    put(owner, _write_tmp(template("llama-proxy.socket.tmpl", agent=a.name, listen=listen)), f"/etc/systemd/system/{unit}.socket")
    put(owner, _write_tmp(template("llama-proxy.service.tmpl", agent=a.name, owner=owner_name, target=f"{bind}:{port}")),
        f"/etc/systemd/system/{unit}.service")
    on(owner, f"systemctl daemon-reload && systemctl enable --now {unit}.socket")
    _cleanup_tmp()
    say(f"{a.name}: {listen} -> {owner_name}'s llama-server at {bind}:{port}")


# ---- the old fleet -----------------------------------------------------------
def cmd_retire_old() -> None:
    say("ryzen: node2 (systemd miot-node2.service)")
    if not DRY_RUN:
        subprocess.run(
            ["ssh", "-o", "BatchMode=yes", "ryzen",
             "systemctl disable --now miot-node2.service 2>/dev/null; rm -f /etc/systemd/system/miot-node2.service; "
             "systemctl daemon-reload; [ -d /root/miot ] && mv /root/miot /root/miot.retired-$(date +%F) || true"],
        )
    say("akuma: node5 (herd service miot)")
    if not DRY_RUN:
        subprocess.run(
            ["ssh", "-o", "BatchMode=yes", "akuma",
             'rm -f /etc/herd/enabled/miot.conf; for p in $(ps | grep "/root/miot/bin/miot" | grep -v grep | awk "{print \\$1}"); '
             'do kill $p; done; [ -d /root/miot ] && mv /root/miot /root/miot.retired || true'],
        )
    say("fc: node3/kuro went with the old VM (recreated by ../akuma host-setup.sh)")


# ---- CLI ----------------------------------------------------------------------
def main() -> None:
    global DRY_RUN
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--dry-run", action="store_true", help="print what would happen, touch no remote host")
    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("ids", help="create each agent's identity once, write mesh.env")

    up = sub.add_parser("up", help="ship kot + persona + config, (re)start it")
    up.add_argument("agent", help="an agent name, or 'all'")

    llama = sub.add_parser("llama", help="a systemd llama-server for one agent")
    llama.add_argument("agent")

    sub.add_parser("retire-old", help="stop and disable the node1..node5-era services")

    envp = sub.add_parser("env", help="print an agent's env file, don't ship it")
    envp.add_argument("agent")

    args = p.parse_args()
    DRY_RUN = args.dry_run

    if args.cmd == "ids":
        cmd_ids()
    elif args.cmd == "up":
        if args.agent == "all":
            for name in LIVE:
                cmd_up(name)
        else:
            cmd_up(args.agent)
    elif args.cmd == "llama":
        cmd_llama(args.agent)
    elif args.cmd == "retire-old":
        cmd_retire_old()
    elif args.cmd == "env":
        print(env_for(args.agent), end="")


if __name__ == "__main__":
    main()
