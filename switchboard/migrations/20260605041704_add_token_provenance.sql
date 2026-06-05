-- Modify "api_tokens" table
ALTER TABLE "tml_switchboard"."api_tokens" ADD COLUMN "user_agent" text NULL, ADD COLUMN "comment" text NULL, ADD COLUMN "created_ip" text NULL, ADD COLUMN "created_port" integer NULL;
