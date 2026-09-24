#!/usr/bin/env bash
# Deploy the 5-agent mesh (docs/TOPOLOGY_TARGET.md). One blueprint, three
# shapes, and only the transport and the supervisor differ between them:
#
#   akuma  bare-metal Akuma: HTTP pull (no scp, no big ssh exec), herd service
#   linux  plain Linux over ssh: scp, systemd unit
#   lima   plain Linux inside Lima `fc`: limactl copy, systemd unit
#   fcguest  Akuma in Firecracker inside Lima `fc` (akuma-guest): the akuma
#          shape, reached through fc — ssh on the mac's :4444 (fc's socat to
#          10.0.2.15:22), HTTP pulled through the guest's NAT from the mac's
#          loopback (192.168.5.2). Boot it first: ../akuma
#          overlays/devbox-firecracker/{guest-setup,build,run}.sh.
#          ryzen-akuma-amd64 (the same on ryzen's real KVM) is staged only.
#
#   overlays/deploy/deploy.sh ids           create each agent's identity once, write mesh.env
#   overlays/deploy/deploy.sh up <agent>    ship kot + persona + config, (re)start it
#   overlays/deploy/deploy.sh up all
#   overlays/deploy/deploy.sh llama <agent> its own llama-server (linux shapes)
#   overlays/deploy/deploy.sh retire-old    stop and disable the node1..node5-era services
#
# Identities are generated ONCE and never regenerated: `kot id` only creates
# a seed file that doesn't exist. Seeds stay on their own host (0600). The
# two Firecracker agents' seeds are staged on the mac (~/.akuma/kot/) until
# their guests exist. Only public accounts land in mesh.env, which is safe to
# commit.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
MESH_ENV="$HERE/mesh.env"
MAC_LAN=192.168.1.203
HTTP_PORT=8765   # the mac serves dist/ to the akuma box on this
AKUMA_REPO="${AKUMA_REPO:-$ROOT/../akuma}"

say() { printf '\033[1;36m[deploy]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[deploy] %s\033[0m\n' "$*" >&2; exit 1; }

# ---- the table -----------------------------------------------------------------
# name | shape | host | arch | persona | llm (url, or "glm") | model
# The litter leader is the first row. Its persona is meow-chan; dumpster-akuma-amd64
# runs GLM, the strongest model in the litter, and planning is the hardest job.
AGENTS=(
  "dumpster-akuma-amd64|akuma|akuma|x86_64|meow|glm|glm-5.3"
  "ryzen-linux-amd64|linux|ryzen|x86_64|tama|http://127.0.0.1:8081|qwen3-4b"
  "mac-linux-aarch64|lima|fc|aarch64|kuro|http://192.168.5.2:8083|qwen3:4b"
  "ryzen-akuma-amd64|fcguest|ryzen|x86_64|sora|http://192.168.1.49:8082|qwen3-4b"
  "mac-akuma-aarch64|fcguest|fc|aarch64|mimi|http://192.168.5.2:8084|qwen3:4b"
)
# dumpster-akuma-amd64's replica wedge (HANDOFF traps) did not reproduce on a
# fresh redeploy, 2026-09-23 — stayed up as a replica for several minutes,
# agent loop ran real turns. Not root-caused, not proven fixed, just back in
# rotation on the strength of that run; watch for a recurrence.
# mac-akuma-aarch64 runs the same role on aarch64 Akuma to see if it wedges too.
LIVE=(dumpster-akuma-amd64 ryzen-linux-amd64 mac-linux-aarch64 mac-akuma-aarch64 ryzen-akuma-amd64)

# llama-server per agent, never ollama: its own process, its own port, its
# thread count pinned, so an agent's turns get a known slice of the box and
# never queue behind another agent's. The mac's servers are
# overlays/local/llama-swarm.sh (8081-8084, -t 1, Metal).
# agent | gguf on its host | port | threads
LLAMAS=(
  "ryzen-linux-amd64|/root/models/gguf/Qwen3-4B-Instruct-2507-Q4_K_M.gguf|8081|6|127.0.0.1"
  # ryzen-akuma-amd64's own, on ryzen's tap0 address (192.168.1.49) so the guest reaches it.
  "ryzen-akuma-amd64|/root/models/gguf/Qwen3-4B-Instruct-2507-Q4_K_M.gguf|8082|4|192.168.1.49"
)

