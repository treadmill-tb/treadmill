-- Modify "oauth_flows" table
ALTER TABLE "tml_switchboard"."oauth_flows"
ADD COLUMN "return_to" text NULL;


-- Modify "staged_logins" table
ALTER TABLE "tml_switchboard"."staged_logins"
ADD COLUMN "return_to" text NULL,
ADD COLUMN "user_agent" text NULL,
ADD COLUMN "created_ip" text NULL,
ADD COLUMN "created_port" integer NULL;
