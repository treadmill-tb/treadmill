# Treadmill Image Format → OCI Migration — Implementation Plan

**Status:** proposed roadmap · **Date:** 2026-06-06

> This document is a **one-time implementation roadmap**, not the image-format
> specification. Following the convention established by
> `doc/switchboard-protocol-refactor-plan.md`, the specification itself lives
> **in the code** to avoid drift:
>
> - **Semantics** (media types, annotation keys, layer/backing-chain rules,
>   selection rules) live as **Rustdoc** on the image types in
>   `treadmill-rs/src/image/` and as named constants.
> - **The wire contract** for any switchboard↔supervisor and switchboard↔client
>   types is pinned by **committed JSON Schema snapshots** generated via
>   `schemars`; a test fails CI on drift (same mechanism as the protocol types).
>
> Once the phases below are complete, this file may be deleted or archived.

---

## 1. Goals

1. Replace the home-grown TOML manifest + content store (`treadmill-rs/src/image/`)
   with **OCI image manifests, an OCI image index, and the OCI Distribution
   protocol**, so we stop maintaining a bespoke format, store, and (unbuilt)
   `source` extension.
2. **Switchboard holds no image bytes.** It becomes a *catalog* (pinned digests
   + registry refs), a *registry token issuer*, and an *attribute matcher*.
3. **Image groups**: one logical image (e.g. "Ubuntu 22.04") resolves to the
   right concrete image for the host it lands on (QEMU x86-64 / QEMU aarch64 /
   RPi aarch64 NBD), via the standard OCI **image index** with Treadmill
   selection annotations.
4. **Federated hosting**: users may host images in their own registry;
   switchboard pins only the manifest digest + ref and pays no storage/bandwidth.
   A canonical hosted registry exists for images that must be guaranteed
   schedulable and for user pushes.
5. **Scoped push tokens**: switchboard selectively mints short-lived,
   repository-scoped registry tokens so users can push new images (from scratch
   or by adding a layer to an existing image) and register the result.
6. **Single-copy local store** on each server: supervisors pull into a shared
   OCI image layout on local disk and point `qemu` / `qemu-nbd` at the blob
   files directly (one inode, shared page cache). See §6.
7. **Image leases**: a running supervisor leases the images it uses so the
   shared store's garbage collector never reclaims an in-use blob. See §7.

## 2. Source of truth

| Concern | Source of truth | Drift guard |
|---|---|---|
| Treadmill media types / `artifactType` | constants in `treadmill-rs/src/image/` | compile-time + review |
| Annotation keys (`ci.treadmill.*`) | constants + Rustdoc | review |
| Manifest / index shapes consumed | OCI image-spec 1.1 (vendored types or `oci-spec` crate) | upstream spec |
| Job ↔ image wire types | Rust types in `treadmill-rs/src/api/` | committed `*.schema.json` snapshot + diff test |
| Backing-chain construction rules | Rustdoc on supervisor image setup | review |
| Catalog / group model | switchboard DB migrations + Rustdoc | sqlx compile-time checks |

---

## 3. Target architecture (recap)

```
            ┌──────────────── switchboard ────────────────┐
            │ catalog DB (digest, ref, group, owner, attrs)│
 job ──────▶│ token/authz server (scoped push/pull)        │
            │ group→manifest matcher (host attributes)     │
            └───────┬───────────────────────┬──────────────┘
            pin {ref,digest}          mints tokens / verifies
       ┌────────────▼─────────┐      ┌───────▼────────────────┐
       │ external registries  │      │ canonical OCI registry │  (we host: Zot)
       │ (user-hosted, byo)   │      │ user pushes + mirrors  │
       └──────────┬───────────┘      └───────────┬────────────┘
                  └────────── pull by digest ────┘
                                   │
          per-server Zot daemon (single writer, store rw in own ns)
            ref-leases ─┤ daemon-owned GC ├─ ro bind-mount of blob files
                                   │
                       supervisors → qemu / qemu-nbd (read-only)
```

The OCI ↔ Treadmill mapping (manifest, index, annotations, cross-repo mount,
token flow) was settled in design discussion and is summarized in §4–§5; this
document focuses on *how we get there*.

---

## 4. Key decisions (the open questions, answered)

Each row is a decision we are committing to unless flagged **(revisit)**.

