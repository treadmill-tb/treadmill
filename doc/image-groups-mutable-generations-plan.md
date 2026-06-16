# Transition plan: image groups as mutable, generationed switchboard entities

Status: proposed. Supersedes the OCI-image-index model of groups described in
`doc/oci-image-migration-plan.md` §5/§8.3 and `doc/images-oci-migration-plan.md`
§I4–I6.

## 1. Motivation and the model we are moving to

Today a "group" is a frozen OCI image index: a content-addressed artifact built
by Nix (`images/lib/mk-group-index.nix`), pushed to a registry, and registered
with the switchboard by its **index digest**. Jobs reference that digest. This
conflates two concerns that must be separated:

- **Authoring handle** — what a user puts in a job spec ("Ubuntu 22.04"). Wants
  to be a *stable name* and a *moving target*.
- **Reproducibility pin** — the exact bytes that ran. Wants to be *immutable* and
  *content-addressed*.

New model (the registry tag-vs-digest split, applied to groups):

- A **concrete image** stays a content-addressed `images` row, registered
  independently whenever each variant is built. Unchanged.
- An **image group** becomes a *mutable, named* switchboard entity: a stable
  `id` + a unique `name` (the moving-target handle) + an append-only series of
  **generations**.
- A **generation** is an *immutable, full-replacement snapshot* of the group's
  members. Every edit creates generation N+1. Members are
  `(image, required_host_tags, index)` triples.
- A **job** references a group by `id` plus an optional generation. Omitted ⇒ the
  **latest generation is resolved and frozen at enqueue time** (written onto the
  job row as an integer). A job may alternatively reference a single image by
  `id` (equivalent to a one-generation, one-member group).
- **Resolution to a concrete image still happens at dispatch** (host-dependent),
  recorded in `jobs.resolved_image_digest`. The generation freeze only fixes the
  *candidate set*; the concrete pick may differ per host, which is acceptable.

### Decisions already taken (from design discussion)

1. Concrete image selection is host-dependent, so it is necessarily a
   schedule-time decision; "reproducible" therefore means per-host. Acceptable.
2. Recency comes from **generations**: default to the latest generation. *Within*
   a generation, specificity wins (largest `required_host_tags`), ties broken by
   an explicit per-member `index`.
3. Images and generations are **never deletable** — metadata is immortal. Only an
   image's *data sources* (`image_locations`) may be GC'd. A job whose chosen
   image has no live location **fails at dispatch** (image-error path).
4. Generations are **immutable** once created (append-only; every membership edit
   is a new generation).
5. Empty/unsatisfiable resolution is a **dispatch failure**, not an enqueue
   reject (host is unknown at enqueue).
6. A job binds the group's stable `id` (+ generation); `name`/`label` are
   human-facing only.
7. `required_host_tags` source of truth is the **generation-creation API
   payload** (CI-supplied). No annotation, no OCI index, no Nix artifact.
8. Permission model: ownership + an `image_group_grants` table with two
   permissions, `use` and `manage` (owner holds `manage` implicitly).

### Defaults chosen here (flag if you disagree — see §10)

- `image_groups.name` is **globally unique** (like `tml_switchboard.groups.name`).
- Holding `use` on a group lets you run its member images even if you do not own
  them individually; dispatch does **not** re-check member-image ownership.
- Job enqueue checks `use` on the referenced group and resolves/freezes the
  generation at enqueue; a missing group/generation or missing `use` is a
  **400/403 at enqueue** (a change from today's "defer everything to dispatch",
  forced by the need to freeze the generation).

---

## Phase 1 — Remove all group infrastructure from Nix and CI

No build artifact represents a group any longer. Groups are pure switchboard
state, populated over the API.

### 1.1 Delete files
- `images/groups.nix`
- `images/lib/mk-group-index.nix`

### 1.2 `images/lib/media-types.nix`
- Remove `groupArtifactType` only (group-exclusive). Keep `ociIndex`: it is also
  used by `mk-treadmill-image.nix` for the single-image OCI layout's top-level
  index.json.

### 1.3 `nix/images.nix`
- Remove the `mkGroupIndex` import (line ~33).
- Remove `groupMembers`, `groupDefs`, `groupPackages` (lines ~169–173, ~225).
- Remove the `groups = …` block from `parseSpec` (lines ~198–202).
- Remove `groupPackages` from the `packages` attrset (line ~236).
- Update the module header comment (lines 6, 12–15) to drop `packages.group-*`.

