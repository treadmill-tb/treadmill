# Treadmill image builds: Nix `vmTools` → libguestfs

Status: in progress. Replaces the `vmTools`/`mk-treadmill-image.nix` image-build
machinery under `images/` with a libguestfs-driven shell pipeline plus a single
Rust `image-util` binary for OCI layout assembly **and** validation.

This plan is scoped to *how the rootfs and OCI layout get built*. The supervisor
store/launch paths and the `treadmill_rs::image` parser are unchanged except for
a new, symmetric `assemble` counterpart to the existing `parse`.

### Implementation status (2026-06-21)

- **Phase 0 — local mechanics: DONE.** guestfish partition extraction, mtools
  FAT edit, the qcow2 overlay→`rebase`→recompose chain, and the qcow2-header
  virtual-size read all validated under KVM via `nix develop .#images`.
  **Still owed:** dispatch `.github/workflows/images-spike.yml` (throwaway) on
  GitHub to confirm aarch64-runner `/dev/kvm` + real `virt-customize --install`
  timings. Gates Phase 2's *CI* run, not its code.
- **Phase 1 — `image-util`: DONE** (commit `2201cee`). `treadmill_rs::image::assemble`
  added (roundtrip-tested against `parse`); `images/check` → `images/util` with
  `assemble` + `parse` subcommands; `mk-treadmill-image.nix` + `media-types.nix`
  deleted; `nix/images.nix` assembles via `image-util assemble`. The vmTools
  recipes still build (parallel pipeline) until Phase 6.
- **Phase 2 — Ubuntu base: DONE.** `images/lib/build-image.sh` (disk path),
  `images/lib/provision-common.sh`, and `images/ubuntu-server-2604/{manifest,provision}.sh`.
  `image-util assemble`+`parse` (2 root layers) wired and validated.
  - **Renamed + bumped:** the base is **`ubuntu-server-2604`** (Ubuntu Server
    26.04 LTS, "resolute") — the cloud image is the *server* (headless) variant,
    and we track the latest LTS. The legacy `images/ubuntu-2204/{rootfs,default}.nix`
    vmTools recipes are untouched (parallel pipeline; deleted in Phase 6).
  - **No personal mirror.** Base = the pinned dated upstream cloudimg
    (`cloud-images.ubuntu.com/releases/26.04/release-20260612/…`); rustup-init =
    the pinned upstream archive (`static.rust-lang.org/rustup/archive/1.29.0/…`).
    Both sha256-verified on every fetch (drift guard).
- **Phase 5 — CI cutover (Ubuntu): pulled forward / partial.** `images.yml`
  overwritten to drive the libguestfs pipeline (the legacy vmTools workflow was
  not yet rolled out to prod): a `prepare` job maps per-family dispatch
  checkboxes to a matched-arch matrix; the `build` job builds the nix bins, then
  runs `build-image.sh` inside `nix develop .#images` (prebuilt libguestfs
  appliance) + artifact + optional ghcr push (sha+latest).
  Only `ubuntu-server-2604` (x86_64) is live; `raspberrypios-13` (aarch64) is a
  commented placeholder pending Phase 4. **Owed:** a real dispatch to confirm
  hosted-runner `/dev/kvm` + apt timings (the Phase 0 spike, now largely
  subsumed for x86_64 by the workflow itself).
- **Phase 3 — Ubuntu runner overlay: DONE.** `build-image.sh` grew a
  `--variant gha-runner` (+ `--base-delta`) path and
  `images/ubuntu-server-2604/gha-runner/provision.sh`. The overlay backs onto
  the byte-identical base delta (the base build keeps it in
  `<out>.build/delta.qcow2`; the runner build refetches the verbatim upstream
  `layer0` itself and `cp`+`rebase -u`s a working copy of the base delta onto it
  so the chain is readable for `virt-customize`), yielding a **3-root-layer**
  chain `layer0 + base-delta + runner-delta`. `images.yml`'s `build` job runs a
  second `build-image.sh` in the same run, uploads both layouts as one artifact,
  and (on `push_to_ghcr`) pushes `<name>` + `<name>-gha-runner`.
  - **Runner is fetched at deploy, not baked.** Pinning a runner tarball goes
    stale within weeks, so — mirroring the legacy Raspbian overlay — the image
    only installs `install-gh-actions-runner.service` (a first-boot oneshot) +
    `gh-actions-runner.service`; the shared
    `images/lib/install-gh-actions-runner.sh` downloads the *latest* release at
    deploy time (`RUNNER_ARCH=x64`). It resolves the version from the
    `releases/latest` redirect (no `jq`/`python3` in the guest) via `curl`
    (added to the Ubuntu package set).
  - **Validated locally** (`nix develop .#images`, KVM): both builds pass their
    internal `image-util parse` (2 and 3 root layers); the runner layout's lower
    two blobs are byte-identical to the base layout's two root layers (dedupe);
    a re-chained read of the runner delta confirms both units enabled in
    `multi-user.target.wants`, the install script present, and `tml`'s shell
    switched to the journal-login shell. The **boot + runner-registers** gate
    (§9) needs a `zot` TCP daemon, which is blocked in the build sandbox — still
    owed on a real runner / dispatch.
