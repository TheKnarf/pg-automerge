#!/usr/bin/env bash
# Installed as /docker-entrypoint-initdb.d/10-pg-automerge.sh. The official
# entrypoint runs it once, when it initializes an empty data directory, as
# the postgres OS user against the temporary socket-only server; it never
# runs again on an existing volume.
#
# PG_AUTOMERGE_CREATE_EXTENSION
#   1 (default)  CREATE EXTENSION IF NOT EXISTS pg_automerge in $POSTGRES_DB
#   0            skip: install it yourself (e.g. into a schema of its own,
#                from a later init script or a migration)
#
# template1 is deliberately left alone: the extension is not trusted (only
# a superuser may install it), and a copy in template1 would put it in
# every database a CREATEDB role creates later.
set -euo pipefail

want="${PG_AUTOMERGE_CREATE_EXTENSION:-1}"
case "$want" in
  0) echo "pg_automerge: PG_AUTOMERGE_CREATE_EXTENSION=0, not creating the extension"; exit 0 ;;
  1) ;;
  *) echo "pg_automerge: PG_AUTOMERGE_CREATE_EXTENSION must be 0 or 1, got '$want'" >&2; exit 1 ;;
esac

db="${POSTGRES_DB:-${POSTGRES_USER:-postgres}}"
echo "pg_automerge: CREATE EXTENSION pg_automerge in database $db"
# The same connection the entrypoint's own docker_process_sql uses.
psql -v ON_ERROR_STOP=1 --no-psqlrc --no-password \
  --username "${POSTGRES_USER:-postgres}" --dbname "$db" \
  -c 'CREATE EXTENSION IF NOT EXISTS pg_automerge' \
  -Atc "SELECT 'pg_automerge ' || extversion || ' installed' FROM pg_extension WHERE extname = 'pg_automerge'"