### 1.4 `nix/checks.nix`
- Line ~364: drop the `lib.hasPrefix "group-" name` clause from the
  package→check exclusion predicate.

### 1.5 `.github/workflows/images.yml`
- Remove the four `group-*` targets from the `nix build` step.
- Remove `push "group-${name}" …` and rewrite the push comment (drop the
  `<name>-group` repo namespacing paragraph).
- Update the "Deferred seam: switchboard registration" trailing comment: group
  registration is no longer an index push; it is a `POST /image-groups` +
  `POST /image-groups/{id}/generations` call (see Phase 5). Leave the actual
  registration step deferred.

### 1.6 Verify
- `nix flake check` and `nix build .#packages.<sys>.images-parse` still evaluate
  with no group references remaining.

---

## Phase 2 — Shared Rust types (`treadmill-rs`)

### 2.1 Delete the OCI-index-group parsing surface
In `treadmill-rs/src/image/`:
- `parse.rs`: remove `GroupMember`, `parse_group`, `ParseError::NotTreadmillGroup`,
  and the `parses_image_group` / `rejects_non_treadmill_artifact_type` group
  tests. Keep `parse_image` and all image-side parsing.
- `media_types.rs`: remove `IMAGE_GROUP_ARTIFACT_TYPE` and the OCI-index media
  type constant if now unused.
- `annotations.rs`: remove `REQUIRED_HOST_TAGS` and `parse_tag_list` (tags now
  arrive as a JSON array, not a CSV annotation). Keep image annotations
  (role/title/version/…).

### 2.2 `treadmill-rs/tests/images_parse.rs`
- Remove the `groups` section of the spec and all group assertions; keep the
  per-image checks.

### 2.3 Image-catalog API types (`treadmill-rs/src/api/switchboard/images.rs`)
Remove `RegisterImageGroupRequest`, the `index_digest`/`members`-from-index
shape of `ImageGroupInfo`, and the index-based `ImageGroupMember`. Add:

```rust
// Create an empty, named group. POST /image-groups
pub struct CreateImageGroupRequest { pub name: String, pub label: Option<String> }

// One member of a new generation; `index` is the array position (tie-break).
pub struct GenerationMemberSpec {
    pub image_id: Uuid,                 // must already be registered via POST /images
    pub required_host_tags: Vec<String>,
}

// Full-replacement new generation. POST /image-groups/{id}/generations
pub struct CreateGenerationRequest { pub members: Vec<GenerationMemberSpec> }

pub struct ImageGroupInfo {
    pub id: Uuid,
    pub name: String,
    pub label: Option<String>,
    pub owner_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub latest_generation: Option<u32>, // None for a group with no generations yet
}

pub struct GenerationMemberInfo {
    pub image_id: Uuid,
    pub manifest_digest: Digest,
    pub required_host_tags: Vec<String>,
    pub index: u32,
}
pub struct ImageGroupGenerationInfo {
    pub group_id: Uuid,
    pub generation: u32,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub members: Vec<GenerationMemberInfo>,
}

// Grants. POST/GET/DELETE /image-groups/{id}/grants
pub enum ImageGroupPermission { Use, Manage }
pub struct ImageGroupGrantRequest { pub subject_id: Uuid, pub permission: ImageGroupPermission }
pub struct ImageGroupGrantInfo { pub subject_id: Uuid, pub permission: ImageGroupPermission }
```

### 2.4 Job init spec (`treadmill-rs/src/api/switchboard.rs`, `enum JobInitSpec`)
- `Image { image: Digest }` → `Image { image: Uuid }` (image id).
- `ImageGroup { image_group: Digest }` →
  `ImageGroup { image_group: Uuid, generation: Option<u32> }`.
- Update doc comments.

### 2.5 Job view (`treadmill-rs/src/api/switchboard/jobs.rs`, `enum JobImageRef`)
- `Image { digest }` → `Image { image_id: Uuid }`.
- `ImageGroup { digest }` → `ImageGroup { group_id: Uuid, generation: u32 }`.
- `resolved_image_digest` stays as the separately-recorded concrete pick.

---