- **Phase 4 (Raspbian): TODO** — see §9.

---

## 1. Decisions (settled)

- **Tool:** libguestfs (`virt-customize`, `guestfish`); **mtools** for FAT edits.
- **Per-image, per-arch workflows.** Ubuntu family on an x86_64 hosted runner;
  Raspbian family on an aarch64 hosted runner. Native-arch → KVM when `/dev/kvm`
  is present, same-arch TCG fallback otherwise (acceptable; no cross-arch
  appliance, no target-kernel boot).
- **Root layering = upstream-as-lowest-layer.** The literal unmodified upstream
  image is the first `role=root` layer (the dedupe blob); our provisioning is a
  qcow2 overlay on top. The boot part (RPi only) is a single modified FAT blob —
  **no layering for boot**.
- **cloud-init purged** on Ubuntu (matches the RPi precedent). We own
  growpart / ssh-host-keys / networking with deterministic units.
- **`expandroot.sh` kept** — runtime root-grow per deployment (the image ships a
  small virtual disk; the host enlarges it arbitrarily at deploy time).
- **Non-hermetic apt is accepted.** Only the upstream base layer dedupes; the
  delta layers churn per rebuild by design.
- **No baked minimum image size.** Virtual size = upstream's; `expandroot` grows
  to the host disk at deploy.
- **puppet** is built by Nix in a separate CI step and passed to the build script
  as `--puppet <path>`. `image-util` likewise (`nix build .#image-util`).
- **OCI assembly + validation = one Rust binary** (`image-util`, the renamed
  `image-check`). `mk-treadmill-image.nix` and `media-types.nix` are **deleted**;
  their logic moves into `treadmill_rs::image::assemble`. Consumer
  (`parse.rs`, chain-assembly, qemu) already handles arbitrary chain depth — no
  change required there.

---

## 2. Final repo layout

```
images/
  lib/
    build-image.sh        # NEW: shared driver (fetch, extract, overlay, virt-customize, finalize, assemble, validate)
    provision-common.sh   # NEW: shared in-guest steps
    install-gh-actions-runner.sh  # NEW: shared first-boot runner installer (downloads latest; RUNNER_ARCH-parameterized)
    expandroot.sh         # KEPT as-is
  ubuntu-server-2604/         # Ubuntu Server 26.04 LTS (was the ubuntu-2204 base)
    manifest.sh           # NEW: sourced shell (data only)
    provision.sh          # NEW
    gha-runner/provision.sh   # NEW
  raspbian-13/
    manifest.sh           # NEW
    provision.sh          # NEW
    gha-runner/provision.sh   # NEW
  util/                   # RENAMED from images/check (was treadmill-image-check / image-check)
    Cargo.toml            #   crate treadmill-image-util, bin `image-util`
    src/main.rs           #   subcommands: parse, assemble

treadmill-rs/src/image/
  parse.rs                # UNCHANGED
  assemble.rs             # NEW: pure manifest/index builder, symmetric with parse
```

Deleted in cleanup (Phase 6): `images/lib/mk-treadmill-image.nix`,
`images/lib/media-types.nix`, `images/ubuntu-2204/{rootfs,default}.nix`,
`images/ubuntu-2204-gha-runner/`, `images/raspbian-13/{parts,default,gha-runner}.nix`,
`images/raspbian-13/install-gh-actions.sh`, and the `image-*`/`mkTreadmillImage`
wiring in `nix/images.nix`.

---

## 3. `image-util` (assembly + validation in Rust)

Rename the `treadmill-image-check` crate / `image-check` binary to
`treadmill-image-util` / `image-util`, with clap subcommands:

- `image-util parse <layout> [--root-layers N] [--boot-layers M] [--title T] [--name NAME]`
  — today's validator verbatim (reparse via `treadmill_rs::image::parse`, assert
  layer counts, chain wiring, head, and blob presence/size).
- `image-util assemble --name NAME --title TITLE [--version V] [--description D] \
     --layer <role>=<path> [--layer <role>=<path> …] -o <out-dir>`
  — produces the OCI layout. `--layer` is **order-significant** and repeatable;
  role is `root` or `boot`. Example (RPi): `--layer boot=boot.fat
  --layer root=upstream-root.qcow2 --layer root=root-delta.qcow2`.

`assemble` responsibilities (the file-I/O half, in the binary):
1. For each layer: sha256 the file, copy into `blobs/sha256/<hex>` (content
   addressed — identical blobs stored once), record size.
2. For `role=root` qcow2 layers: read the **virtual size from the qcow2 header**
   (8-byte big-endian field at offset 24) — no `qemu-img` runtime dependency.
3. Map role → media type from the Rust constants (`root` → disk-qcow2,
   `boot` → boot-FAT-v1).
4. Call `treadmill_rs::image::assemble::build_manifest(...)` to get the
   `ImageManifest` + index, then write `manifest.json` (CAS), `config` (empty),
   `index.json`, and the `oci-layout` marker.

New `treadmill_rs::image::assemble` (the pure half, in the model crate):
- Input: ordered `[{ role, media_type, digest, size, virtual_size? }]` + image
  metadata. Output: `oci_spec::image::ImageManifest` (+ the wrapping index).
- Derives chain wiring exactly as `parse` consumes it: the `role=root`
  subsequence in order — first gets no `lower`, each subsequent root's `lower` =
  previous root's digest, manifest `head` = last root's digest; `role=boot`
  layers are standalone (no `lower`, never `head`). Uses the same
  `annotations`/media-type constants as `parse`.
- Any media-type / artifact-type strings that today live only in
  `media-types.nix` are added here as Rust constants (single source of truth).
- Unit test: `assemble::build_manifest(...)` → `parse::parse_image(...)`
  roundtrips and preserves roles/chain/head. This replaces `nix/images.nix`'s
  `imageHelperSmoke` and is the structural reason the producer can no longer
  drift from the parser.

---

## 4. `manifest.sh` (data only, sourced — no TOML, no parser)

```sh
# images/ubuntu-server-2604/manifest.sh
arch="x86_64"
type="disk"                       # whole-disk qcow2 base
title="Ubuntu Server 26.04"
version="26.04"
# Pinned dated upstream release (server cloudimg is already a qcow2). No personal
# mirror — base + rustup-init come from upstream stable URLs, sha256-verified on
# every fetch.
base_image_url="https://cloud-images.ubuntu.com/releases/26.04/release-20260612/ubuntu-26.04-server-cloudimg-amd64.img"
base_image_sha256="0c9fb915bab0b36b361d3bf8aeae2115dda19d81a306656964de048033481670"
rustup_init_url="https://static.rust-lang.org/rustup/archive/1.29.0/x86_64-unknown-linux-gnu/rustup-init"
rustup_init_sha256="4acc9acc76d5079515b46346a485974457b5a79893cfb01112423c89aeb5aa10"
packages=(vim tmux htop build-essential git usbutils pciutils nload nano gnupg bc mtr zip unzip wget gpg ca-certificates dbus)
puppet_daemon_args='--transport auto_discover'
serial_consoles=(ttyS0)
```

```sh
# images/raspbian-13/manifest.sh
arch="aarch64"
type="sd"                         # FAT boot partition + ext4 root partition
title="Raspberry Pi OS 13 (NBD)"
version="13"
base_image_url="https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-04-21/2026-04-21-raspios-trixie-arm64-lite.img.xz"
base_image_sha256="..."
base_image_mirror="https://alpha.mirror.svc.schuermann.io/files/treadmill-tb/2026-04-21-raspios-trixie-arm64-lite.img.xz"
packages=()                       # nbd-client installed from a local .deb in provision.sh
puppet_daemon_args='--transport tcp --tcp-control-socket-addr "$(ip route show 0.0.0.0/0 | cut -d" " -f3 | head -n1):3859"'
serial_consoles=(ttyAMA0 ttyAMA10)
nbd_cmdline="console=serial0,115200 ip=dhcp root=/dev/nbd0 rw nbdroot=dhcp,root,nbd0 rootfstype=ext4 fsckfix rootwait net.ifnames=0 loglevel=7"
```

