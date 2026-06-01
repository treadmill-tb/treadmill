-- Add the audit log: append-only `audit_events` plus per-relation visibility
-- rows in `audit_event_relations`. See AUDIT_LOG_PLAN.md and SCHEMA.sql for
-- the full design. The append-only trigger at the bottom is hand-appended:
-- Atlas's community inspector does not manage Postgres functions or triggers
-- (see 20260531183310_add_oauth_login.sql), so it lives outside the diff and
-- is left untouched by future `atlas migrate diff` runs.

-- Create enum type "audit_entity_kind"
CREATE TYPE "tml_switchboard"."audit_entity_kind" AS ENUM ('job', 'host', 'subject');
-- Create enum type "audit_role"
CREATE TYPE "tml_switchboard"."audit_role" AS ENUM ('actor', 'subject', 'context');
-- Create "audit_events" table
CREATE TABLE "tml_switchboard"."audit_events" ("event_id" uuid NOT NULL, "event_type" text NOT NULL, "payload" jsonb NOT NULL, "actor_id" uuid NOT NULL, "correlation_id" uuid NULL, "created_at" timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY ("event_id"));
-- Create index "audit_events_created_at_idx" to table: "audit_events"
CREATE INDEX "audit_events_created_at_idx" ON "tml_switchboard"."audit_events" ("created_at");
-- Create "audit_event_relations" table
CREATE TABLE "tml_switchboard"."audit_event_relations" ("event_id" uuid NOT NULL, "entity_kind" "tml_switchboard"."audit_entity_kind" NOT NULL, "entity_id" uuid NOT NULL, "role" "tml_switchboard"."audit_role" NOT NULL, "view_policy" text NOT NULL, PRIMARY KEY ("event_id", "entity_kind", "entity_id", "role"), CONSTRAINT "audit_event_relations_event_id_fkey" FOREIGN KEY ("event_id") REFERENCES "tml_switchboard"."audit_events" ("event_id") ON UPDATE NO ACTION ON DELETE CASCADE);
-- Create index "audit_event_relations_entity_idx" to table: "audit_event_relations"
CREATE INDEX "audit_event_relations_entity_idx" ON "tml_switchboard"."audit_event_relations" ("entity_kind", "entity_id", "event_id" DESC);

-- Enforce append-only on `audit_events`: any direct UPDATE or DELETE raises.
-- Same trigger pattern as `deny_irrevocable_grant_change`. FK-cascade teardown
-- of `audit_event_relations` when an event is deleted is not gated by this
-- trigger, so a deliberate GC that drops the trigger to remove whole events
-- still cleans the relation rows up via the cascade.
CREATE OR REPLACE FUNCTION "tml_switchboard"."deny_audit_event_change"()
    RETURNS trigger
    LANGUAGE plpgsql AS
$$
begin
    raise exception 'audit events are append-only (% on %.%)',
        TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME;
end;
$$;

CREATE TRIGGER "audit_events_append_only"
    BEFORE DELETE OR UPDATE ON "tml_switchboard"."audit_events"
    FOR EACH ROW EXECUTE FUNCTION "tml_switchboard"."deny_audit_event_change"();