## Phase 3 — Switchboard DB schema

Edit `switchboard/SCHEMA.sql` (source of truth) **and** add one migration in
`switchboard/migrations/` (timestamped). `./migrate.sh -v` must show no drift.
This is a destructive change to the image-group tables; acceptable pre-production
(no group data to preserve).

### 3.1 Replace the image-group tables (IMAGE CATALOG section)

Drop `image_group_members` and the `index_digest` column; restructure:

```sql
-- A named, mutable image group. `name` is the stable moving-target handle a job
-- references (by id); membership lives in immutable per-generation snapshots.
-- Groups are never deleted (metadata is immortal), so jobs that pinned a
-- generation always resolve.
create table tml_switchboard.image_groups
(
    id            uuid not null primary key,
    name          text not null unique,
    owner_subject uuid references tml_switchboard.subjects (subject_id) on delete set null,
    label         text,
    created_at    timestamp with time zone not null default current_timestamp
);

-- One immutable snapshot of a group's membership. Every edit appends a new
-- generation (per-group monotonic from 1). Append-only: a trigger blocks
-- UPDATE/DELETE (same pattern as audit_events). `created_by` is provenance.
create table tml_switchboard.image_group_generations
(
    group_id   uuid not null references tml_switchboard.image_groups (id) on delete cascade,
    generation int  not null,
    created_by uuid references tml_switchboard.subjects (subject_id) on delete set null,
    created_at timestamp with time zone not null default current_timestamp,

    primary key (group_id, generation),
    constraint generation_positive check (generation >= 1)
);

-- The members of ONE generation. Full replacement per generation; image
-- selection is host-tag-only. `index` is the explicit array position, used as
-- the deterministic tie-break among equally-specific admissible members.
-- image_id references an immortal images row (no cascade: images never deleted).
create table tml_switchboard.image_group_members
(
    group_id           uuid   not null,
    generation         int    not null,
    image_id           uuid   not null references tml_switchboard.images (id),
    required_host_tags text[] not null default '{}',
    index              int    not null,

    primary key (group_id, generation, image_id),
    unique (group_id, generation, index),
    foreign key (group_id, generation)
        references tml_switchboard.image_group_generations (group_id, generation)
        on delete cascade
);

-- Append-only enforcement for generations (reuse the audit trigger function or
-- a dedicated copy of deny_*_change()).
create trigger image_group_generations_append_only
    before update or delete on tml_switchboard.image_group_generations
    for each row execute function tml_switchboard.deny_audit_event_change();
```

(If reusing `deny_audit_event_change` reads oddly, add a small
`deny_generation_change()` twin; functionally identical.)

### 3.2 Permissions

```sql
create type tml_switchboard.image_group_permission as enum ('use', 'manage');

create table tml_switchboard.image_group_grants
(
    group_id   uuid not null references tml_switchboard.image_groups (id) on delete cascade,
    subject_id uuid not null references tml_switchboard.subjects (subject_id) on delete cascade,
    permission tml_switchboard.image_group_permission not null,
    granted_at timestamp with time zone not null default current_timestamp,

    primary key (group_id, subject_id, permission)
);
```

No `revocable`/irrevocable trigger: image-group grants are all user-managed
(unlike job grants, which carry fixed grants). Authorization is the standard
ownership ∨ grant disjunction evaluated via `principals()` (see `auth/engine.rs`).

### 3.3 Jobs table

In `tml_switchboard.jobs`:
- Drop `image_digest text`, add `image_id uuid references tml_switchboard.images (id)`.
- Drop `image_group_digest text`, add `image_group_id uuid` +
  `image_group_generation int`, with
  `foreign key (image_group_id, image_group_generation) references
   tml_switchboard.image_group_generations (group_id, generation)` (on delete
  no action — immortal).
- Replace `resolved_image_digest text` with `resolved_image_id uuid references
  tml_switchboard.images (id)` (on delete no action — images are immortal); the
  concrete digest is recovered by join. Update `set_resolved_image` and the
  dispatch-reconstruction path (`sql/job.rs` ~881–907, `scheduler.rs` ~513)
  accordingly, and `JobInfo.resolved_image_digest` is populated via that join.
- Rewrite `valid_init_spec`:

