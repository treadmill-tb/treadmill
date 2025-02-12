CREATE DOMAIN tml_switchboard.port AS integer CHECK (
    value >= 0
    AND value < 65535
);


CREATE DOMAIN tml_switchboard.ssh_host AS text CHECK (value IS NOT NULL);


CREATE DOMAIN tml_switchboard.ssh_port AS tml_switchboard.port CHECK (value IS NOT NULL);


CREATE TYPE tml_switchboard.ssh_endpoint AS (
    ssh_host tml_switchboard.ssh_host,
    ssh_port tml_switchboard.ssh_port
);


ALTER TABLE "tml_switchboard"."supervisors"
RENAME COLUMN "ssh_endpoints" TO "_ssh_endpoints_old";


ALTER TABLE "tml_switchboard"."supervisors"
ADD COLUMN "ssh_endpoints" tml_switchboard.ssh_endpoint[];


UPDATE "tml_switchboard"."supervisors" AS s
SET
    ssh_endpoints = (
        SELECT
            array_agg(ROW)
        FROM
            (
                SELECT
                    (
                        REGEXP_REPLACE(UNNEST, '(^.*):[0-9]+', '\1'),
                        SPLIT_PART(UNNEST, ':', -1)::int
                    )::tml_switchboard.ssh_endpoint
                FROM
                    unnest(s._ssh_endpoints_old)
            ) AS derivedTable
    );


ALTER TABLE "tml_switchboard"."supervisors"
ALTER COLUMN "ssh_endpoints"
SET NOT NULL;


ALTER TABLE "tml_switchboard"."supervisors"
DROP COLUMN "_ssh_endpoints_old";


ALTER TABLE "tml_switchboard"."jobs"
RENAME COLUMN "ssh_endpoints" TO "_ssh_endpoints_old";


ALTER TABLE "tml_switchboard"."jobs"
ADD COLUMN "ssh_endpoints" tml_switchboard.ssh_endpoint[];


UPDATE "tml_switchboard"."jobs" AS j
SET
    ssh_endpoints = (
        SELECT
            array_agg(ROW)
        FROM
            (
                SELECT
                    (
                        REGEXP_REPLACE(UNNEST, '(^.*):[0-9]+', '\1'),
                        SPLIT_PART(UNNEST, ':', -1)::int
                    )::tml_switchboard.ssh_endpoint
                FROM
                    unnest(j._ssh_endpoints_old)
            ) AS derivedTable
    );


ALTER TABLE "tml_switchboard"."jobs"
DROP COLUMN "_ssh_endpoints_old";
