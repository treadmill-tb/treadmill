-- Modify "api_tokens" table
ALTER TABLE "tml_switchboard"."api_tokens"
DROP CONSTRAINT "job_tokens_never_expire",
ADD CONSTRAINT "job_tokens_never_expire" CHECK (
    (
        subject_kind <> 'job'::tml_switchboard.subject_kind
    )
    OR (expires_at IS NULL)
);


-- Modify "users" table
ALTER TABLE "tml_switchboard"."users"
ADD COLUMN "bot" boolean NOT NULL DEFAULT FALSE;


-- Atlas (community) does not migrate functions/triggers; added by hand.
-- Only a bot user may hold a non-expiring user token.
CREATE OR REPLACE FUNCTION tml_switchboard.api_tokens_non_expiring_bot () returns trigger language plpgsql AS $$
begin
    if NEW.subject_kind = 'user' and NEW.expires_at is null and not exists (
        select 1
        from tml_switchboard.users
        where subject_id = NEW.subject_id and bot
    ) then
        raise exception
            'only a bot user may hold a non-expiring token (subject %)', NEW.subject_id;
    end if;

    return NEW;
end;
$$;


CREATE CONSTRAINT TRIGGER api_tokens_non_expiring_bot
AFTER insert
OR
UPDATE ON tml_switchboard.api_tokens FOR each ROW
EXECUTE function tml_switchboard.api_tokens_non_expiring_bot ();
