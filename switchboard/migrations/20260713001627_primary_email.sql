-- Modify "user_emails" table
ALTER TABLE "tml_switchboard"."user_emails"
DROP CONSTRAINT "user_emails_pkey",
ADD CONSTRAINT "primary_email_verified" CHECK (
    (provider <> ''::text)
    OR verified
),
ADD PRIMARY KEY ("email", "provider");


-- Create index "user_emails_one_primary" to table: "user_emails"
CREATE UNIQUE INDEX "user_emails_one_primary" ON "tml_switchboard"."user_emails" ("user_id")
WHERE
    (provider = ''::text);


-- Enforce that a verified address belongs to at most one user (trigger/function
-- not tracked by the schema diff, so authored by hand).
CREATE OR REPLACE FUNCTION tml_switchboard.user_emails_verified_single_owner () returns trigger language plpgsql AS $$
begin
    if not NEW.verified then
        return NEW;
    end if;

    perform pg_advisory_xact_lock(hashtext('tml_switchboard.user_emails'));

    if exists (
        select 1
        from tml_switchboard.user_emails
        where email = NEW.email and verified and user_id <> NEW.user_id
    ) then
        raise exception
            'verified email % already belongs to another user', NEW.email;
    end if;

    return NEW;
end;
$$;


CREATE CONSTRAINT TRIGGER user_emails_verified_owner
AFTER insert
OR
UPDATE ON tml_switchboard.user_emails FOR each ROW
EXECUTE function tml_switchboard.user_emails_verified_single_owner ();
