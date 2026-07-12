# TODOS

Deferred work and known shortcuts, captured so they aren't lost. Not a roadmap —
just things we consciously punted on.

## Image-Source Audit Surface

There is no audit surface for image
sources: no `image_source` value in `audit_entity_kind`, and no
`ImageSourceAdded` / `ImageSourceRemoved` / `ImageSourceGrant{Created,Revoked}`
events. Source add/delete and
grant/revoke therefore produce no audit rows; `ImageRegistered` still fires for a
new digest. When added, mirror the `image_set` grant events (`view(Manage)` on
the source) and add the entity-kind enum value in the same migration.

## Separate `manage` Permission from `own` Permission

Currently, `manage` is the same as ownership: it allows to change ownership
arbitrary. There should be a more granular permission that allows to change
attributes of the resource (such as a host's label or tag set), without being
able to change the ACLs or ownership attribute.

## Drop `users.username`

Users should have only a display name (`full_name`) and their subject ID — no
separate `username` handle. Remove the column, its UNIQUE constraint,
`unique_username`/reserved-name logic, and the username field from
`PATCH /users/me`. `user_identities.provider_login` remains display-only. Group
`name` uniqueness is unaffected.

## Primary Email; GitHub Registration Sets It

Add a notion of a user's primary email (e.g. `is_primary` on `user_emails`, or a
`primary_email` FK on `users`). GitHub registration should set it from the
verified primary email returned by the provider. Coordinate with the login-time
email re-sync.

## Private Image Sources: Credentials + Source-Level Use Grants

The image-source permission lift ships owned-but-universally-public sources (any
authenticated user may register/own a source; a source is deletable/manageable
by its owner + admins; every source is usable by everyone). The deferred half:
private sources backed by an OCI registry that requires credentials. That needs
(a) a credentials slot on the source, protected at rest (envelope encryption
with a KEK from config/env, or an external secret store — never plaintext in the
DB), handed to the supervisor only at dispatch; and (b) a source-level `use`
grant surface (owner + grants + the `everyone` subject) so usability becomes
"the job owner can *use* some source" rather than "some source exists".

## Future-Proofing Route Filters

Currently, routes such as `/images` implicitly filter their output to only
include resources that the user owns (transitively) or that they can use.
However, this means that weakening these filters in the future can have
undesired effects: for instance, the frotend may suddenly present images as
owned or accessible that actually arent. So, we should have mandatory filters on
these endpoints. For instance, this could mean a mandatory "owned" or "usable"
filter (owned implies usable), and an owner filter that can take an ID (for a
group that the user is a member of), or the special values "self" and
"self-groups" (for transitive group reachability). These types of filters should
be standardized across most resource endpoints, except for ones where extending
it beyond some natural ownership definition is unlikely (e.g., audit events
should only ever be requestable for one self, or by admins).
