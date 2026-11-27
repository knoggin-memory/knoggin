#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Runs once, when the database volume is first made (the Postgres image runs every
# script in /docker-entrypoint-initdb.d). Makes the three logins of tab "Deployment" from
# the three URL files, so that no password is typed twice:
#   postgres_url            the router: owns the database and its tables
#   postgres_url_lifecycle  the lifecycle task: SELECT and DELETE only
#   postgres_url_backup     the nightly dump: SELECT only
set -euo pipefail

# postgres://<user>:<password>@db:5432/knoggin?sslmode=disable
user_of() { sed -E 's#^[a-z]+://([^:]+):.*$#\1#' "$1"; }
password_of() { sed -E 's#^[a-z]+://[^:]+:([^@]+)@.*$#\1#' "$1"; }

sql() {
    psql -v ON_ERROR_STOP=1 --no-psqlrc --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" "$@"
}

owner=$(user_of /run/secrets/postgres_url)
lifecycle=$(user_of /run/secrets/postgres_url_lifecycle)
backup=$(user_of /run/secrets/postgres_url_backup)

sql -v owner="$owner" -v owner_pw="$(password_of /run/secrets/postgres_url)" \
    -v lifecycle="$lifecycle" -v lifecycle_pw="$(password_of /run/secrets/postgres_url_lifecycle)" \
    -v backup="$backup" -v backup_pw="$(password_of /run/secrets/postgres_url_backup)" \
    -v db="$POSTGRES_DB" <<'SQL'
CREATE ROLE :"owner" LOGIN PASSWORD :'owner_pw';
CREATE ROLE :"lifecycle" LOGIN PASSWORD :'lifecycle_pw';
CREATE ROLE :"backup" LOGIN PASSWORD :'backup_pw';
ALTER DATABASE :"db" OWNER TO :"owner";
REVOKE ALL ON DATABASE :"db" FROM PUBLIC;
GRANT CONNECT ON DATABASE :"db" TO :"lifecycle", :"backup";
GRANT USAGE ON SCHEMA public TO :"lifecycle", :"backup";
-- The router makes the tables (its migration runs at start), so the grants go to
-- the tables it will make.
ALTER DEFAULT PRIVILEGES FOR ROLE :"owner" IN SCHEMA public GRANT SELECT, DELETE ON TABLES TO :"lifecycle";
ALTER DEFAULT PRIVILEGES FOR ROLE :"owner" IN SCHEMA public GRANT SELECT ON TABLES TO :"backup";
ALTER DEFAULT PRIVILEGES FOR ROLE :"owner" IN SCHEMA public GRANT SELECT ON SEQUENCES TO :"backup";
SQL