| # | Question | Decision |
|---|----------|----------|
| D1 | Manifest format | OCI image manifest (`application/vnd.oci.image.manifest.v1+json`), JSON. Pure-artifact form: empty config (`application/vnd.oci.empty.v1+json`), `artifactType = application/vnd.treadmill.image.v1+json`. |
| D2 | Per-blob role/type | Custom media types: `application/vnd.treadmill.disk.qcow2`, `application/vnd.treadmill.boot.fat.v1`. Role echoed in `ci.treadmill.role` annotation (`root` / `boot`). |
| D3 | Backing chain (`head`/`lower`/`virtual-size`) | Expressed as **OCI layer order** + per-layer annotations `ci.treadmill.qcow2.virtual-size` and `ci.treadmill.qcow2.lower` (digest of the lower layer). The manifest-level `ci.treadmill.qcow2.head` annotation names the top layer's digest. **Backing paths are never baked into the shared qcow2 blobs**; the chain is supplied at runtime (D9). |
| D4 | Image groups | OCI **image index** (`application/vnd.oci.image.index.v1+json`), `artifactType = application/vnd.treadmill.image-group.v1+json`. Each member descriptor carries `platform` (arch/os/variant) plus `ci.treadmill.target` (`qemu`/`nbd-netboot`) and optional `ci.treadmill.board`. |
| D5 | Image identity in API/DB | An image is identified by its **manifest digest** plus a resolvable **registry ref**. New type `treadmill_rs::image::ImageRef { registry, repository, digest }`. `JobInitSpec` references either a concrete `ImageRef` or a registered **catalog id** (group or image) resolved by switchboard. Pinning is always by digest; tags are aliases resolved at registration time. |
| D6 | Registry library | `oci-client` crate (pure-Rust OCI Distribution client) on the supervisor; `oci-spec` crate (or vendored minimal types) for manifest/index structs. No shelling to `skopeo`/`oras` in the hot path. CLI may shell to `oras` for ergonomics. |
| D7 | Local store shape | A **per-server Zot daemon** owns the store (single writer); its storage is a standard OCI image layout (`blobs/sha256/<digest>`, `oci-layout`). Consumers never write it — they `open()` blobs **read-only** via a ro bind-mount. All ingest/dedup/locking/GC are the daemon's serialized internal job; supervisors only *read*. See §7. |
| D8 | Daemon on management path, not data path | The daemon fetches each blob **once** (pull-through cache of the canonical registry); consumers read those same files directly → still **single-copy + shared page cache**. This is the §6 single-copy requirement *and* a managed concurrency model — the earlier "registry = second copy" objection applied only to HTTP-served data, which we don't do. **(revisit)** thin custom daemon over the same OCI layout if the Zot direct-read coupling is unacceptable (§7.4 fallback). |
| D9 | Runtime backing chain | `qemu-system-*`: `-blockdev` node graph. `qemu-nbd`: `--image-opts` with nested `backing.*` (verified: `qemu-nbd` has `--image-opts`, no `-blockdev`). Shared lower layers pinned `read-only=on`; only the per-job overlay (created with no baked backing) is writable. |
| D10 | Registry (both tiers) | **Zot** at two tiers: a **canonical** Zot (central, source of truth, user pushes, switchboard catalog) and a **per-server** Zot (single-writer local store + pull-through cache + read-only file source for consumers, §7). Same software both places. **(revisit)** Harbor for the canonical tier if we later want built-in project RBAC. |
| D11 | Auth | Switchboard becomes the registry's **bearer-token issuer** (Docker v2 token flow), reusing the existing `auth`/`token` subsystem. Zot is configured **non-anonymous for both pull and push** (we don't pay bandwidth for unauthorized access). Switchboard mints short-lived, single-repository tokens: `repository:<repo>:pull` is included in the **job dispatch message** so the supervisor's `oci-client` can pull; `repository:<repo>:push` is handed to a user out-of-band for publishing. Scope is per-repo + short TTL, so a compromised supervisor only gains pull on images it was actually dispatched. BYO/external registries use the user's own creds; only the canonical registry is switchboard-gated. |
| D12 | Federation / multi-location | An image's **identity is its manifest digest**; a *location* is a `(registry, repository)` that serves those bytes. An image may have **many locations** (D16): external/BYO (best-effort) and/or canonical (`skopeo copy`, dedup-cheap). Any location is interchangeable because every pull is digest-verified. "Guaranteed-schedulable / system" = "has a canonical-registry location." |
| D13 | Local GC + in-use protection | The per-server **daemon owns GC**; supervisors express "in use" as a **Zot reference** (per-job tag/referrer pinning the digest) that GC honors, instead of flock + lease files. Single writer ⇒ no multi-writer GC/ingest race. The canonical registry is protected by catalog references, not distributed leases. See §7. |
| D14 | Existing images | **None.** There is no legacy image corpus to migrate; we treat OCI as greenfield. The TOML format and `treadmill-rs/src/image/manifest.rs` are deleted at cutover (D-day) without any conversion tool. New images are produced directly as OCI by users (standard OCI tooling) and by the `tiny-efi` test fixture. |
| D15 | Signing | **Dropped.** Digest pinning already guarantees image integrity (no registry can swap content under a pinned digest), and the host already defends against untrusted *workloads*, which strictly dominate a malicious *image* — so provenance buys the host nothing. Signing would only add defense against a switchboard/catalog-DB compromise repointing a job at a malicious digest, which is low-value relative to the effort. The freed effort goes to host isolation (§11), the actual threat surface. |
| D16 | Image refs / locations | Each image has **one digest identity** and **one-or-more locations** (`image_locations`, §5.3). Enables registry redundancy (supervisor fails over across locations) and **promotion to a system image by adding a canonical location without removing the original ref** — the digest, and thus every existing reference/job, is unchanged. The dispatch carries an *ordered list* of `{ref, pull-token}` for failover. |

Decisions still wanting a human "go": **D10** (Zot vs Harbor — *tentatively Zot,
confirmed*). D12/D16 (multi-location + promotion) and D11 (token-gated pull)
are now settled per review. Signing (D15) is dropped; reopen only if defense
against a switchboard/catalog compromise becomes a requirement.

