# Treadmill Images → OCI + Monorepo Migration Plan

**Status:** proposed roadmap · **Date:** 2026-06-14

Companion to `doc/oci-image-migration-plan.md` (the format/runtime side). That
doc replaces the bespoke TOML store with OCI in the **runtime** (switchboard,
supervisor, `treadmill-rs/src/image/`). *This* doc covers the **producer**: the
legacy `images/` repository that still emits the TOML store format, its cleanup,
and folding it into the monorepo so it can build OCI images in CI.

The reference producer already exists in-tree: `nix/tiny-efi.nix` hand-assembles
a valid Treadmill OCI layout. The production images become its siblings.

---

## 1. Decisions (settled with the maintainer, 2026-06-14)

| # | Question | Decision |
|---|----------|----------|
| I1 | Where does the image-building code live? | **Monorepo.** Move into `code/` (this repo). Eliminates puppet/protocol version skew, lets us generate/validate manifest strings against the in-tree parser, and reuses fenix + the `tml-puppet-static-*` packages + cachix. Heavy builds are kept out of PR CI by a **separate workflow**, not a separate repo. |
| I2 | Logical image naming | Distro+version only: **`ubuntu-2204`**, **`raspbian-13`** (Raspberry Pi OS = Debian 13 "trixie"). Arch / target / transport (`amd64`, `arm64`, `qemu`, `nbd-netboot`, `uefi`) move into the OCI `platform` field and `ci.treadmill.required-host-tags`, **not** the name. |
| I3 | Runner overlays | **Keep**, as proper 2-layer OCI backing chains (base root layer + runner root layer, `ci.treadmill.qcow2.lower` set, **no baked backing**). |
| I4 | Group taxonomy | **4 separate groups** — `ubuntu-2204`, `ubuntu-2204-gha-runner`, `raspbian-13`, `raspbian-13-gha-runner`. "with runner" is an explicit user choice, not a host-derived selection, so it is its own logical image/group. |
| I5 | Group support scope | **N-member index machinery, table-driven, populated with the members that exist today** (1 each). Adding e.g. `ubuntu-arm64-qemu` later is a one-line addition to `images/groups.nix`. No new arch/board variants are authored in this pass. |
| I6 | Group representation | A group is an **OCI image index** (`artifactType = application/vnd.treadmill.image-group.v1+json`) whose members reference the concrete image manifests **by digest** and carry `platform` + `ci.treadmill.required-host-tags`. `required-host-tags` is a **scheduling policy**, declared in `images/groups.nix`, **never** baked into a per-image manifest or the root filesystem. |
| I7 | CI scope | **Build the OCI artifact first** (and validate it). **ghcr.io push is a stretch goal** in the same workflow. **Switchboard registration is deferred** (not implemented now) but the data it needs lives in `images/groups.nix`, and the workflow leaves a documented hook. |
| I8 | Puppet source | Consume the in-tree **`tml-puppet-static-x86_64` / `tml-puppet-static-aarch64`** packages (`nix/puppet-cross-musl.nix`). **Delete** `images/lib/puppet.nix` and its `builtins.fetchGit` pins of treadmill + fenix. |
| I9 | Drift guard | A flake check parses every built image/group layout with the **real `treadmill-rs` parser** (`parse_image`/`parse_group`). This is the backstop against media-type/annotation drift; manifest strings live once in `images/lib/media-types.nix`. |
| I10 | Netboot boot partition | **Raw FAT blob** (`application/vnd.treadmill.boot.fat.v1`, `role=boot`), not a tar archive. The build already produces the FAT (`0.fat`) before tarring it — we stop tarring. |

---

## 2. Target layout in the monorepo