---

## 5. `build-image.sh`

`./images/lib/build-image.sh <name> --puppet <path> --image-util <path> \
   [--variant base|gha-runner] [--base-delta <path>] -o <out-dir>`

```
source images/<name>/manifest.sh

# 1. Fetch + verify base (upstream URL; sha256 is the drift guard)
curl -fL "$base_image_url" -o base.dl
echo "$base_image_sha256  base.dl" | sha256sum -c -

# 2. Produce the lowest root layer (verbatim upstream) and the boot blob:
if [ "$type" = disk ]; then          # Ubuntu cloudimg IS already qcow2
  layer0=base.dl                     # used verbatim -> the dedupe blob
elif [ "$type" = sd ]; then          # Raspbian SD image
  xz -d < base.dl > sd.img
  guestfish --ro -a sd.img run : download /dev/sda1 boot.fat       # raw FAT -> boot blob
  guestfish --ro -a sd.img run : download /dev/sda2 root.raw
  qemu-img convert -f raw -O qcow2 root.raw layer0.qcow2           # lowest root layer
  # boot-FAT edits (no overlay mechanism for boot) via mtools:
  printf '%s\n' "$nbd_cmdline" | mcopy -i boot.fat -o - ::cmdline.txt
  : | mcopy -i boot.fat -o - ::ssh.txt                             # enable sshd on first boot
fi

# 3. Customization delta = qcow2 overlay backed by the previous root layer
prev=${base_delta:-$layer0}          # gha-runner backs onto the base delta
qemu-img create -f qcow2 -b "$prev" -F qcow2 delta.qcow2
virt-customize -a delta.qcow2 \
  --copy-in "$puppet":/usr/local/bin \
  --copy-in rustup-init:/opt --copy-in images/lib/expandroot.sh:/opt \
  $( [ ${#packages[@]} -gt 0 ] && printf -- '--install %s ' "$(IFS=,; echo "${packages[*]}")" ) \
  --run images/lib/provision-common.sh \
  --run images/<name>/[gha-runner/]provision.sh
# provision scripts receive $puppet_daemon_args, $serial_consoles, etc. via env.

# 4. Finalize the delta: trim, detach backing, compress
virt-customize -a delta.qcow2 --run-command 'fstrim -av || true'
qemu-img rebase -u -b "" -F qcow2 delta.qcow2                      # standalone differential blob
qemu-img convert -c -O qcow2 delta.qcow2 delta.c.qcow2 && mv delta.c.qcow2 delta.qcow2

# 5. Assemble + validate the OCI layout
#   disk base:    --layer root=layer0      --layer root=delta
#   disk runner:  --layer root=layer0      --layer root=base-delta --layer root=runner-delta
#   sd base:      --layer boot=boot.fat    --layer root=layer0      --layer root=delta
"$image_util" assemble --name <name> --title "$title" --version "$version" \
  <ordered --layer flags...> -o "$out"
"$image_util" parse "$out" --root-layers <N> --boot-layers <0|1> --title "$title"
```

Notes:
- **Byte-identical base for overlays** is guaranteed structurally: the
  `gha-runner` variant runs in the *same workflow run / same script* and reuses
  the exact `base-delta.qcow2` file (passed via `--base-delta`) — never rebuilt.
- `--install` resolves from live archives (non-hermetic, accepted). RPi
  `nbd-client` is a local `.deb`: `--copy-in nbd-client.deb:/tmp` +
  `--run-command 'apt-get install -y /tmp/nbd-client*.deb'`.
- libguestfs runs provisioning in its appliance with the target root mounted —
  **the guest kernel is never booted**, so the entire RPi DTB-merge /
  `qemu-system-aarch64 -kernel` / `sysrq`/`poweroff -f` / 7z surgery /
  `/boot/firmware-mock` machinery is deleted outright.

---

## 6. `provision-common.sh` (shared in-guest steps)

Runs under `virt-customize` (root mounted, no boot). Ports the Treadmill-specific
bits from the legacy `rootfs.nix` / `parts.nix`:

- `useradd -m -u 1000 -s /bin/bash tml`; groups `plugdev`,`tty` (+ `dialout`,`gpio`
  on RPi); `010_tml-nopasswd` sudoers.