---

## 5. Data model

### 5.1 `treadmill-rs/src/image/` (replaces `manifest.rs`)

- Delete `manifest.rs` (the `GenericImageManifest`/extension machinery) and the
  TOML assumptions in its module doc.
- Add `media_types.rs`: `pub const` strings for the Treadmill media types and
  `artifactType`s (D1, D2, D4).
- Add `annotations.rs`: `pub const` keys for `ci.treadmill.role`,
  `ci.treadmill.qcow2.{head,lower,virtual-size}`, `ci.treadmill.target`,
  `ci.treadmill.board`, plus accessors that parse/validate them off an
  `oci_spec::image::{ImageManifest, Descriptor, ImageIndex}`.
- Add `image_ref.rs`: `ImageRef { registry: String, repository: String, digest:
  Digest }` with `Display`/`FromStr` (`registry/repo@sha256:…`) and `schemars`.
- `Digest` newtype wrapping `sha256:<hex>` (replaces `ImageId`'s raw `[u8;32]`
  at the API boundary; keep a `[u8;32]` accessor for filesystem paths).

### 5.2 `image_store` → `oci_store` (read-only client to the daemon)

Replace `image_store.rs`. The supervisor's store type is now a **read-only client
of the per-server Zot daemon** (§7), not a writer: digest→path mapping over the
ro-mounted `blobs/sha256/<hex>`, `ensure_present` (drives the daemon), presence
checks, manifest parse, and ref-lease hold/release (§7.3). Ingest, dedup, atomic
writes, and GC live in the daemon.

### 5.3 switchboard DB (new migrations)

```
images(                   -- identity is the digest, NOT a location (D16)
  id uuid pk,
  manifest_digest text unique,
  artifact_type text,                                      -- image vs group
  owner_subject uuid -> subjects,
  label text, attrs jsonb,
  created_at timestamptz, ...
)

image_locations(          -- N locations per image (D12/D16)
  image_id uuid -> images,
  registry text, repository text,
  status text check (status in ('external','canonical','system')),
  added_at timestamptz,
  primary key (image_id, registry, repository)
)                         -- promotion = INSERT a canonical/system location;
                          -- redundancy = multiple rows; digest never changes

image_groups(
  id uuid pk, index_digest text unique,
  owner_subject uuid, label text, ...
)                         -- group is itself an OCI index, pinned by digest;
                          -- its locations also live in image_locations

image_group_members(      -- denormalized for the matcher; rebuilt from the index
  group_id uuid -> image_groups,
  image_id  uuid -> images,
  arch text, os text, variant text,
  tml_target text, tml_board text,        -- selection axes from annotations
  primary key (group_id, image_id)
)
```

`jobs.image_id BYTEA` (SHA-256 of TOML manifest) is replaced by a reference to
`images.id` **or** an inline `ImageRef`, plus the resolved concrete
`manifest_digest` recorded at dispatch for reproducibility/audit.

---

## 6. Supervisor side (single-copy, direct files)

### 6.1 Fetch into the shared layout

`supervisor/lib/src/image_store_client.rs` → `oci_store.rs`:

- `ensure_present(digest, locations: &[(ImageRef, PullToken)]) -> Result<()>`:
  ask the **local Zot daemon** to make `digest` present — a pull-through from one
  of the dispatched `locations` (trying them in order, failing over on error,
  D16) using the per-location bearer **pull token** (D11), or a cache hit. The
  daemon is the only writer; concurrent `ensure_present` for the same digest is
  serialized inside it and idempotent (content-addressed). The supervisor does
  **not** write the store.
- `blob_path(digest) -> PathBuf`: returns the daemon storage's
  `…/blobs/sha256/<hex>`, opened **read-only** via the ro bind-mount, for direct
  use (single-copy, §6.3).
- `manifest(digest) -> ImageManifest`: read+parse the stored manifest blob (ro).

The `FetchingImage`/`ImageFetched` state machines in `supervisor/qemu/src/main.rs`
and `supervisor/nbd-netboot/src/main.rs` stay structurally the same; only the
client calls and the manifest accessors change (attrs → OCI annotations).

### 6.2 Runtime backing-chain construction (D3, D9)

Today both supervisors read `…head` / `…lower` / `…blob-virtual-size` *attrs* and
trust the backing path **baked into the qcow2** (`full_backing_filename`
canonicalization). New behavior:

- Read the chain from the **OCI manifest**: ordered layers, `ci.treadmill.qcow2.lower`
  annotations, `ci.treadmill.qcow2.head`.
- Build the chain explicitly at launch, pointing each node at its
  `blob_path(digest)`; never mutate or rebase the shared blobs:
  - QEMU VM: emit `-blockdev` nodes (overlay → … → base), lowers `read-only=on`.
  - NBD: emit a single `--image-opts` string with nested `backing.*` (overlay
    writable, lowers `read-only=on`). A helper in `treadmill-rs` emits **both**
    forms from one chain description so the two supervisors share logic.
- The per-job writable overlay is created with **no baked backing**
  (`qemu-img create -f qcow2 overlay.qcow2 <virtual-size>`); the backing comes
  entirely from the runtime args.
