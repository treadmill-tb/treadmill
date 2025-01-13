ALTER TABLE "tml_switchboard"."supervisors"
ADD COLUMN "ssh_endpoints" TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[];


ALTER TABLE "tml_switchboard"."supervisors"
ALTER COLUMN "ssh_endpoints"
DROP DEFAULT;


ALTER TABLE "tml_switchboard"."jobs"
ADD COLUMN "ssh_endpoints" TEXT[];
