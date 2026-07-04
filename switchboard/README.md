# Treadmill Switchboard

The switchboard is Treadmill's central coordinator: it schedules jobs onto
hosts, fronts the REST API, and manages users, groups, and authentication. See
the repository-level `AGENTS.md` for how to build and test it.

## Operator runbook: login admission & bootstrap

Interactive OAuth login is gated by an **admission allow-list**
(`tml_switchboard.login_allowlist`): a brand-new external identity may create
an account only if it is individually allow-listed or is a member of an
allow-listed org. Existing users are never gated. The table is hand-managed by
operators via SQL; the switchboard only reads it. After admission, a new user
must accept the Terms of Service before their account is created (and a bump
of `service.current_tos_version` in the switchboard config sends existing
users back through the same consent step on their next login).

All SQL below runs against the switchboard database (e.g. via `psql
"$DATABASE_URL"`).

### Allow-list an individual user

Entries reference the provider's *stable numeric id*, never the renameable
login handle. For GitHub, find it at `https://api.github.com/users/<login>`
(the `id` field):

```sql
INSERT INTO tml_switchboard.login_allowlist (provider, kind, external_id, comment)
VALUES ('github', 'user', '<github-numeric-user-id>', '<who/why>');
```

### Allow-list an org

Any *current* member of the org is admitted (membership is checked live at
login, so it also covers private memberships — the switchboard requests the
`read:org` scope for this). Find the org id at
`https://api.github.com/orgs/<org>`:

```sql
INSERT INTO tml_switchboard.login_allowlist (provider, kind, external_id, comment)
VALUES ('github', 'org', '<github-org-numeric-id>', '<note>');
```

Allow-listing an org governs **admission only**. If you also want its members
auto-added to a local group, that is the separate
`tml_switchboard.group_auto_sources` mechanism (one row binding the org to a
group; the reconciler then maintains `group_members` rows for it).

### Bootstrap the first admin

A fresh deployment has no users, and admin authority is membership in the
seeded `admins` group — so the first admin is created by hand:

1. Allow-list your own GitHub identity (first SQL statement above).

2. Log in through the switchboard's GitHub OAuth flow and accept the ToS. This
   provisions you as a regular user.

3. Find your new subject id:

   ```sql
   SELECT subject_id, username FROM tml_switchboard.users
   WHERE username = '<your-github-login>';
   ```

4. Add it to the admin group:

   ```sql
   INSERT INTO tml_switchboard.group_members (group_id, member_id, source)
   VALUES (
       (SELECT subject_id FROM tml_switchboard.groups WHERE name = 'admins'),
       '<subject_id>',
       'manual'
   );
   ```

Subsequent admins can be added the same way (steps 3–4), or through the API by
an existing admin.

### Denied logins

A denied registration leaves **no** user record; it is visible only as an
operator-only `registration_denied` audit event in
`tml_switchboard.audit_events` (carrying the provider, login handle, denial
reason, and client address).
