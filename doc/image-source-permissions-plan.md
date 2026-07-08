# Image / Source Permissions Lift — Plan

Status: proposed (not yet implemented). Author: workshopped with the maintainer,
2026-07-08.

This plan covers two interlocking changes to the switchboard authorization model
and the console:

- **The `everyone` subject** — replace per-entity `public` boolean columns with a
  single well-known "public" subject that can be *granted* permissions, uniformly
  across every resource kind (issue #4).
- **The image / source restructure** — stop treating an image as an ownable
  thing; make the *source* (registry location, eventually credentialed) the
  ownable, grantable, deletable entity, and derive image-group usability from it
  (issue #7).

Everything else from the original issue batch (job label, retaining
`started_at`/host past finalization, dropping `users.username`, primary email,
private-source credentials) is **out of scope here** and recorded in
`TODOS.md`.

## 1. Model

### 1.1 Images are not ownable

An image is a manifest — a content hash. It carries no owner and no ACL. It keeps
a surrogate `uuid` id (future-proofing for a hypothetical non-OCI image format)
and cached, manifest-derived metadata (`label`, `attrs`, `artifact_type`) that is
**not user-writeable** — it is a projection of the validated manifest.
`manifest_digest` stays **globally `UNIQUE`**: the same bytes are the same row for
everyone. (This retires the earlier per-owner-uniqueness / deep-clone idea — with
no ownership on images there is nothing to clone.)

### 1.2 Sources are the ownable, grantable, deletable entity

A *source* is a place an image's bytes can be pulled from (`registry`,
`repository`), possibly — later — with credentials. What today is
`image_locations` becomes `image_sources`:

- A public, unauthenticated source is usable by everyone and is
  owned/managed by whoever registered it (add/delete/manage rights).
- A private source (deferred, see `TODOS.md`) would additionally carry
  credentials and restrict *use* to grantees.

Sources are **always deletable** by their owner (+ admins), even when referenced
by an image-group generation. This is the deliberate decoupling: images and
generations are immortal; the bytes behind them are not guaranteed to stay
reachable.

### 1.3 Image groups pin images; usability is derived

A group generation pins **images** (by id), never sources. Whether a subject `S`
can actually *use* a group generation `g` is:

```
usable(S, group, g) := can_access_image_group(S, group, 'use')     -- group ACL
                       AND for every member image m of g:
                           exists a source of m that S may `use`     -- source ACL
```

A group grant is therefore **necessary but not sufficient**: `S` may hold `use`
on the group yet be unable to run it because a member image has no source `S` can
reach. This is intentional (a source can go down or get too expensive to host);
the console must surface it per generation rather than treat it as a bug.

## 2. Decisions locked (from design review)

1. **Enqueue source check = all members.** At enqueue, every member of the frozen
   generation must have a source the **job owner** may `use`. (A "partially
   inhabited" generation implicitly constrains the eligible-host set and is
   confusing; we can relax to "≥1 member" later behind an option.)
2. **Source gate keys on the job owner**, not the enqueuing user. The enqueuer can
   only name owners it is a member of, and inherits their permissions, so this can
   only *narrow* the eligible image set, never escalate. Consistent with
   `eligible_hosts`, which already evaluates over the owner's principals.
3. **Full source ACL now**: `image_sources` get `owner_subject` + `use`/`manage`
   grants + the `everyone` subject. Every source is public this session (no
   credentials), so `use` grants are mostly inert, but the structure lands now to
   avoid churn when credentials arrive.
4. **Public is revocable.** No irreversibility. With images non-owned and sources
   always deletable, nothing's privacy hinges on a group staying public; "public"
   is a normal grant to the `everyone` subject, addable/removable with `manage`.
5. **"Public" is expressed uniformly as a grant.** No dedicated public endpoint;
   publishing = grant the `everyone` subject `use`. The console renders a "Public"
   toggle that maps to adding/removing that grant.
6. **Any authenticated user** may register an image (create the digest row if new)
   and add a source, owning the source they add. Sources are
   deletable/manageable by owner + admins.
7. **Usability is computed lazily per generation, for all subjects** (not just the
   viewer), and exposed via the API — serving both "is this group broken?" (owner
   view) and "can I use this group?" (grantee view).
8. **Private registries fully deferred.** `image_sources` carries a brief SCHEMA
   note that a location may later carry credentials (stored in an external system
   or encrypted); not a concern now.

## 3. Commit plan

Two independently-compiling, independently-checking commits. A is a prerequisite
for B (B's public-source usability relies on the `everyone` grant).

### Commit A — the `everyone` subject / public refactor (issue #4)

Self-contained: seeds the well-known subject, folds it into `principals()`, and
rewrites "public" as a grant everywhere it appears.

- **`switchboard/SCHEMA.sql`**
  - Seed a well-known `everyone` subject: `subjects` row of kind `group`, fixed
    UUID `00000000-0000-0000-0000-000000000004`, plus a `groups` row named
    `everyone`. Document it next to `admins`.
  - Change `tml_switchboard.principals(uuid)` to **always union the `everyone`
    id** into its result, so a grant to `everyone` on any resource is visible to
    every subject. (This is the single mechanism that makes "public" work across
    hosts/jobs/image-groups/sources uniformly.)
  - Drop `image_groups.public` and update the table comment.
- **Migration** (`nix run '.#switchboard-migrate' -c everyone_subject`): seed the
  subject/group; for every existing `image_groups` row with `public = true`,
  insert `image_group_grants(group_id, EVERYONE, 'use')`; drop the column. Then
  `-v` to verify migrations reproduce SCHEMA; the `switchboard-migrations-
  consistency` check enforces this. `principals()` is a function change → carried
  in the migration by hand (Atlas community won't diff function bodies reliably;
  see AGENTS.md §2).
- **`switchboard/src/auth/engine.rs`**
  - Add `EVERYONE_SUBJECT_ID: Uuid = Uuid::from_u128(4)`.
  - Remove the `$3 = 'use' and public` branch from `can_access_image_group` and
    the `public` union from `image_group_permissions` — both are now subsumed by
    the ordinary grant check, because `principals()` includes `everyone`.
- **`switchboard/src/sql/image.rs`**: drop `public` from `GroupRecord`,
  `create_group`, all `select`s; delete `set_group_public`; `list_owned_groups`
  loses the `g.public OR` disjunction (public groups now reach the caller via the
  `everyone` grant through `principals()` — verify listing still shows them, and
  extend the query to include groups reachable by a grant if it doesn't already).
- **`switchboard/src/routes/images.rs`**: delete `set_image_group_public`; drop
  `public` from `group_info`, `create_image_group`. "Make public" is now just a
  `POST /image-groups/{id}/grants` with `subject_id = EVERYONE`.
- **`switchboard/src/routes/mod.rs`**: remove the `PUT /image-groups/{id}/public`
  route.
- **Audit**: retire the `ImageGroupPublicSet` event (or keep the type but stop
  emitting) — grant/revoke of the `everyone` subject already produces
  `ImageGroupGrantCreated`/`Revoked`, which is the more honest record.
- **`treadmill-rs/src/api/switchboard/images.rs`**: remove `public` from
  `ImageGroupInfo` and `CreateImageGroupRequest`; delete `SetImageGroupPublicRequest`.
- **OpenAPI**: regenerate (`UPDATE_SCHEMA=1 cargo test -p treadmill-switchboard
  --test openapi_spec`).
- **`.sqlx`**: regenerate (`nix run '.#switchboard-sqlx-prepare'`).
- **Console** (`console/`):
  - `app/routes/image-group-detail.tsx`: replace the `setPublic` mutation +
    "Make public/private" button with a "Public" toggle that grants/revokes the
    `everyone` subject `use`; render the "public" badge off the presence of that
    grant rather than a `public` field.
  - `app/routes/image-groups.tsx`: drop the `public` checkbox from the create
    form and the `public` badge column (or derive it from grants).
  - Expose the `everyone` subject id to the console (a small constant, matching
    the seeded UUID) so the toggle can target it.
  - Regenerate `app/api/schema.d.ts` (`npm run codegen`).

### Commit B — the image / source restructure (issue #7)

- **`switchboard/SCHEMA.sql`**
  - `images`: drop `owner_subject`. Keep `id, manifest_digest (UNIQUE),
    artifact_type, label, attrs, created_at`. Reword the comment: images are
    non-owned manifest identities; `label`/`attrs` are cached manifest
    projections, not user metadata.
  - `image_locations` → `image_sources`: add surrogate `id uuid PRIMARY KEY`;
    add `owner_subject uuid REFERENCES subjects ON DELETE SET NULL`; keep
    `image_id, registry, repository, status, added_at`; keep a `UNIQUE(image_id,
    registry, repository)` to preserve dedup. Add the deferred-credentials note.
  - `image_source_permission` enum (`use`, `manage`) + `image_source_grants`
    table (mirror `image_group_grants`; no irrevocable trigger).
  - A usability helper, e.g. `image_reachable_source(p_subject, p_image)` →
    `bool` (exists a source of the image the subject may `use`, evaluated over
    `principals()`; with `everyone` folded in, a public source satisfies
    everyone). Compose it for a generation check
    `generation_usable(p_subject, p_group, p_gen)`.
- **Migration** (`... -c image_sources`): create `image_sources` from
  `image_locations` (mint ids; set `owner_subject` from the old
  `images.owner_subject`); grant `everyone` `use` on every migrated source so
  pre-existing behavior (any image usable by anyone) is preserved; create the
  grants table/enum; drop `images.owner_subject` and the old `image_locations`.
- **`switchboard/src/auth/engine.rs`**: add `ImageSourcePermission` +
  `can_access_image_source` / `image_source_permissions`, mirroring the group
  helpers.
- **`switchboard/src/sql/image.rs`**:
  - `ImageRecord` loses `owner_subject`. Registration owns the *source*, not the
    image.
  - Source CRUD: create source (id + owner), delete source, list sources for an
    image, fetch source; source grant CRUD + list.
  - Image/group listing reworked from "owned" to "reachable via a usable source"
    (and/or via group membership) — `list_owned` / `list_owned_groups` semantics
    revisited.
  - Generation-usability query returning, per member image, whether it has (a) a
    source the viewer may use and (b) a public source — enough for both API use
    cases.
- **`switchboard/src/routes/images.rs`**:
  - `register_image`: drop the digest-owner conflict check (no image owner);
    create/own a source. Re-registration adds a source owned by the caller.
  - `image_info` / `ImageInfo`: drop `owner_id`; return sources with their owner
    + the viewer's permissions.
  - New source routes: add/delete a source, grant/revoke source `use`/`manage`,
    list source grants. Wire in `routes/mod.rs`.
  - `generation_info` / group detail: include per-member usability + an
    aggregate "broken for public" / "usable by you" signal.
  - Visibility checks (`visible_group`, `get_image`) move off image ownership.
- **`switchboard/src/routes/jobs.rs`** (enqueue): after freezing the generation,
  assert every member has a source the **job owner** may `use` (decision #1/#2);
  reject otherwise. Apply the same check to a concrete-image job (closing the
  currently-unchecked `routes/jobs.rs:137` gap).
- **Scheduler / dispatch** (`scheduler.rs` / `sql/job.rs` resolve): at dispatch,
  after the matcher resolves the concrete member for the chosen host, re-check
  that the resolved image has a source the owner may `use` (sources can be
  deleted between enqueue and dispatch). On failure, finalize with `image_error`
  (or a new `image_source_unavailable` termination reason — TBD in B).
- **`treadmill-rs/src/api/switchboard/images.rs`**: `ImageInfo` loses `owner_id`;
  `ImageLocation` → source view with `id`, `owner_id`, `permissions`; add source
  grant request/response types; add per-generation usability types.
- **OpenAPI** + **`.sqlx`** regenerated as above.
- **Console**:
  - `image-detail.tsx`, `images.tsx`: drop image `owner_id`; show sources with
    owners; add source add/delete + grant UI (own detail section).
  - `image-group-detail.tsx` / `generation-detail.tsx`: per-member "no source you
    can use" / "no public source" indicators; group-level "this generation is not
    usable by all its grantees" banner for owners.
  - Regenerate `schema.d.ts`.

## 4. Out of scope / deferred

- Private-source **credentials** (encryption at rest, supervisor hand-off) and
  source-level restriction of *use* to grantees — `TODOS.md`. The usability
  computation and enqueue/schedule checks are written against "a source the owner
  may use", so they tighten to the credentialed world with no structural change.
- Job `label`; retaining `started_at`/`dispatched_on_host_id` past finalization;
  dropping `users.username`; primary email — all `TODOS.md`.
- An `image` / `image_source` **audit entity kind** for source grant/credential
  changes (parity with `image_group`) — optional; add in B only if cheap.

## 5. Risks / things to watch

- **`principals()` is hot.** Folding `everyone` into it touches every ownership
  and grant query (incl. `eligible_hosts`). Confirm no query regresses:
  ownership joins compare against `owner_id`, which is never `everyone`, so
  ownership semantics are unchanged; only grant checks gain the public grant.
  Verify the route + DB test suites (`user_routes.rs`, `#[sqlx::test]`) stay
  green.
- **Listing semantics.** After dropping the `public` column, "list groups I can
  see" must include groups I reach via the `everyone` grant; make sure the
  listing query joins grants (through `principals()`), not just ownership.
- **Enqueue vs. per-host resolution.** Decision #1 checks *all* members at
  enqueue even though only one is resolved per host — accepted as the
  conservative, predictable choice; revisit behind an option if it proves
  annoying.
- **Migration/`principals()` function body** isn't diffed by Atlas community;
  hand-carry it and run `-v` + `-r` so `atlas.sum` stays consistent
  (`flake check` gate).
