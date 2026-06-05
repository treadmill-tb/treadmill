-- Rename the API-token cancellation concept to "revocation".
ALTER TYPE "tml_switchboard"."api_token_cancellation" RENAME ATTRIBUTE "canceled_at" TO "revoked_at";
ALTER TYPE "tml_switchboard"."api_token_cancellation" RENAME ATTRIBUTE "cancellation_reason" TO "revocation_reason";
ALTER TYPE "tml_switchboard"."api_token_cancellation" RENAME TO "api_token_revocation";
ALTER TABLE "tml_switchboard"."api_tokens" RENAME COLUMN "canceled" TO "revoked";