- USB udev rule `99-tml.rules` (`SUBSYSTEM=="usb", GROUP="plugdev", TAG+="uaccess"`).
- dbus policy `ci.treadmill.Puppet.conf`.
- `tml-puppet.service` (Type=notify, ssh-dir `ExecStartPre`s, `Restart=always`),
  `ExecStart` built from `$puppet_daemon_args`; enabled via `multi-user.target.wants`.
- rustup: `sudo -u tml /opt/rustup-init -y --default-toolchain none --profile minimal`.
- `expandroot`: install `/opt/expandroot`, `firstboot-expandroot.service` + sentinel
  (runs once per fresh deployment; grows root to the host-enlarged disk).
- **ssh host keys:** `rm -f /etc/ssh/ssh_host_*_key*`; install
  `ssh-generate-host-keys.service` (`Before=ssh.service`,
  `ConditionPathExistsGlob=!/etc/ssh/ssh_host_*_key`, `ExecStart=/usr/bin/ssh-keygen -A`)
  → unique keys per deployment. (The old `policy-rc.d` dance is gone: openssh is
  preinstalled in the base, so there is no install-time start race.)
- serial autologin: for each `$serial_consoles` device, drop
  `serial-getty@<dev>.service.d/override.conf` with `--autologin tml`.
- **Networking** (identical on both; RPi NIC is `eth0` via `net.ifnames=0`):

  ```ini
  # /etc/systemd/network/10-eth.network
  [Match]
  Name=en* eth*
  [Network]
  DHCP=yes
  IPv6AcceptRA=yes
  IPv6PrivacyExtensions=no
  [Link]
  RequiredForOnline=true
  ```

  `systemctl enable systemd-networkd systemd-resolved`.

---

## 7. Per-image `provision.sh`

**Ubuntu (`ubuntu-server-2604/provision.sh`):**
- `apt-get purge -y cloud-init`; `rm -f /etc/netplan/*.yaml` (networkd drives the net).
- grub serial console: `GRUB_CMDLINE_LINUX="console=ttyS0"`, `GRUB_TERMINAL="serial"`,
  drop `quiet`; `update-grub`. Partitioning/UEFI/`grub-install`/initramfs come from
  the cloud image and are **not** reapplied.
- Deleted vs `rootfs.nix`: the `sgdisk`/`mkfs` partition dance, `grub-install`,
  manual `update-initramfs`, the `policy-rc.d` SSH block, the firstboot growpart
  (now `expandroot`), the hand-rolled `10-eth.network` (now shared).

**Raspbian (`raspbian-13/provision.sh`)** — root delta only; boot FAT was edited
in `build-image.sh`:
- `userdel -r pi`.
- nbd fstab: `/dev/nbd0 / ext4 defaults,noatime,nodiratime 0 1` + `proc`.
- mask SD/firmware-only units: `dphys-swapfile`, `rpi-eeprom-update`, `userconfig`,
  `systemd-hostnamed(.socket)`, `systemd-logind`, `cloud-init*`/`cloud-final`/
  `cloud-init.target`, `rpi-resize`, `systemd-growfs-root`, `rpi-resize-swap-file`,
  `sshswitch`.
- install `nbd-client` from the copied-in `.deb`.
- Deleted vs `parts.nix`: DTB merge, the `qemu-system-aarch64` customize-boot,
  `sysrq`/`poweroff -f`, 7z surgery, the `/boot/firmware-mock` + `FWLOC` hack
  (never booting; at runtime `/boot/firmware` is the real TFTP FAT).

**gha-runner overlays (`*/gha-runner/provision.sh`):** applied as a delta backed
by the base delta. `build-image.sh` `--copy-in`s the shared
`images/lib/install-gh-actions-runner.sh`; the per-image provision installs two
units — `install-gh-actions-runner.service` (a first-boot oneshot that runs the
shared script to download the *latest* runner release for `RUNNER_ARCH`, so no
version is baked into / pinned in the image) and `gh-actions-runner.service`
(configures + runs it as `tml`) — and switches `tml`'s shell to the
journal-login shell. The runner tarball is **not** fetched at build time.

---

## 8. CI

A single `images.yml` (`workflow_dispatch`, non-cancelling `concurrency`), with
per-family dispatch checkboxes and a matrix that runs each family on a
matched-arch hosted runner (Ubuntu → `ubuntu-latest`, Raspberry Pi OS →
`ubuntu-24.04-arm`). A `prepare` job resolves the checkboxes into the build
matrix (the single place mapping family → runner + static-puppet package); the
`build` job, per matrix entry:

