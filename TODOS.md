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