# Every node's view of every *other* live node. Differs per vantage point:
# the fc VM reaches the LAN directly, and the LAN reaches mac-linux-aarch64 through
# Lima's 0.0.0.0 forward of fc:9944 (../akuma host-setup.sh LIMA_LAN_PORTS).
# mac-linux-aarch64 and mac-akuma-aarch64 share fc and talk over its tap0 (10.0.2.2 is fc,
# 10.0.2.15 the guest); the LAN reaches mac-akuma-aarch64 through fc:9945, a socat relay
# (kot-relay-mac-akuma-aarch64.service) that Lima exposes as mac:9945.
route() { # route <from> <to>
  case "$1>$2" in
    *">dumpster-akuma-amd64") echo http://192.168.1.120:9944 ;;
    *">ryzen-linux-amd64") echo http://192.168.1.126:9944 ;;
    "mac-akuma-aarch64>mac-linux-aarch64") echo http://10.0.2.2:9944 ;;
    *">mac-linux-aarch64") echo "http://$MAC_LAN:9944" ;;
    "mac-linux-aarch64>mac-akuma-aarch64") echo http://10.0.2.15:9944 ;;
    *">mac-akuma-aarch64") echo "http://$MAC_LAN:9945" ;;
    # ryzen-akuma-amd64: Akuma/amd64 in Firecracker on ryzen's real KVM, on ryzen's
    # existing guest network: tap0 + proxy-ARP, the guest's pinned lease is a
    # real LAN address, so everyone reaches it directly. No relay.
    *">ryzen-akuma-amd64") echo http://192.168.1.50:9944 ;;
    *) die "no route to $2 yet" ;;
  esac
}

row() { local r; for r in "${AGENTS[@]}"; do [ "${r%%|*}" = "$1" ] && { echo "$r"; return; }; done; die "unknown agent $1"; }
field() { row "$1" | cut -d'|' -f"$2"; }

# ---- transports ----------------------------------------------------------------
on() { # on <agent> <shell command>, run as root on the agent's host
  case "$(field "$1" 2)" in
    akuma) ssh -o BatchMode=yes akuma "$2" ;;
    linux) ssh -o BatchMode=yes "$(field "$1" 3)" "$2" ;;
    lima)  limactl shell fc -- sudo sh -c "$2" ;;
    fcguest)
      local o=(-o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
      case "$1" in
        # Generous: an 11 MB kot takes ~2 min into the amd64 guest (~90 KB/s
        # inbound, measured 2026-09-22) and the ssh session carries the wget.
        mac-akuma-aarch64) timeout 600 ssh "${o[@]}" -p 4444 root@localhost "$2" ;;
        # The amd64 image's sshd trusts mkdisk.sh's test key only.
        ryzen-akuma-amd64) timeout 600 ssh "${o[@]}" -p 2222 -i "$AKUMA_REPO/target/x86_64-unknown-none/release/amd64-ssh-test-key" root@192.168.1.50 "$2" ;;
      esac ;;
    *) die "$1: shape $(field "$1" 2) not deployable yet" ;;
  esac
}

put() { # put <agent> <local file> <remote path>
  local a="$1" src="$2" dst="$3"
  case "$(field "$a" 2)" in
    linux) scp -q "$src" "$(field "$a" 3):$dst.new" && on "$a" "mv $dst.new $dst" ;;
    lima)  limactl copy "$src" "fc:/tmp/$(basename "$dst").new" && on "$a" "mv /tmp/$(basename "$dst").new $dst" ;;
    akuma|fcguest)
      # HTTP only: no SFTP subsystem, and an ssh exec channel stalls at
      # exactly 1 MiB. Serve a staging dir holding just this file. The
      # guest reaches the mac's loopback as 192.168.5.2 (Lima), so it
      # needs no LAN-facing listener; the metal box needs the LAN one.
      local bind=0.0.0.0 from=$MAC_LAN
      # mac-akuma-aarch64 reaches the mac's loopback as 192.168.5.2 (Lima); ryzen-akuma-amd64
      # comes out through ryzen's NAT onto the LAN like the metal box does.
      [ "$a" = mac-akuma-aarch64 ] && { bind=127.0.0.1; from=192.168.5.2; }
      local stage; stage="$(mktemp -d)"
      cp "$src" "$stage/f"
      if lsof -iTCP:$HTTP_PORT -sTCP:LISTEN -P >/dev/null 2>&1; then
        die "port $HTTP_PORT already has a listener (a stale http.server?) — lsof -iTCP:$HTTP_PORT"
      fi
      (cd "$stage" && exec python3 -m http.server $HTTP_PORT --bind $bind >/dev/null 2>&1) &
      local srv=$!
      sleep 1
      local want; want="$(md5 -q "$src")"
      on "$a" "wget -q -O $dst.new http://$from:$HTTP_PORT/f && md5sum $dst.new" | grep -q "$want" \
        || { kill $srv; wait $srv 2>/dev/null || true; rm -rf "$stage"; die "transfer of $src to $a failed or corrupted"; }
      kill $srv; wait $srv 2>/dev/null || true; rm -rf "$stage"
      on "$a" "chmod +x $dst.new 2>/dev/null; mv $dst.new $dst"
      ;;
  esac
}