```
code/
  images/
    lib/
      media-types.nix       # Nix mirror of treadmill-rs media_types.rs / annotations.rs
      mk-treadmill-image.nix # ordered layers -> validated OCI image layout
      mk-group-index.nix     # members -> OCI image-index (group) layout
      expandroot.sh          # moved verbatim from images/lib/expandroot.sh
    groups.nix               # declarative group membership + required-host-tags table
    ubuntu-2204/
      default.nix            # base amd64 qemu/uefi image -> OCI layout
      gha-runner.nix         # +runner overlay (2-layer root chain)
    raspbian-13/
      default.nix            # base arm64 nbd-netboot image -> OCI layout (boot FAT + root qcow2)
      gha-runner.nix         # +runner overlay (boot FAT + 2-layer root chain)
      run-qemu-vm.nix        # local dev boot harness (reads OCI layout)
  nix/
    images.nix              # flake-parts module: packages.* image-/group-, apps.* run-*
    tiny-efi.nix            # existing reference fixture (unchanged)
  .github/workflows/
    images.yml             # NEW: build + parse-check + artifact (+ stretch ghcr push)
```

`nix/images.nix` is added to `flake.nix`'s `imports`. It is **Linux-only**
(`lib.optionalAttrs isLinux …`), exactly like `nix/tiny-efi.nix`, because the
builds need `runInLinuxVM`, `qemu-system-*`, `dosfstools`/`mtools`, etc.

The legacy standalone `images/` repository is archived once this lands; its
content is not preserved verbatim (no backwards compatibility required).

---

## 3. Shared Nix library

### 3.1 `images/lib/media-types.nix`

A single attrset mirroring the Rust constants, referenced everywhere a media
type / annotation key / artifactType string is needed:

```nix
{
  imageArtifactType = "application/vnd.treadmill.image.v1+json";
  groupArtifactType = "application/vnd.treadmill.image-group.v1+json";
  diskQcow2         = "application/vnd.treadmill.disk.qcow2";
  bootFatV1         = "application/vnd.treadmill.boot.fat.v1";
  ociEmptyConfig    = "application/vnd.oci.empty.v1+json";
  ociManifest       = "application/vnd.oci.image.manifest.v1+json";
  ociIndex          = "application/vnd.oci.image.index.v1+json";
  ann = {
    role            = "ci.treadmill.role";
    head            = "ci.treadmill.qcow2.head";
    lower           = "ci.treadmill.qcow2.lower";
    virtualSize     = "ci.treadmill.qcow2.virtual-size";
    requiredHostTags= "ci.treadmill.required-host-tags";
    title           = "org.opencontainers.image.title";
    version         = "org.opencontainers.image.version";
    description     = "org.opencontainers.image.description";
    baseName        = "org.opencontainers.image.base.name";
  };
}
```

These strings are the **only** copy of the literals; correctness against the
parser is enforced by the §6 parse-check, not by hoping they match.

### 3.2 `images/lib/mk-treadmill-image.nix`

```
mkTreadmillImage {
  name;                 # store-path label, e.g. "ubuntu-2204"
  title;                # org.opencontainers.image.title
  version ? null;
  description ? null;
  layers = [            # ORDERED; chain semantics derived from order + role
    { path;             # store path to the blob file
      mediaType;        # diskQcow2 | bootFatV1
      role;             # "root" | "boot"
      virtualSize ? null; }  # qemu-img virtual size for qcow2 root layers
  ];
}
-> derivation producing a standard OCI image layout:
     $out/oci-layout
     $out/index.json            (plain OCI index -> the one image manifest)
     $out/blobs/sha256/<digest> (config, every layer, the manifest)
```