- Drop the `blob-virtual-size`-vs-header equality check's reliance on baked
  backing; keep the virtual-size sanity check against the annotation.

### 6.3 Single-copy guarantees

- One daemon-owned store per server; all supervisors and all `qemu`/`qemu-nbd`
  processes open the same `blobs/sha256/<digest>` inode (read-only) for the lower
  layers → one disk copy + shared page cache. The daemon writes once (§7); nobody
  else writes at all.
- The store and per-job overlays (`/run/treadmill/jobs/<id>/overlay.qcow2`) live
  on the same filesystem so any helper copy can `--reflink`.
- **Mount-namespace split (= §11 isolation):** the store is rw only in the
  daemon's namespace; supervisors and each per-job `qemu`/`qemu-nbd` sandbox get a
  **read-only bind-mount of `blobs/sha256`** (only the blobs they need). Same
  inode (single-copy preserved), but immutability is OS-enforced and the rest of
  the store is hidden from a compromised media process.

---

## 7. Per-server store daemon, leases & GC (the explicit ask)

**Problem:** multiple supervisors share one local store; concurrent
fetches/ingests/GC must not corrupt it, and GC must never delete a blob a running
`qemu`/`qemu-nbd` needs. **Solution:** a **single-writer daemon owns the store**;
supervisors never write it. We use **Zot** as that daemon rather than hand-rolling
one — it already provides serialized ingest, dedup, internal locking, GC, and the
OCI Distribution protocol, and its local storage is a standard OCI image layout
we can read directly.

This *resolves* (not contradicts) the single-copy requirement of §6: the daemon
is on the **management** path, not the **data** path. It fetches each blob **once**
into its store; consumers `open()` those same files read-only. One on-disk copy,
shared page cache — and the daemon-as-network-server (pull-through cache of the
canonical registry) comes for free.

### 7.1 Topology & namespaces

```
per-server Zot (own mount ns: store rw) ──┐ pull-through cache of canonical registry
   storage = standard OCI image layout    │
   /var/lib/treadmill/store/              │ ro bind-mount of blobs/sha256
     ├── oci-layout                       ▼
     └── blobs/sha256/<digest> ── supervisors + per-job qemu/qemu-nbd sandboxes
                                   (see only blobs/sha256, READ-ONLY)
```

- Store is **rw only in the daemon's mount namespace**; every consumer (and the
  network read path) sees `blobs/sha256` **read-only** → OS-enforced immutability
  (this *is* the §11 isolation mechanism, generalized). Same inode preserves
  single-copy + page-cache sharing.
- Push to the daemon is token-gated (D11); reads are by digest only.

### 7.2 Ensure-present (supervisor)

The supervisor no longer writes the store. On dispatch it asks the local daemon
to make digest `D` present — either by pulling through from the canonical
registry (lazy, using the dispatched pull token, D11/§6.1) or as a cache hit —
then opens `blobs/sha256/<D's blobs>` read-only for the backing chain (§6.2).
Concurrent ensure-present for the same `D` is serialized inside the daemon and is
idempotent (identical content-addressed bytes).

### 7.3 Leases as references (in-use protection)

We keep the lease *concept* but express it **natively to the daemon** instead of
via flock + lease files:

- Before use, the supervisor pins `D` as a **Zot reference** (a per-job tag or
  referrer, e.g. `inuse/<job-id>` → `D`); on teardown it removes it. A crashed
  supervisor's references are reaped by a TTL/reconcile sweep keyed on live jobs.
- Configure Zot GC to **honor references / untagged-retention**, or disable Zot's
  periodic GC and trigger GC ourselves between safe points. Either way, an image
  referenced by a live job is never collected.
- Pinning the **manifest/index digest** is sufficient — the daemon's GC computes
  reachability, transitively protecting config + all layer blobs.

### 7.4 Why this is race-free

- All mutation is serialized through one writer (the daemon); supervisors can
  only *read*, so there is no multi-writer ingest/rename/GC race to reason about.
- GC honors live references, so an in-use digest is retained.
- Defense in depth: on Linux an already-`open()`ed blob survives `unlink`, so a
  running job is correct even against a mis-timed GC; the only residual hazard is
  a not-yet-opened blob in a chain, which the reference in §7.3 covers, and which
  a re-ensure-present would otherwise repair (idempotent, content-addressed).
- Blobs are immutable, so there is never a torn-content hazard — only a presence
  hazard, which references prevent and re-pull repairs.

> **Fallback if the Zot direct-read coupling is unacceptable.** The only thing we
> depend on is "the daemon's local storage is a standard OCI image layout we can
> ro-mount." If that operational coupling is undesirable, replace Zot with a thin
> custom daemon exposing the *same* OCI layout + a hold/release lease API + an
> ensure-present RPC — strictly more code we own, same external shape. Try
> Zot-direct first.

### 7.5 Canonical-registry GC (no distributed lease)

The canonical registry is **not** protected by supervisor leases. Instead:

- Every **registered** image stays reachable (referenced by a catalog row →
  kept tagged / kept in its group index); switchboard never removes the registry
  reference while the catalog row exists.
