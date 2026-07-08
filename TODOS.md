# TODOS

Deferred work and known shortcuts, captured so they aren't lost. Not a roadmap —
just things we consciously punted on.

## Re-Sync Verified/Non-Verified Emails on Login

Currently, the auth code does not properly re-sync emails on login. Previously
unverified emails that are now verified upstream aren't marked as such, emails
aren't removed, and now-unverified emails aren't transitioned in the switchboard
DB.

## Separate `manage` Permission from `own` Permission

Currently, `manage` is the same as ownership: it allows to change ownership
arbitrary. There should be a more granular permission that allows to change
attributes of the resource (such as a host's label or tag set), without being
able to change the ACLs or ownership attribute.

## User-Provided Job Label

Jobs should carry an optional, user-provided `label` (naming it `label` to match
images/image-groups, not `name`). ASCII-printable, max 256 chars, non-unique,
mutable after enqueue. Needs the DB column + CHECK, Rust validation, the
enqueue/patch API surface, and the console (job list + detail + new-job form).

## Retain Job `started_at` and Host After Finalization

`started_at` and `dispatched_on_host_id` are erased when a job finalizes: the
`started_at_iso_executing` and `dispatched_host_iso_assigned` CHECK constraints
force them NULL outside the executing states, and the worker's finalize queries
(`sql/job.rs`) null them explicitly. Consequence: the console's "Started" and
"Host" fields go blank the moment a job terminates. Fix: relax both CHECKs to
"monotonic" (set-once, retained through `finalized`; NULL only if the job never
reached that stage, e.g. queue-timeout) and stop nulling them on finalize.
"Currently bound" is then derived from `job_state` / `hosts.current_job`, not
from `dispatched_on_host_id`. Console already reads both fields; no frontend
change needed beyond confirming the display.

## Drop `users.username`

Users should have only a display name (`full_name`) and their subject ID — no
separate `username` handle. Remove the column, its UNIQUE constraint,
`unique_username`/reserved-name logic, and the username field from
`PATCH /users/me`. `user_identities.provider_login` remains display-only. Group
`name` uniqueness is unaffected.

## Primary Email; GitHub Registration Sets It

Add a notion of a user's primary email (e.g. `is_primary` on `user_emails`, or a
`primary_email` FK on `users`). GitHub registration should set it from the
verified primary email returned by the provider. Coordinate with the existing
"Re-Sync Verified/Non-Verified Emails on Login" item above.

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