1. `install-nix-action` + read-only `cachix-action`.
2. Build the binaries: `nix build --no-link --print-out-paths
   .#tml-puppet-static-<arch>` and `.#image-util` (capture the store paths — an
   `-o puppet` out-link would collide with the in-tree `puppet/` crate dir).
3. Run `build-image.sh` **inside `nix develop .#images`** (assembles + validates
   internally), then `upload-artifact` the `out/` layout. The libguestfs
   toolchain comes from the devshell's `libguestfs-with-appliance` — a *prebuilt*
   appliance, so no runtime `supermin` build (no host-kernel readability hack)
   and the same network backend that works in local dev. The distro (apt)
   libguestfs was tried first but fought the hosted runner (supermin needs the
   0600 host kernel). Either way libguestfs auto-prefers `passt` for `--network`
   when a `passt` binary is on PATH, and passt exits 1 on the hosted runner; the
   step `rm`s the runner's `/usr/bin/passt` so libguestfs falls back to qemu
   slirp user-net (what local dev uses — there is no libguestfs setting to force
   it, only the passt-runnable autodetect).
4. On `push_to_ghcr`: `skopeo copy oci:out` to `ghcr.io/<owner>/<name>:{<sha>,latest}`.

(A temporary `cachix push` of the two binaries sits between 2 and 3 so reruns
substitute them instead of recompiling.)

The `gha-runner` overlay variant (a second `build-image.sh` invocation backed by
the base delta) folds in with Phase 3; the Raspberry Pi `ubuntu-24.04-arm` row is
a placeholder until the Phase 4 `sd` path lands.

---

## 9. Phased rollout (each phase has an explicit gate)

- **Phase 0 — spike.** On both runner types: `virt-customize --install` +
  `--run-command` succeed; record `/dev/kvm` presence on the **arm** runner
  (KVM vs TCG) + wall-clock; `guestfish download /dev/sda1|2` yields a mountable
  raw FAT and a convertible ext4; `mcopy -i boot.fat` edits `cmdline.txt`.
  **Gate:** all four succeed on both arches.
- **Phase 1 — `image-util`.** Rename `image-check` → `image-util`; add
  `treadmill_rs::image::assemble` + the `assemble` subcommand; keep `parse`.
  Delete `mk-treadmill-image.nix` / `media-types.nix` and their `nix/images.nix`
  wiring; rewire the hermetic checks to `image-util`. **Gate:** clippy/nextest
  green; the `assemble → parse` roundtrip unit test passes; `nix flake check`
  green.
- **Phase 2 — Ubuntu base.** `build-image.sh` (disk path),
  `provision-common.sh`, `ubuntu-2204/{manifest,provision}.sh`. **Gate
  (behavioral, not package-diff):** `image-util parse` OK; `nix run
  .#qemu-supervisor-local --image <layout>` boots, `tml` autologins on ttyS0,
  `tml-puppet` connects over D-Bus, USB `uaccess` works, and root expands to a
  deliberately enlarged overlay disk. Run beside the legacy pipeline.
- **Phase 3 — Ubuntu runner overlay.** `ubuntu-2204/gha-runner/provision.sh`;
  assert the 3-root-layer chain + runner registration. **Gate:** parse + boot +
  runner registers.
- **Phase 4 — Raspbian.** `build-image.sh` sd path +
  `raspbian-13/{manifest,provision}.sh` + `gha-runner`. **Gate:** parse; NBD boot
  with `tml-puppet` over tcp on a real/emulated target; boot FAT serves the
  correct `cmdline.txt`.
- **Phase 5 — CI cutover.** Land the two new workflows alongside the legacy
  `images.yml`; burn-in building both, comparing only the behavioral gates
  (package sets legitimately differ between debootstrap and cloud-image bases).
- **Phase 6 — cleanup.** Delete the legacy `vmTools`/`parts.nix`/`default.nix`
  files and `nix/images.nix` wiring (§2); retire the stale image docs; this file
  becomes the canonical image-build reference.

---

## 10. Residual risks (accepted, with mitigation)

- **Non-reproducible deltas** (live apt) — accepted; the big upstream layer still
  dedupes; pin a `snapshot.ubuntu.com` URL later if it ever bites.
- **arm KVM uncertain** — Phase 0 resolves; same-arch TCG fallback accepted.
- **cloud-init purge side-effects on Ubuntu networking** — mitigated by owning
  `10-eth.network` + removing `/etc/netplan/*.yaml`; verified by the Phase 2 boot
  gate.
