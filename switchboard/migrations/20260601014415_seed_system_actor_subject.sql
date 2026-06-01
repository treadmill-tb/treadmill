-- Seed the well-known system actor subject.
--
-- Hand-written: Atlas's community inspector does not manage seed data (see
-- 20260531183310_add_oauth_login.sql), so this INSERT is appended by hand and
-- left untouched by future `atlas migrate diff` runs, mirroring the seeded
-- `admins` group. The `system` subject has a hard-coded well-known UUID so the
-- switchboard can attribute the audit events it raises on its own initiative
-- (worker-driven job/host transitions, internal automation) to a stable actor.
-- It is a bare `subjects` row: it never logs in and owns nothing.
INSERT INTO "tml_switchboard"."subjects" ("subject_id", "kind")
VALUES ('00000000-0000-0000-0000-000000000002', 'system');