# ---- identities ----------------------------------------------------------------
ship_binary() {
  local a="$1" arch; arch="$(field "$a" 4)"
  [ -x "$ROOT/dist/$arch/kot" ] || die "no dist/$arch/kot — overlays/local/build.sh $arch"
  on "$a" "mkdir -p /root/kot/bin /root/kot/db"
  put "$a" "$ROOT/dist/$arch/kot" /root/kot/bin/kot
  on "$a" "chmod 755 /root/kot/bin/kot"
  put "$a" "$ROOT/crates/kot/personas/$(field "$a" 5).md" /root/kot/persona.md
}

account_of() { # the first line `kot id` prints is the account hex
  # 2>/dev/null on the far side: Akuma's sshd merges stderr into stdout.
  on "$1" "/root/kot/bin/kot --seed-file /root/kot/id_ed25519.seed id --comment $1 2>/dev/null" | head -1 | tr -d '\r'
}

cmd_ids() {
  local accounts=() a acct
  for a in "${LIVE[@]}"; do
    say "$a: shipping kot, creating identity if missing"
    ship_binary "$a"
    acct="$(account_of "$a")"
    [[ "$acct" =~ ^[0-9a-f]{64}$ ]] || die "$a: kot id printed '$acct'"
    accounts+=("$a=$acct")
  done
  for a in ryzen-akuma-amd64 mac-akuma-aarch64; do
    say "$a: staging identity on the mac (moves into its guest rootfs later)"
    acct="$(cargo run -q --release -p kot -- --seed-file "$HOME/.akuma/kot/$a.seed" id --comment "$a" 2>/dev/null | head -1)"
    [[ "$acct" =~ ^[0-9a-f]{64}$ ]] || die "$a: kot id printed '$acct'"
    accounts+=("$a=$acct")
  done

  local root_line root_acct members roster
  root_line="$(cat "$HOME/.akuma/miot/id_ed25519.pub")"
  root_acct="$(cargo run -q --release -p kot -- --seed-file "$HOME/.akuma/miot/id_ed25519.seed" id --comment miot-root 2>/dev/null | head -1)"
  members="$root_acct"; roster="root=pub:$root_acct"
  for p in "${accounts[@]}"; do
    members="$members,${p#*=}"
    roster="$roster,${p%%=*}=pub:${p#*=}"
  done
  local leader="${accounts[0]#*=}"
  cat > "$MESH_ENV" <<EOF
# Genesis for the 5-agent mesh — generated by deploy.sh ids, $(date +%F).
# Public keys only. Every node must agree on all of this: it is genesis.
MIOT_ROOT_PUBKEY="$root_line"
MIOT_LEADER=$leader
MIOT_MEMBERS=$members
MIOT_ROSTER=$roster
EOF
  say "wrote $MESH_ENV"
  cat "$MESH_ENV" >&2
}

