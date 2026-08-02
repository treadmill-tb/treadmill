-- Modify "jobs" table
ALTER TABLE "tml_switchboard"."jobs"
ADD COLUMN "job_ip_address" inet NULL;


-- Create "job_services" table
CREATE TABLE "tml_switchboard"."job_services" (
    "job_id" uuid NOT NULL,
    "name" text NOT NULL,
    "label" text NULL,
    "protocol" text NOT NULL,
    PRIMARY KEY ("job_id", "name"),
    CONSTRAINT "job_services_job_id_fkey" FOREIGN KEY ("job_id") REFERENCES "tml_switchboard"."jobs" ("job_id") ON UPDATE NO ACTION ON DELETE CASCADE,
    CONSTRAINT "valid_service_name" CHECK (
        char_length(name) BETWEEN 1 AND 16
        AND name ~ '^[a-z][a-z0-9]*$'
    )
);


-- Change notifications for "job_services", keyed on the owning job; see the
-- CHANGE NOTIFICATIONS section of SCHEMA.sql for the payload contract.
CREATE TRIGGER job_services_notify_write
AFTER insert
OR delete ON tml_switchboard.job_services FOR each ROW
EXECUTE function tml_switchboard.notify_change ('job_id');


CREATE TRIGGER job_services_notify_update
AFTER
UPDATE ON tml_switchboard.job_services FOR each ROW WHEN (OLD.* IS DISTINCT FROM NEW.*)
EXECUTE function tml_switchboard.notify_change ('job_id');
