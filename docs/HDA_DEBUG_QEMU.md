# Debugging meow's Intel HDA driver under QEMU, on ryzen — plan

Written 2026-09-29. **Nothing here is built yet.** It is a plan, with what
was checked on ryzen today marked as such and everything else marked open.

## Why

meow (`dumpster-akuma-amd64`, the bare-metal akuma box) has been writing an
Intel HDA driver for the Akuma kernel (`../akuma`, branch `amd64-audio`,
brief: `../akuma/docs/runbooks/add-intel-hda-audio.md`; progress in HANDOFF,
"Staged on meow's box for the HDA work"). Every test of it is a boot of the
metal box, and a hang there costs a reboot of the machine meow itself lives
on (HANDOFF: the box hung 2026-09-26; meow's own `reboot -f` loop).

The proposal: **tama** (`ryzen-linux-amd64`) boots the same kernel under QEMU
with an emulated HDA controller, on ryzen, with music coming out of ryzen's
own speakers. A hang costs a VM. **meow supervises** — it owns the driver's
direction and the real hardware, and is the only one who can say "it plays on
the metal box".

## Roles

| who | does | why them |
|---|---|---|
| tama | edits the driver, builds the kernel, boots it under QEMU, reads the serial log, listens | a Linux box with a toolchain (`/root/src/akuma-miot`, rustup), GLM (`glm-5.3-flash`, low) via z.ai, and the speakers |
| meow | supervises: sets the milestones, reviews what tama pushes, does the final run on real hardware | holds the driver so far and the only real HDA controller (`8086:8c20`) |
| sora | **not needed** | Firecracker has no HDA/audio device at all, only virtio |

They talk over the mesh (`say`); code moves through `akuma-litter` (each cat
pushes its own branch, `docs/GIT_HOME.md`).

```
   meow (metal box)                         tama (ryzen)
   ─────────────────                        ────────────────────────────
   amd64-audio branch  ── push litter ──►   pull, edit driver, build kernel
   sets milestones     ◄── say ───────►      boot in QEMU (q35 + intel-hda)
   reviews the diff                          serial log ──► [HDA] lines
   final run on 8c20   ◄── push litter ──    wavplay ──► pipewire ──► speakers
```

## Why QEMU and not Firecracker

Firecracker emulates virtio, serial and a handful of basics. No HDA, no
audio. `../akuma/amd64/run.sh` already boots the kernel under QEMU (`-M
microvm`, "the local stand-in for Firecracker"), but `microvm` has **no PCI
bus** and HDA is a PCI device, so the debug run needs `-M q35` (or `pc`).

## The run

Roughly (final flags are tama's to settle; this is the shape):

```
qemu-system-x86_64 -M q35 -cpu max -accel kvm -m 1024 -smp 2 \
  -kernel target/x86_64-unknown-none/release/akuma-amd64 \
  -audiodev pipewire,id=a0 \
  -device intel-hda -device hda-duplex,audiodev=a0 \
  -serial file:hda.log -display none -no-reboot
```

- `intel-hda` emulates the ICH6 controller (`8086:2668`); `ich9-intel-hda` is
  `8086:293e`. The real target is `8086:8c20`. **The register layout is the
  HDA spec's, the same across them**, so discovery, CORB/RIRB, codec walk,
  stream setup and DMA are all exercised. A quirk specific to `8c20` will not
  show up here, which is what meow's final run is for.
- Speakers: `-audiodev pipewire` (or `pa`/`alsa`). ryzen has pipewire and an
  ALC257 analog codec (`aplay -l`, checked 2026-09-29). The pipewire session
  belongs to `netoneko` (uid 1001), while `kot.service` runs as root; root
  can reach that socket with `XDG_RUNTIME_DIR=/run/user/1001`. Open: confirm.
- The music is meow's `bootstrap/music/tokyo_rider_enter_omegashima.wav`
  (54 MB, 24-bit/44.1 kHz stereo) played by `/bin/wavplay`, which today stops
  at `cannot open /dev/dsp` until a driver registers the device. Neither is on
  ryzen; both are staged on meow's box.

### Order of checks (each one isolates one layer)

1. **Host plays the WAV** (`mpv`/`aplay` on ryzen). Speakers, volume, session.
2. **QEMU's HDA plays it in a Linux guest** (any live image, `aplay`). The
   emulated controller and the audio backend work; anything later is the driver.
3. **Akuma kernel under QEMU, no driver change.** `[HDA]` discovery lines
   appear in the log.
4. **Akuma kernel with meow's driver**, `wavplay` opens `/dev/dsp`, sound comes
   out. "`[HDA] ready` and silence" is **not** a pass (the runbook says so).
5. **meow's run on the metal box** with what tama pushed.

## Guarding ryzen

Checked today: ryzen has 13 GB RAM, ~5 GB available; root disk 96% full, **8.2 GB
free**; `/dev/kvm` present. `kot.service` has no memory cap, no hardening, only
`CPUAffinity=2 3` and it runs as root; the llama-server is capped at 7 GB and
sora's VMM at 2.4 GB. Nothing would stop a QEMU guest from starving them, so
each debug boot is its own transient unit, the way builds already are (`kot-build`,
`MemoryMax=6G`):

- `systemd-run --unit=hda-debug --property=MemoryMax=2G --property=MemorySwapMax=0 --property=RuntimeMaxSec=300 --property=AllowedCPUs=…`
- `RuntimeMaxSec` so a hung guest cannot outlive its run; `-no-reboot` so a
  kernel triple-fault ends the run instead of looping.
- a launcher script kept next to the other ryzen host files, e.g.
  `overlays/deploy/hosts/ryzen/hda-debug.sh`, and named in the `MIOT_CONTEXT`
  prompt files so tama knows it exists. No new `kot` code to begin with — tama
  already has `Bash`. A dedicated tool only if it proves worth it.

## Sora and the mesh

Checked with `kot peers` on tama, 2026-09-29: tama leads (term 281); kuro, mimi,
yuki, shiro are in sync; quorum is 4 of 7. **sora** was 1511 s stale (its
console ends at `[herd] Reloading config...`, no ping answer) and **meow** was
off on purpose. That leaves five current voters, one above quorum, so stopping
`sora.service` (frees 2.4 GB) is safe *as of that reading*, but kuro and mimi
share one Lima VM (`192.168.1.203`); losing it drops the mesh below quorum
until meow is back. Bring meow up first, then decide about sora.

## Open, unchecked

- **QEMU is not installed on ryzen.** Package size vs 8.2 GB free disk.
- **Does the kernel have virtio-PCI?** It drives virtio-MMIO today, which `q35`
  does not have. Without it, the debug guest has no disk and no network, and
  the results come out over the serial log only. Look in `../akuma/amd64/src`
  before assuming either way.
- Whether `-cpu max -accel kvm` + PVH `-kernel` boots the kernel on `q35` the
  way `microvm` does (`amd64/run.sh` only ever used `microvm`).
- The pipewire-as-root detail above.
- Getting the WAV and `wavplay` from the metal box to ryzen.
- Whether `kot.service` itself should get `MemoryMax`/`ProtectSystem` — separate
  change, not part of this plan.