# ---- config + service ----------------------------------------------------------
env_for() { # the agent's env file: genesis + its own row + peers from its vantage
  local a="$1" peers="" p llm model
  for p in "${LIVE[@]}"; do [ "$p" = "$a" ] || peers="${peers:+$peers,}$(route "$a" "$p")"; done
  llm="$(field "$a" 6)"; model="$(field "$a" 7)"
  # shellcheck disable=SC1090
  . "$MESH_ENV"
  echo "MIOT_NAME=$a"
  echo "MIOT_PORT=9944"
  echo "MIOT_DB=/root/kot/db/kot.db"
  echo "MIOT_PEERS=$peers"
  echo "MIOT_SEED_FILE=/root/kot/id_ed25519.seed"
  echo "MIOT_PERSONA=/root/kot/persona.md"
  echo "MIOT_MODEL=$model"
  if [ "$llm" = glm ]; then
    echo "MIOT_GLM=true"
    echo "MIOT_GLM_TOKEN_FILE=/root/kot/zai.token"
  else
    echo "MIOT_LLM=$llm"
  fi
  echo "MIOT_ROOT_PUBKEY=\"$MIOT_ROOT_PUBKEY\""
  echo "MIOT_LEADER=$MIOT_LEADER"
  echo "MIOT_MEMBERS=$MIOT_MEMBERS"
  echo "MIOT_ROSTER=$MIOT_ROSTER"
}

cmd_up() {
  local a="$1" tmp; tmp="$(mktemp)"
  [ -f "$MESH_ENV" ] || die "no mesh.env — run: $0 ids"
  say "$a: shipping"
  ship_binary "$a"
  env_for "$a" > "$tmp"
  if [ "$(field "$a" 6)" = glm ]; then
    put "$a" "$HOME/.akuma/z.ai/token" /root/kot/zai.token
    on "$a" "chmod 600 /root/kot/zai.token"
  fi
  if [ "$(field "$a" 2)" = fcguest ]; then
    # Its identity was generated once on the mac (`ids`); move it in, never
    # over an existing one.
    if ! on "$a" "test -s /root/kot/id_ed25519.seed"; then
      put "$a" "$HOME/.akuma/kot/$a.seed" /root/kot/id_ed25519.seed
      on "$a" "chmod 600 /root/kot/id_ed25519.seed"
    fi
    # mesh.env's MIOT_ROSTER keys by persona name (mimi, sora, ...), not by
    # this script's agent id — relabeled 2026-09-22 (HANDOFF item 6) without
    # updating this check, so it always compared against an empty grep and
    # would have refused every fcguest identity, matching or not.
    local persona; persona="$(field "$a" 5)"
    [ "$(account_of "$a")" = "$(grep -o "$persona=pub:[0-9a-f]*" "$MESH_ENV" | cut -d: -f2)" ] \
      || die "$a: identity in the guest does not match mesh.env"
    # Only the Lima guest needs a relay: Lima exposes only sockets listening
    # in fc. ryzen-akuma-amd64 has a LAN address of its own.
    [ "$a" = mac-akuma-aarch64 ] && cat > "$tmp.relay" <<EOF
[Unit]
Description=LAN :9945 -> $a (Akuma guest 10.0.2.15:9944)
After=network-online.target

[Service]
ExecStart=/usr/bin/socat TCP-LISTEN:9945,fork,reuseaddr TCP:10.0.2.15:9944
Restart=always

[Install]
WantedBy=multi-user.target
EOF
    if [ "$a" = mac-akuma-aarch64 ]; then
      limactl copy "$tmp.relay" fc:/tmp/kot-relay.service
      limactl shell fc -- sudo sh -c "mv /tmp/kot-relay.service /etc/systemd/system/kot-relay-$a.service && systemctl daemon-reload && systemctl enable --now kot-relay-$a.service >/dev/null 2>&1"
    fi
  fi
  case "$(field "$a" 2)" in
    akuma|fcguest)
      # herd has `env =` lines, but a wrapper script is the version-proof
      # shape (docs/TOPOLOGY.md, node5). The env file is sourced with -a so
      # its quoted ssh-key line survives.
      sed 's/^/export /' "$tmp" > "$tmp.sh"
      { echo '#!/bin/sh'; cat "$tmp.sh"; echo 'exec /root/kot/bin/kot run'; } > "$tmp.start"
      put "$a" "$tmp.start" /root/kot/start.sh
      on "$a" "chmod 755 /root/kot/start.sh"
      printf 'command = /bin/sh\nargs = /root/kot/start.sh\nrestart = true\nrestart_delay = 2000\n' > "$tmp.conf"
      # herd's convention: confs live in available/, `herd enable` copies
      # one into enabled/ (re-read every 20 s). NO_ENABLE=1 stages without
      # enabling — for when the operator has `herd disable`d it on purpose.
      on "$a" "mkdir -p /etc/herd/available"
      put "$a" "$tmp.conf" /etc/herd/available/kot.conf
      if [ -z "${NO_ENABLE:-}" ]; then
        on "$a" "rm -f /etc/herd/enabled/kot.conf; herd enable kot"
      fi
      # Restart = kill: herd restarts it (restart = true), and re-reads
      # /etc/herd/enabled every 20 s on its own, so a new conf needs nothing.
      # Skipped with NO_ENABLE: a hand `kill` on this box preceded a
      # sshd-can't-spawn wedge once (HANDOFF traps); a reboot is safer.
      [ -n "${NO_ENABLE:-}" ] || on "$a" "for p in \$(ps | grep '/root/kot/bin/kot run' | grep -v grep | awk '{print \$1}'); do kill \$p; done"
      ;;
    linux|lima)
      put "$a" "$tmp" /root/kot/kot.env
      cat > "$tmp.unit" <<EOF