```sql
constraint valid_init_spec check (
  (resume_job_id is null
     and (image_id is not null)::int
       + (image_group_id is not null)::int = 1
     and (image_group_id is null) = (image_group_generation is null))
  or (resume_job_id is not null and restart_job_id is null
        and image_id is null and image_group_id is null
        and image_group_generation is null)
)
```

Update the column-group doc comment above the constraint accordingly.

### 3.4 Generation numbering concurrency
Allocation happens in the create-generation transaction (Phase 4): take
`pg_advisory_xact_lock(hashtext('image_group_gen:' || group_id))`, compute
`coalesce(max(generation),0)+1`, insert generation + members, commit. Mirrors the
advisory-lock pattern already used by `group_members_no_cycle`.

---

## Phase 4 — SQL data access (`switchboard/src/sql/image.rs`)

Remove `GroupRecord.index_digest`, `fetch_group_by_digest`, `insert_group`
(digest form), `clear_members`, and the digest-keyed `members_for_group`. Add:

- `create_group(conn, id, name, owner, label)` → insert into `image_groups`.
- `fetch_group_by_id(conn, id) -> Option<GroupRecord>` and
  `fetch_group_by_name(conn, name) -> Option<GroupRecord>`.
- `list_owned_groups` — same as today but selecting the new columns.
- `create_generation(tx, group_id, created_by, members: &[(image_id,
  required_host_tags, index)]) -> Result<u32>` — advisory-lock + `max+1` + insert
  generation row + insert all member rows; returns the new generation number.
- `latest_generation(conn, group_id) -> Option<u32>`.
- `members_for_generation(conn, group_id, generation) -> Vec<GroupMemberRecord>`
  ordered by `index`; `GroupMemberRecord` gains nothing structurally (already
  carries `image_id`, `manifest_digest`, `required_host_tags`); rename its
  `position` field to `index`.
- Grant helpers: `grant_image_group`, `revoke_image_group`,
  `list_image_group_grants`, and an authorization predicate
  `subject_has_group_permission(conn, subject, group_id, permission) -> bool`
  (ownership-reach ∨ matching grant via `principals()`).

---

## Phase 5 — REST routes (`switchboard/src/routes/images.rs`, `routes/mod.rs`)

Replace `register_image_group` / `ensure_member_image` (index-pull path) with:

- `POST /image-groups` → `create_image_group` (name + label). Caller becomes
  owner. 201.
- `GET /image-groups` → list owned/reachable (unchanged shape, new fields).
- `GET /image-groups/{id}` → `ImageGroupInfo` (+ `latest_generation`). Visibility:
  owner-reach ∨ `use`/`manage` grant.
- `POST /image-groups/{id}/generations` → `create_generation`. Requires
  `manage`/ownership. Body is the full member list (`GenerationMemberSpec[]`).
  Validates every `image_id` exists (FK also enforces); records member rows with
  array order as `index`. Returns `ImageGroupGenerationInfo`. **No member pull**
  — images are pre-registered via `POST /images`; `required_host_tags` come from
  the payload.
- `GET /image-groups/{id}/generations/{n}` → `ImageGroupGenerationInfo`.
- Grants: `POST /image-groups/{id}/grants`, `GET …/grants`,
  `DELETE …/grants/{subject_id}/{permission}` — require `manage`/ownership.
- **No** group DELETE, **no** generation DELETE (immortal).

`register_image` / `list_images` / `get_image` are unchanged. Drop the
`oci_spec::image::ImageIndex` import and the `parse_group` call.

Register the new routes in `routes/mod.rs` (replacing the two existing
`/image-groups` entries).

---

## Phase 6 — Job enqueue and dispatch resolution

### 6.1 Enqueue (`switchboard/src/sql/job.rs::insert`, `routes/jobs.rs`)
Map `JobInitSpec` → columns:
- `Image { image }` → `image_id = image`. (FK validates existence at insert.)
- `ImageGroup { image_group, generation }`:
  - check caller holds `use` on `image_group` (else 403);
  - `generation = generation.or(latest_generation(group))`; if the group has no
    generation, **reject (400)** — nothing to freeze;
  - write `image_group_id = image_group`, `image_group_generation = <frozen int>`.
- `RestartJob` inheritance: copy predecessor's `image_id` /
  `image_group_id` / `image_group_generation` (the frozen generation rides
  along; restart dispatch still uses the pinned `resolved_image_digest`, per the
  existing restart path at `sql/job.rs` ~881).