Behaviour (matches `tiny-efi.nix`'s hand-assembly and what `parse.rs` expects):

1. Hash each `layers[*].path` into `blobs/sha256/<hex>`; record size; for qcow2
   layers record `virtualSize`.
2. Write the empty config blob `{}` (`ociEmptyConfig`, with `"data":"e30="`).
3. **Chain wiring (role-aware):** consider the sub-sequence of `role=="root"`
   layers in order. The first root layer gets **no** `lower`; each subsequent
   root layer gets `ci.treadmill.qcow2.lower = <digest of previous root layer>`.
   The manifest-level `ci.treadmill.qcow2.head = <digest of last root layer>`.
   `role=="boot"` layers are standalone (no `lower`, never `head`).
4. Emit the pure-artifact manifest (`artifactType = imageArtifactType`, empty
   config, the layers with their per-layer annotations) and a top-level
   `index.json` wrapping it (single member), as `tiny-efi.nix` does.

This single helper produces every image: 1-root-layer (ubuntu base),
2-root-layer chains (runner overlays), and boot+root (raspbian).

### 3.3 `images/lib/mk-group-index.nix`

```
mkGroupIndex {
  name;                 # e.g. "ubuntu-2204"
  members = [
    { image;            # an mkTreadmillImage layout
      platform;         # { architecture; os; variant ? null; }
      requiredHostTags; # [ "arch=amd64" ... ]  -> ci.treadmill.required-host-tags
    }
  ];
}
-> derivation producing a self-contained OCI layout where:
     index.json is an OCI image index, artifactType = groupArtifactType,
       each manifest descriptor = { the member's manifest digest+size,
         platform, annotations.<requiredHostTags> = csv(requiredHostTags) }
     blobs/sha256/ contains every member's manifest + config + all layer blobs
```

The group layout is self-contained so a single `skopeo copy oci:<layout>
docker://…` pushes the index **and** all referenced member content. Member
manifest digests are read from each member layout's `index.json`.

---

## 4. The four images (exact build recipes)

All three "build a disk" recipes are ports of the current `images/` files with
three mechanical changes: (a) puppet binary comes from the in-tree static
package, (b) output goes through `mkTreadmillImage` instead of the TOML store
shell, (c) overlays strip baked backing.

### 4.1 `ubuntu-2204/default.nix` (amd64, qemu/uefi) — 1 root layer
Port `images/vm-ubuntu-2204-amd64-uefi/default.nix` verbatim through
`vmTools.makeImageFromDebDist` (same package list, `createRootFS`,
`postInstall`: grub-efi, networking, `tml` user, ssh, dbus, the
`tml-puppet.service` unit, rustup), with:
- `cp ${treadmill-puppet-static-x86_64}/bin/tml-puppet …` (drop the bespoke
  fenix musl build).
- Keep the `qemu-img convert -c` compression of the final qcow2.
- Wrap: `mkTreadmillImage { name="ubuntu-2204"; title="Ubuntu 22.04";
  layers=[{ path=base.qcow2; mediaType=diskQcow2; role="root";
  virtualSize=<qemu-img>; }]; }`.

### 4.2 `ubuntu-2204/gha-runner.nix` — 2 root layers
Port `vm-ubuntu-2204-amd64-uefi/gh-actions-overlay.nix`:
- `runInLinuxVM` over a `qemu-img create -b base.qcow2` overlay installs the
  GitHub Actions runner + `gh-actions-runner.service` (unchanged unit text).
- Produce the **differential** overlay blob:
  `qemu-img convert -O qcow2 -B base.qcow2 -F qcow2 overlay-full.qcow2 diff.qcow2`
  then **`qemu-img rebase -u -b "" -f qcow2 diff.qcow2`** to strip the baked
  backing path. *(Replaces the legacy `rebase -u -b <relative-path>`, which is
  exactly the baked-backing the new format forbids — D3.)*
- Wrap: `mkTreadmillImage { name="ubuntu-2204-gha-runner";
  title="Ubuntu 22.04 + GitHub Actions runner";
  layers=[ base(root), diff(root) ]; }` → `lower`/`head` auto-wired.

### 4.3 `raspbian-13/default.nix` (arm64, nbd-netboot) — boot FAT + root qcow2
Port `netboot-raspberrypi-nbd/default.nix`:
- Keep `customizedSDImage`: download RPi OS, customize in TCG
  `qemu-system-aarch64`, patch `cmdline.txt` for NBD root, install
  `${treadmill-puppet-static-aarch64}/bin/tml-puppet`, nbd-client, the
  `tml-puppet.service` (tcp control-socket transport via `ip route`), expandroot.
- **Boot blob:** extract `0.fat` and store it **directly** as the boot blob
  (delete the `bootPartArchive` tar step). `mediaType=bootFatV1`, `role="boot"`.
- **Root blob:** extract `1.img`, `e2fsck`, `qemu-img convert -f raw -O qcow2`
  → `root.qcow2`. `mediaType=diskQcow2`, `role="root"`, `virtualSize=<qemu-img>`.
- Wrap: `mkTreadmillImage { name="raspbian-13";
  title="Raspberry Pi OS 13 (NBD)";
  layers=[ { boot.fat; bootFatV1; role="boot"; },
           { root.qcow2; diskQcow2; role="root"; virtualSize; } ]; }`
  → `head` = root digest; boot layer standalone.

### 4.4 `raspbian-13/gha-runner.nix` — boot FAT + 2 root layers
Port `netboot-raspberrypi-nbd/gh-actions-runner-overlay.nix`: same boot FAT
blob, runner overlay on the **root** via `runInLinuxVM` + differential
`convert -B` + `rebase -u -b ""`. Layers `[ boot(boot), base-root(root),
runner(root) ]`, head = runner digest.

> **Open verify (flag for the Phase 6.6 supervisor author):** whether the
> `role=boot` FAT layer needs a `virtual-size` annotation for the per-job
> qcow2-over-FAT overlay (D17/§6.4). It is a raw FAT, so its size is the
> descriptor `size`; we emit no `qcow2.virtual-size` on it until the consumer
> requires one.

---

## 5. Groups (`images/groups.nix` + `nix/images.nix`)

```nix
# images/groups.nix
{ images }:  # the built mkTreadmillImage layouts
{
  ubuntu-2204 = [
    { image = images.ubuntu-2204; platform = { architecture="amd64"; os="linux"; };
      requiredHostTags = [ "arch=amd64" ]; }
  ];
  ubuntu-2204-gha-runner = [
    { image = images.ubuntu-2204-gha-runner; platform = { architecture="amd64"; os="linux"; };
      requiredHostTags = [ "arch=amd64" ]; }
  ];
  raspbian-13 = [
    { image = images.raspbian-13; platform = { architecture="arm64"; os="linux"; variant="v8"; };
      requiredHostTags = [ "arch=arm64" "raspberrypi" ]; }
  ];
  raspbian-13-gha-runner = [
    { image = images.raspbian-13-gha-runner; platform = { architecture="arm64"; os="linux"; variant="v8"; };
      requiredHostTags = [ "arch=arm64" "raspberrypi" ]; }
  ];
}
```

`nix/images.nix` maps each entry through `mkGroupIndex` → `packages.<sys>.group-<name>`.
One member each today; the table is the single extension point for future
members.

---

## 6. Verification

### 6.1 Drift-guard parse check (`nix/checks.nix`)
Add `checks.<sys>.images-parse` (Linux-only, **not** in the PR fast-check set —
built only by the images workflow): for each `image-*` and `group-*` layout,
read `index.json` → manifest/index blob and run the real `treadmill-rs`
`parse_image` / `parse_group`, asserting:
- ubuntu-2204: 1 root layer, `head` = that layer, no `lower`.
- *-gha-runner: 2 root layers, `lower`/`head` wired, no baked backing
  (`qemu-img info` reports an empty backing file on the overlay blob).
- raspbian-13: a `role=boot` layer + a `role=root` layer, `head` = root.
- groups: member digests resolve, `required_host_tags` parse to the table values.

Implemented as a `treadmill-rs` integration test taking layout paths via env
vars (same shape as the existing `tiny-efi-image` check).

### 6.2 Local boot harnesses (flake apps)
Rewrite the `run-qemu-vm` helpers as `apps.<sys>.run-ubuntu-2204` /
`run-raspbian-13` that read the **OCI layout** (jq over `index.json` → manifest
→ `role=root` head blob, `role=boot` blob) instead of the TOML store via dasel,
build the backing chain locally (lowers read-only + a fresh per-run overlay,
mirroring `image::blockdev::BackingChain`), and boot qemu. The ubuntu harness
keeps the OVMF + `fw_cfg opt/…tcp-ctrl-socket` plumbing; the raspbian harness
keeps the `raspi3b` + kernel/dtb-from-FAT plumbing for dev testing.

---

## 7. CI (`code/.github/workflows/images.yml`)

Separate workflow so heavy builds never gate ordinary PRs (the main `ci.yml` is
untouched).

- **Triggers:** `pull_request`/`push` filtered to `images/**`, `nix/images.nix`,
  `images/lib/**`, `nix/puppet-cross-musl.nix`; plus `workflow_dispatch` and a
  `schedule` (weekly, to catch upstream deb/RPi-image drift).
- **Runner:** `ubuntu-latest` (provides `/dev/kvm` for `runInLinuxVM`). The
  raspbian build is full TCG (no KVM, slow) — cachix is essential; revisit a
  larger/self-hosted runner if wall-clock is unacceptable.
- **Steps:** `install-nix` + `cachix-action` (treadmill-tb, push only on main) →
  `nix build .#packages.<sys>.{image-*,group-*}` → `nix build
  .#checks.<sys>.images-parse` → `actions/upload-artifact` of `./result*`.
- **Stretch (same workflow, main/tag only):** `skopeo login ghcr.io` with
  `GITHUB_TOKEN`, then per image/group `skopeo copy oci:<layout>
  docker://ghcr.io/treadmill-tb/<name>:<git-sha>` (+ `:latest`). Push group
  indexes after their members so descriptors resolve.
- **Deferred hook:** a commented, no-op `register-with-switchboard` step
  documenting that it will `POST /images` + `/image-groups` (D11 token flow,
  §8 of the format plan) once switchboard's catalog exists; the digests + tags
  it needs are already emitted by the build and declared in `groups.nix`.

---

## 8. Phasing (each phase builds + verifies on its own)

- **A — Scaffolding & lib.** Add `images/lib/{media-types,mk-treadmill-image,
  mk-group-index}.nix` + `expandroot.sh`; add `nix/images.nix` (empty) to
  `flake.nix`; wire puppet packages. *Verify:* run an existing fixture's blobs
  (e.g. tiny-efi's qcow2s) through `mkTreadmillImage` and parse the result with
  `treadmill-rs` — proves the helper before any heavy build.
- **B — ubuntu-2204 base.** *Verify:* `nix build .#packages.<sys>.image-ubuntu-2204`;
  `images-parse`; boots via `apps.<sys>.run-ubuntu-2204`.
- **C — ubuntu-2204-gha-runner.** *Verify:* parse asserts 2 root layers +
  empty baked backing; boots; runner unit present.
- **D — raspbian-13 base.** *Verify:* parse asserts boot+root roles, head=root;
  local netboot QEMU harness reaches the `tml` login.
- **E — raspbian-13-gha-runner.** *Verify:* parse (boot + 2 root); boots.
- **F — Groups.** `groups.nix` + `mkGroupIndex` → 4 `group-*` outputs. *Verify:*
  `parse_group` over each (members, platform, required-host-tags).
- **G — CI build+verify+artifact.** Land `images.yml` (build + parse-check +
  upload). *Verify:* green on a PR touching `images/**`.
- **H — ghcr push (stretch).** Add the `skopeo copy` step on main/tag.
- **I — Switchboard registration (deferred).** Out of scope now; the workflow
  hook + `groups.nix` data are the seam.

Cleanup is intrinsic: nothing in the new tree contains the TOML store, dasel,
`builtins.fetchGit` of treadmill/fenix, the boot tar, or baked backing paths.

---

## 9. Risks / items to watch

- **`vmTools.debDistros.ubuntu2204x86_64`** must still exist in the flake's
  pinned `nixos-unstable`; `debDistros` occasionally breaks upstream. Verify in
  Phase B; if gone, pin a known-good nixpkgs for the ubuntu build only.
- **raspbian TCG build time.** Cross-arch emulation is slow; relies on cachix to
  avoid rebuilds. Larger/self-hosted runner is the escape hatch.
- **Boot-FAT virtual-size** annotation requirement is unresolved until the
  Phase 6.6 supervisor lands (§4.3 note).
- **KVM availability** on GitHub-hosted runners for `runInLinuxVM` (ubuntu base
  + both runner overlays). Currently present on `ubuntu-latest`; if a runner
  lacks it the deb/runInLinuxVM builds fall back to slow TCG.
- **ghcr namespace/permissions** (`packages: write`, `treadmill-tb` org policy)
  needed before the stretch push works.