[Unit]
Description=kot $a — mesh node + agent loop (akuma-miot)
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=/root/kot/kot.env
ExecStart=/root/kot/bin/kot run
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
      put "$a" "$tmp.unit" /etc/systemd/system/kot.service
      on "$a" "systemctl daemon-reload && systemctl enable kot.service >/dev/null 2>&1; systemctl restart kot.service"
      ;;
  esac
  rm -f "$tmp" "$tmp".*
  say "$a: up — curl $(route - "$a")/mesh/peers"
}

# ---- models --------------------------------------------------------------------
cmd_llama() { # a systemd llama-server for one agent, on its (host's) linux side
  local a="$1" r="" x gguf port threads bind tmp host_agent="$1"
  # A Firecracker guest's model runs on the guest's Linux host.
  [ "$a" = ryzen-akuma-amd64 ] && host_agent=ryzen-linux-amd64
  for x in "${LLAMAS[@]}"; do [ "${x%%|*}" = "$a" ] && r="$x"; done
  [ -n "$r" ] || die "$a has no llama-server row"
  IFS='|' read -r _ gguf port threads bind <<<"$r"
  on "$host_agent" "test -x /root/llama.cpp/build/bin/llama-server" || die "$a: build llama.cpp first (/root/llama.cpp/build/bin/llama-server)"
  tmp="$(mktemp)"
  cat > "$tmp" <<EOF
[Unit]
Description=llama-server for kot $a (port $port, $threads threads)
After=network-online.target

[Service]
ExecStart=/root/llama.cpp/build/bin/llama-server -m $gguf --host $bind --port $port -c 8192 --jinja --parallel 1 -t $threads
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
  put "$host_agent" "$tmp" "/etc/systemd/system/llama-$a.service"
  rm -f "$tmp"
  # ollama nowhere: it loads whatever it's asked for with its own thread
  # policy, which is exactly the unreserved sharing this setup avoids.
  on "$host_agent" "systemctl disable --now ollama.service 2>/dev/null; systemctl daemon-reload && systemctl enable llama-$a.service 2>/dev/null; systemctl restart llama-$a.service"
  say "$a: llama-server on $bind:$port ($threads threads)"
}

# ---- the old fleet -------------------------------------------------------------
cmd_retire_old() {
  say "ryzen: node2 (systemd miot-node2.service)"
  ssh -o BatchMode=yes ryzen 'systemctl disable --now miot-node2.service 2>/dev/null; rm -f /etc/systemd/system/miot-node2.service; systemctl daemon-reload; [ -d /root/miot ] && mv /root/miot /root/miot.retired-$(date +%F) || true'
  say "akuma: node5 (herd service miot)"
  ssh -o BatchMode=yes akuma 'rm -f /etc/herd/enabled/miot.conf; for p in $(ps | grep "/root/miot/bin/miot" | grep -v grep | awk "{print \$1}"); do kill $p; done; [ -d /root/miot ] && mv /root/miot /root/miot.retired || true'
  say "fc: node3/kuro went with the old VM (recreated by ../akuma host-setup.sh)"
}

case "${1:-}" in
  ids) cmd_ids ;;
  up)
    [ -n "${2:-}" ] || die "up <agent>|all"
    if [ "$2" = all ]; then for a in "${LIVE[@]}"; do cmd_up "$a"; done; else cmd_up "$2"; fi
    ;;
  llama) cmd_llama "${2:?agent}" ;;
  retire-old) cmd_retire_old ;;
  env) env_for "${2:?agent}" ;;
  *) sed -n '2,21p' "$0"; exit 2 ;;
esac