Update the `insert` query, the `SqlJob` struct fields
(`image_digest`/`image_group_digest` → `image_id`/`image_group_id`/
`image_group_generation`), and the `select` lists at lines ~578 and ~732–773.

### 6.2 Dispatch resolution (`SqlJob::resolve_image_spec`)
- `image_id` branch: fetch the image row by id; `concrete_image_spec`.
- group branch: `members_for_generation(group_id, image_group_generation)`,
  build `GroupMember<(Uuid,String)>` in `index` order, `select_member` by host
  tags (specificity, ties by index), then `concrete_image_spec`.
- No admissible member → `NoMatchingMember` → existing `image_error` finalize.
- `concrete_image_spec`: ensure it returns an error (→ `image_error`) when the
  chosen image has **zero live locations** (GC'd). Add a
  `ImageResolveError::NoLocations` variant if not already covered.

### 6.3 Matcher (`switchboard/src/matcher.rs`)
Algorithm unchanged (specificity, earliest wins on ties). Only the data source
changes (generation-scoped, ordered by `index`). Update the module doc comment
to describe generations + `index` instead of "the index descriptor" /
`position`. Keep all `select_member` tests.

### 6.4 `JobImageRef` assembly (`sql/job.rs` ~683–696)
Build `Image { image_id }` / `ImageGroup { group_id, generation }` from the new
columns.

---

## Phase 7 — Authorization wiring

- Extend `auth/engine.rs` (or wherever host/job permission checks live) with the
  image-group check used by Phase 5/6: ownership-reach ∨ matching
  `image_group_grants` row, via `principals()`.
- Enqueue uses `use`; generation creation and grant management use `manage`
  (owner implicit).
- Dispatch does **not** re-check member-image ownership (group `use` is
  sufficient — the group owner curated the members).

---

## Phase 8 — Tests, fixtures, docs

- `switchboard/tests/image_routes.rs`: drop `image_index_bytes`; add coverage for
  create-group → create-generation (full replacement, generation increments) →
  enqueue-with-latest (freeze) → enqueue-with-explicit-generation → resolve to
  concrete member by host tags → dispatch failure when no admissible member and
  when the chosen image has no locations → `use`/`manage` permission enforcement.
- Matcher unit tests: keep; adjust any `position`→`index` naming.
- `FIXTURES.sql`: add a sample group + generation if fixtures reference images.
- Update `doc/oci-image-migration-plan.md` §5/§8.3 and
  `doc/images-oci-migration-plan.md` §I4–I6 to point at this document; correct
  the SCHEMA.sql IMAGE CATALOG header comments.
- Regenerate the OpenAPI spec under `switchboard/api-spec` if it is generated.

---

## 9. Suggested commit sequence (one reviewable step each)

1. Phase 1 (Nix + CI removal) — self-contained, no Rust impact.
2. Phase 2 (shared types + delete parse_group) — compiles against unchanged
   switchboard only after Phase 5/6; sequence 2 may need to land together with
   3–6 to keep the tree compiling. Land 2+3+4+5+6 as one schema/API change if a
   green build per commit is required; otherwise split with `#[allow(dead_code)]`
   shims.
3. Phase 3 (schema + migration).
4. Phase 4 (sql/image.rs).
5. Phase 5 (routes).
6. Phase 6 (enqueue + resolution + matcher).
7. Phase 7 (authz).
8. Phase 8 (tests + docs).

## 10. Open questions for you

1. **Group name scope:** globally unique, or unique per owner? (Default above:
   global.)
2. **Listing visibility:** should `use` grantees see a group in `GET
   /image-groups`, or only owners/`manage`? (Default: `use` can see + inspect.)
3. **Image reference by digest vs id at enqueue:** the plan switches
   `JobInitSpec::Image` to an image **id** (uniform with groups, FK-validated).
   Do you still want a raw-digest escape hatch (register-on-enqueue), or is
   "register via POST /images, then reference by id" acceptable as the only path?
4. **resolved pin shape:** RESOLVED — store `resolved_image_id uuid references
   images(id)` only (images are immortal/FK-enforced); recover the digest by
   join. Replaces the former `resolved_image_digest text`.