- A **running** job is already protected locally (it holds a lease on its own
  pulled copy), so even if the canonical copy were GC'd mid-job the job is
  unaffected.
- Registry GC therefore only ever reclaims un-referenced uploads — standard Zot
  GC, no Treadmill-specific protocol.

---

## 8. Switchboard side

### 8.1 Catalog & registration

- `POST /images` (auth'd): body `{ registry, repository, manifest_digest }`.
  Switchboard **pulls the manifest by digest**, validates it is a Treadmill
  artifact (artifactType, media types, required annotations, blob sizes), records
  an `images` row + first `image_locations` row. Promotion/mirror = `skopeo copy`
  into the canonical registry then INSERT a `canonical`/`system` location (D16);
  the digest and all existing references are unchanged.
- `POST /image-groups`: register an index digest; switchboard pulls the index,
  validates each member, and populates `image_group_members` for the matcher.
- `GET /images`, `GET /image-groups`: list/inspect (ownership-scoped).

### 8.2 Token issuer (D11)

- Implement the Docker v2 token endpoint in `switchboard/src/routes/` backed by
  `auth`/`token`. Mint short-lived JWTs scoped to a **single repository**, sign
  with a key Zot trusts. Zot is configured **non-anonymous for pull and push**.
  - **Pull**: at job dispatch, switchboard mints `repository:<repo>:pull` tokens
    (one per candidate location) and includes them in the dispatch message; the
    supervisor presents them to `oci-client` (§6.1). No standing registry
    credentials live on the host — tokens are per-job, per-repo, short-TTL.
  - **Push**: a user requests a `repository:<repo>:push` token out-of-band, then
    `oras push` (cross-repo blob mount for reused base layers) → `POST /images`
    to register the digest. Unauthorized pushes never reach storage, so we don't
    pay their bandwidth.
- Zot config: `auth.bearer` realm/service pointed at switchboard.

### 8.3 Scheduler: group → concrete image

- `JobInitSpec::Image { image_ref }` (concrete) **or**
  `JobInitSpec::ImageGroup { group_id }` (resolved at dispatch).
- After a host is chosen, the matcher reads host attributes (architecture +
  Treadmill target/board, sourced from host registration / the to-be-specified
  `tag_config`) and selects the `image_group_members` row whose
  `arch/os/variant/tml_target/tml_board` match, yielding a concrete
  `manifest_digest`.
- The dispatched `ImageSpecification` carries the concrete `manifest_digest`
  plus an **ordered list of `{ImageRef, pull-token}`** (one per `image_locations`
  row, canonical/system preferred) so the supervisor pulls without re-resolving
  and can fail over across locations (D16). Record the resolved digest on the job
  row.

### 8.4 Protocol/type changes (drift-guarded)

- `treadmill-rs/src/api/switchboard.rs`: `JobInitSpec::Image { image_id }` →
  `Image { image_ref }` + `ImageGroup { group_id }`.
- `treadmill-rs/src/api/switchboard_supervisor.rs`: `ImageSpecification::Image
  { image_id }` → `Image { manifest_digest, locations: Vec<{ image_ref,
  pull_token }> }` (ordered, for token-gated pull + failover, D11/D16).
- Regenerate the committed JSON Schema snapshots; the diff test classifies this
  as a **breaking** change (acceptable: pre-1.0, coordinated cutover).

---

## 9. Phased implementation

Phases are independently reviewable; each lands behind tests and keeps the tree
building. Earlier phases (0–3) are pure supervisor/format work and can merge
before any switchboard catalog exists. **Every phase ships with the tests that
verify it in isolation** (§12) — a phase is "done" only when its own tests pass,
so we never have to advance to the next phase to gain confidence in this one.
Each phase below names its verifying test target.

### Phase 0 — Format & types (no behavior change)
- Add `oci-spec` + `oci-client` to the workspace.
- New `treadmill-rs/src/image/`: `media_types.rs`, `annotations.rs`,
  `image_ref.rs`, `Digest`, `parse.rs` (OCI manifest/index → validated Treadmill
  view). Keep old `manifest.rs` compiling in parallel until cutover.
- No legacy conversion (D14): OCI is greenfield. The only producer we need now is
  the one that builds the **`tiny-efi` aarch64 two-layer fixture** (§12.2) via Nix.
- **Verified by (§12):** `cargo test -p treadmill-rs image::` — annotation/
  `ImageRef`/`Digest` round-trips + `parse` validation against real-wire-format
  manifest/index JSON; `nix build .#checks.<sys>.tiny-efi-image` builds + reparses
  the fixture with `oci-spec`. No daemon/qemu needed.

### Phase 0.5 — Supervisor job-core test seam (refactor, no behavior change)
- Extract the in-process-drivable job core from `supervisor/qemu` and
  `supervisor/nbd-netboot` (*ensure-present → build chain → launch → Ready →
  teardown*) behind a trait, with the launcher (`qemu`/`qemu-nbd`/`qemu-img`
  invocations) injectable. Add a tiny **direct/fake-switchboard driver** that
  feeds an `ImageSpecification` and observes reported states. `supervisor/mock`
  is **deleted** (not migrated) — the fake-switchboard driver supersedes it.
- This is pure refactor of the *current* (still-TOML) code, so it can land before
  any OCI behavior change and de-risks every later supervisor test.
- **Verified by:** `cargo test -p treadmill-supervisor-lib core::` — drive the
  core with a **stub launcher**; assert the `FetchingImage → ImageFetched →
  Ready → Terminated` transitions and that the launcher received the expected
  arguments. No real qemu yet (logic only).

### Phase 1 — Per-server daemon + supervisor store client
- Deploy the **per-server Zot** (nix module): pull-through cache of a dev
  canonical Zot, storage = OCI image layout, own mount ns (store rw), ro
  bind-mount of `blobs/sha256` for consumers (§7.1).
- Replace `image_store_client.rs` with `oci_store.rs` (`ensure_present` driving
  the local daemon; `blob_path`/`manifest` reading ro files; §6.1). Wire into
  both supervisors' `FetchingImage` states via the Phase 0.5 seam. Still uses
  baked backing for now.
- **Verified by:** `cargo test -p treadmill-supervisor-lib oci_store::` against a
  **real child Zot** (§12.3) — `ensure_present` lazily caches from the dev
  canonical, byte-for-byte digest verification, location failover. NixOS check
  `nix build .#checks.<sys>.store-ro-mount` (§12.4) asserts the consumer
  bind-mount is read-only (writes `EROFS`).

### Phase 2 — Runtime backing chains (D3/D9) + first real boot
- Add the chain-description type + dual emitter (`-blockdev` / `--image-opts`) in
  `treadmill-rs`. Switch `supervisor/qemu` and `supervisor/nbd-netboot` to build
  chains from OCI annotations and stop trusting baked backing; create overlays
  with no baked backing.
- **Verified by:**
  - `cargo test -p treadmill-rs blockdev::` — `insta` golden tests of a 3-layer
    chain → `-blockdev` args and `--image-opts` string (§12.5).
  - `cargo test -p treadmill-supervisor-lib chain_qemu_img::` — feed the emitted
    options to **`qemu-img info`** and assert the reported backing chain, virtual
    sizes, and read-only nodes (oracle, no boot).
  - `cargo test -p treadmill-supervisor-qemu boot::tiny_efi` — **the host-level
    aarch64 boot test** (§12.6): drive the QEMU core (Phase 0.5 seam) with the
    two-layer `tiny-efi` image under non-accelerated `qemu-system-aarch64 -M
    virt` + AAVMF, capture serial, assert `TREADMILL_OK rev=1` appears and
    `BASE-ONLY` never does. Skips cleanly if qemu/AAVMF are absent.

### Phase 3 — Leases-as-references & GC (§7.3–7.4)
- Supervisor pins/unpins a per-job Zot reference (`inuse/<job-id>` → digest)
  around job execution; reconcile sweep reaps references of dead jobs. Configure
  Zot GC to honor references (or drive GC ourselves between safe points).
- **Verified by:** `cargo test -p treadmill-supervisor-lib lease::` against a
  **real child Zot** — pin a reference, trigger GC, assert the in-use closure is
  retained while an unreferenced digest is collected; a parallel `ensure_present`
  of the same digest stays consistent. (This is also where we confirm the §7.3
  assumption that Zot GC honors references — against the real binary.)

### Phase 4 — Switchboard catalog & scheduler
- DB migrations (§5.3). `images`/`image-groups` registration + list routes with
  manifest validation. Matcher (group + host attrs → concrete digest).
- Protocol change (§8.4) + schema snapshot regen. Replace `jobs.image_id` plumbing
  in `switchboard/src/sql/job.rs` and the dispatch path.
- **Verified by:** `cargo test -p switchboard routes::images` (mirroring the
  `users` route tests) — registration validates against a child Zot + writes an
  audit entry; `matcher::` unit tests (group + host attrs → concrete digest);
  `cargo sqlx prepare --check`.

### Phase 5 — Canonical registry, tokens, push, CLI
- Promote the dev canonical Zot to a production deployment (nix module); the
  per-server Zots (Phase 1) become its pull-through caches. Token endpoint in
  switchboard trusted by both tiers.
- `tml image push` (scoped token → `oras push` → register) and `tml image
  add-layer` (cross-repo mount + new manifest).
- **Verified by:** `cargo test -p switchboard registry_token::` (mint/verify a
  scoped token a child Zot accepts) + `cargo test -p cli image::` (`assert_cmd`:
  push from scratch, add a `rev=2` layer via cross-repo mount, re-register).

### Phase 6 — Federation & promotion
- Multi-location support (§5.3): `image_locations`, `skopeo copy` mirroring,
  promote-to-system (add canonical location, digest unchanged, D16), dispatch
  location list + per-location pull tokens, supervisor failover.
- **Verified by:** `cargo test -p switchboard promotion::` with **two child
  Zots** — promote a user image and assert the original ref still resolves;
  supervisor `oci_store` failover test where the first location is unreachable
  and the second serves the (digest-identical) bytes.

### Phase 6.5 — Host isolation (§11)
- Sandbox each per-job `qemu`/`qemu-nbd` (bwrap or hardened systemd unit):
  RO bind-mounted lower blobs only, per-job overlay rw, dropped caps, seccomp
  (`qemu -sandbox on`), per-job uid/namespaces, NBD socket bound to the board's
  link only. Audit the puppet control-socket parser for untrusted input.
- **Verified by (NixOS VM tests, §12.4):**
  - `.#checks.<sys>.sandbox-isolation` — a process in the sandbox cannot open a
    sibling job's blob and cannot write a lower (`EROFS`/`EACCES`).
  - `.#checks.<sys>.nbd-netboot` — a **second test node acting as the board**
    netboots the `tiny-efi` root over TFTP+NBD served by the supervisor node's
    `qemu-nbd --image-opts` chain; assert the board reaches the sentinel.
  - `cargo test -p treadmill-rs control_socket::fuzz` — round-trip/bounds tests
    of the puppet control-socket decoder against adversarial input.

### Phase 7 — Cutover & cleanup
- Delete `treadmill-rs/src/image/manifest.rs`, the TOML store, and the legacy
  `image_id` paths. Update `doc/` architecture SVG. Archive this plan.

### Phase 8 (optional) — Unify the supervisor job state machine
- The QEMU and NBD-netboot supervisors are near-identical state machines
  (`FetchingImage → ImageFetched → Running → Stopping`, with parallel
  `fetch_image`/`start_job_cont`/`process_monitor`/`stop_job_internal`/
  `finish_running_job_shutdown`). Phase 0.5 already shared the *subprocess* seam
  (`ProcessLauncher`); this step hoists the **state machine itself** into a
  generic core in `supervisor/lib`, parameterized over the target-specific bits
  (image validation/backing-chain prep, workload launch args, extra teardown
  like boot-archive unpack and start/stop scripts) behind a trait.
- Strictly a refactor with no behavior change, and **optional** — deferred to the
  end because it is delicate (the locking/`std::mem::replace` state dance and the
  `process_monitor`↔`stop_job` race must be preserved exactly) and buys
  maintainability rather than capability. Best tackled once both supervisors are
  on the OCI path so the unified core is written against the final shapes.
- **Verified by:** the existing Phase 0.5 transition test (now driving the shared
  core for *both* targets via their trait impls) plus the Phase 2 boot test.

---

## 10. Risks & items needing a human decision

- **Host attribute source.** Selection (§8.3) depends on host arch/target/board
  attributes, which overlap with the still-"FIXME: TO BE SPECIFIED"
  `tag_config`. Phase 4 must pin that schema down; tracked as a dependency.
- **NBD `--image-opts` nesting depth.** Deep `backing.backing.*` strings are
  fiddly; Phase 2 includes a builder + a real `qemu-nbd` smoke test rather than
  hand-written strings.
- **External-location availability.** A user deleting their registry breaks
  images that have *only* an external location; surfaced at schedule time as an
  `ImageError`, and the rationale for promoting/mirroring to a canonical location
  (§5.3, D16).
- **Host isolation is a hard requirement, not a nicety** (§11). `qemu-nbd`
  parses untrusted NBD input from a potentially-compromised board with access to
  the host store; Phase 6.5 is mandatory before NBD targets run untrusted
  tenants in production. Tracked as a release blocker, not a "revisit."
- **Zot direct-read coupling** (§7). Consumers `open()` the daemon's blob files,
  depending on its storage being a standard OCI image layout we can ro-mount.
  Reads of immutable content-addressed blobs are safe and writes are the daemon's
  serialized job, so this is an *operational* coupling (storage path/layout), not
  a correctness one. Fallback: a thin custom daemon over the same layout (§7.4).
- **Breaking protocol change.** Cutover is coordinated (pre-1.0, few images);
  no dual-format runtime is maintained.

---

## 11. Host isolation & threat model

Treadmill hosts run **untrusted workloads**, so the host must defend against
arbitrary guest code by assumption. Signing is therefore *not* part of the
defense (D15): a malicious image is strictly weaker than the malicious workload
the host already tolerates, and digest pinning already prevents content
substitution. The real surfaces are the **trusted host processes that handle
untrusted input**.

**The store is structurally protected** by the §7 daemon split: it is rw only in
the daemon's mount namespace, and every consumer below sees `blobs/sha256`
read-only. So even a fully compromised `qemu-nbd` cannot mutate the shared store
or reach blobs outside its bind-mount — the isolation and the concurrency model
are the same mechanism.

### 11.1 `qemu-nbd` (highest risk)
The untrusted workload runs on a **physical board** and drives `qemu-nbd` over
the NBD protocol. `qemu-nbd` is thus a trusted host process parsing
attacker-controlled NBD requests and qcow2 operations, with filesystem access to
the shared store. Unsandboxed, an exploit reads or corrupts *other jobs'* images.
**Mitigation (Phase 6.5):** one `qemu-nbd` per job inside a **bwrap**/hardened
systemd sandbox with

- only this job's overlay (rw) + its specific lower blobs **bind-mounted
  read-only** — never the whole CAS (this is OS-enforced immutability, and stays
  single-copy/page-cache-shared because it's the same inode, §6.3);
- dropped capabilities, seccomp, a dedicated uid, and user/mount/pid/net
  namespaces;
- the NBD listener bound only to the target board's link/VLAN.

### 11.2 `qemu-system-*`
A strong boundary but a large attack surface (device-emulation breakouts). Run
with `-sandbox on` (seccomp), dropped privileges, and the same per-job
overlay-only FS view + confinement.

### 11.3 Puppet control socket
`treadmill-rs/src/control_socket.rs` / `api/supervisor_puppet.rs` carry messages
from *inside* an untrusted guest to the supervisor. Audit the supervisor-side
decoder for untrusted-input handling (bounds, allocation limits, no trust in
guest-supplied lengths). Flagged for review; not yet assessed in detail.

### 11.4 TFTP boot path
Netboot boot files are served to the board read-only; ensure per-target
isolation and that the served boot dir is not writable by the workload.

---

## 12. Testing strategy

**Principle:** prefer Rust-internal tests; reach for NixOS VM tests only when the
kernel, mount namespaces, or a live netboot client is itself the thing under
test. Avoid full system integration tests — there is no "boot a real switchboard
+ supervisor + board" test in this plan. Each phase (§9) is verified by its own
named target so it can be signed off without advancing.

### 12.1 Tiers

| Tier | Mechanism | Used for |
|---|---|---|
| 1 — pure Rust | `cargo test`, `insta` snapshots | annotations, `ImageRef`/`Digest`, `parse` validation, the backing-chain emitter, GC reachability |
| 2 — Rust integration | `cargo test` spawning real **child Zot** / real **qemu** | `ensure_present`, leases/GC against the real Zot, `qemu-img` chain validation, **the aarch64 boot test** |
| 3 — NixOS VM (`nixosTest`, flake checks) | full machines under the test driver | ro bind-mount enforcement, bwrap sandbox, NBD netboot with an emulated board |

Nothing uses hardware acceleration: all qemu runs are **TCG**, so the
arch is irrelevant to the host and we standardize on **aarch64** everywhere
(`qemu-system-aarch64 -M virt` + AAVMF), matching the real RPi/QEMU-aarch64
targets.

### 12.2 The `tiny-efi` fixture (shared)
A `no_std` Rust UEFI app (`aarch64-unknown-uefi`, `uefi` crate) that prints a
sentinel to the console (→ serial) and calls `ResetSystem(Shutdown)`. Nix packs
it into a FAT ESP → qcow2, built as **two layers** to prove chain assembly:

- **base** layer: `BOOTAA64.EFI` prints `BASE-ONLY` — a tripwire for a
  mis-assembled chain;
- **overlay** layer (backed by base): overwrites `BOOTAA64.EFI` to print
  `TREADMILL_OK rev=1`.

Booting the **head** must yield the overlay's message; seeing `BASE-ONLY` (or a
boot failure) fails the test. A `rev=2` overlay variant exercises add-a-layer
(Phase 5). The fixture is committed as a Nix derivation, tiny, network-free.

### 12.3 Real child Zot (shared)
A test fixture launches a real `zot` (from the flake) on a random port with a
temp storage dir; torn down at end. Used wherever registry *behavior* matters —
pull-through (Phase 1), and especially **GC honoring references** (Phase 3),
which pins the §7.3 assumption against the real implementation rather than a
mock. Lighter pull-only tests may use `oci-client` directly.

### 12.4 NixOS VM tests (the only tier-3 checks)
- `store-ro-mount` (Phase 1): the per-server Zot owns the store rw in its ns; a
  consumer sees `blobs/sha256` ro and a write returns `EROFS`.
- `sandbox-isolation` (Phase 6.5): a process in a per-job sandbox cannot open a
  sibling job's blob and cannot write a lower.
- `nbd-netboot` (Phase 6.5): **two top-level nodes** — a supervisor node serving
  TFTP + `qemu-nbd --image-opts`, and a **board node** that netboots the
  `tiny-efi` root over the virtual network. Both are driver-level nodes, so no
  nested virtualization; assert the board reaches the sentinel.

### 12.5 The backing-chain emitter (highest-value Tier-1)
The emitter (chain description → `-blockdev` graph / `--image-opts` string) is
pure and is the riskiest *correctness* surface, so it is golden-snapshotted
(`insta`) and additionally validated by `qemu-img info` (Tier-2) — both without
booting. The boot test (§12.6) then confirms the whole path once.

### 12.6 The aarch64 boot test (host-level, Tier-2)
Drive the QEMU supervisor core (Phase 0.5 seam) with the two-layer `tiny-efi`
image, launch non-accelerated `qemu-system-aarch64 -M virt` + AAVMF with
`-nographic -serial …`, and assert the serial output contains `TREADMILL_OK
rev=1` and never `BASE-ONLY`. Runs directly on the CI host (no NixOS VM, no
nesting); gated on `qemu`/AAVMF being present (provided by the flake), skipping
cleanly otherwise.

### 12.7 What is deliberately *not* tested end-to-end
Switchboard↔supervisor over a live WS connection, and the full schedule→dispatch
→run loop, are covered by their component tests (protocol schema snapshots; the
Phase 0.5 in-process driver; route tests) rather than a standing integration
environment. If a true smoke test is ever wanted, it is the `nbd-netboot` VM test
extended with a switchboard node — explicitly out of scope here.
