# Shared helpers of the multi-session test scripts (concurrency.sh,
# notify.sh, dump.sh, extension.sh, limits.sh, bench_*.sh, ...). Sourced,
# not run.
#
# Sets ROOT_DIR (and cd's there), PG_CONFIG, BINDIR (the client tools:
# psql, pg_dump, pg_restore), HOST, PORT, CONN (the psql/pg_dump
# connection arguments), EXTERNAL and WORK (a scratch directory), and an
# EXIT trap that
#   1. calls the script's on_exit function, if it defines one, with the
#      exit status,
#   2. drops the databases listed in DROP_DBS if a server is running,
#   3. stops the server if start_server started it (never one that was
#      already running),
#   4. removes WORK.
# The script sets DB (the database `sql` talks to) and DROP_DBS.
#
# Two modes:
#   - pgrx (default): the pgrx-managed Postgres 18 on localhost:PGRX_PG_PORT
#     (default 28818); install_extension runs `cargo pgrx install`,
#     start_server starts it if needed (and the trap stops it again).
#   - external: PG_AUTOMERGE_TEST_HOST is set. The server is someone
#     else's (e.g. the Docker image, see tests/docker.sh) and must already
#     run with the extension available: install_extension does nothing,
#     start_server only checks that it is up. PG_AUTOMERGE_TEST_PORT
#     (default 5432), PG_AUTOMERGE_TEST_USER (default postgres, must be a
#     superuser), PG_AUTOMERGE_TEST_PASSWORD (optional). The scripts create
#     and drop their own databases (and extension.sh a role) on it, so use
#     a throwaway server.
# The client tools are PG_CONFIG's bindir in both modes: by default pgrx's
# pg18 build; any Postgres 18 client works in external mode (CI's docker job
# uses PGDG's postgresql-client-18). Same major version as the server, as
# pg_dump requires.
#
# Env: PG_CONFIG (default: pgrx's pg18), PG_AUTOMERGE_INSTALLED=1 (skip the
# dev-build install: `mise run test` installs once for all scripts).

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

PG_CONFIG="${PG_CONFIG:-$(sed -n 's/^pg18 *= *"\(.*\)"/\1/p' "${PGRX_HOME:-$HOME/.pgrx}/config.toml")}"
[[ -x "$PG_CONFIG" ]] || { echo "pg_config for pg18 not found (run mise run pgrx-init)" >&2; exit 1; }
BINDIR="$("$PG_CONFIG" --bindir)"
if [[ -n "${PG_AUTOMERGE_TEST_HOST:-}" ]]; then
    EXTERNAL=1
    HOST="$PG_AUTOMERGE_TEST_HOST"
    PORT="${PG_AUTOMERGE_TEST_PORT:-5432}"
    CONN=(-h "$HOST" -p "$PORT" -U "${PG_AUTOMERGE_TEST_USER:-postgres}")
    # libpq reads it; a script that connects as another role sets its own.
    if [[ -n "${PG_AUTOMERGE_TEST_PASSWORD:-}" ]]; then export PGPASSWORD="$PG_AUTOMERGE_TEST_PASSWORD"; fi
else
    EXTERNAL=0
    HOST=localhost
    PORT="${PGRX_PG_PORT:-28818}"
    CONN=(-h "$HOST" -p "$PORT")
fi
WORK="$(mktemp -d)"
STARTED_SERVER=0
DB=
DROP_DBS=()

log() { printf '==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# psql against a database; extra args are passed through.
sql_on() {
    local db="$1"; shift
    PGOPTIONS="-c client_min_messages=warning" "$BINDIR/psql" -X -q -At -v ON_ERROR_STOP=1 "${CONN[@]}" -d "$db" "$@"
}
sql() { sql_on "$DB" "$@"; }

server_running() { "$BINDIR/pg_isready" -q "${CONN[@]}"; }

# Poll until `query` returns t (10s timeout).
wait_for() {
    local what="$1" query="$2"
    for _ in $(seq 1 500); do
        [[ "$(sql -c "$query")" == t ]] && return 0
        sleep 0.02
    done
    fail "timed out waiting for: $what"
}

# install_extension [extra `cargo pgrx install` args, e.g. --release].
# A plain (dev) install is skipped when PG_AUTOMERGE_INSTALLED=1, and every
# install against an external server (which has its own build).
install_extension() {
    if [[ $EXTERNAL == 1 ]]; then
        log "external server $HOST:$PORT: using its installed extension"
        return
    fi
    if [[ $# == 0 && "${PG_AUTOMERGE_INSTALLED:-0}" == 1 ]]; then
        log "extension already installed"
        return
    fi
    log "installing extension${1:+ ($*)}"
    cargo pgrx install --pg-config "$PG_CONFIG" "$@" >"$WORK/install.log" 2>&1 \
        || { cat "$WORK/install.log" >&2; fail "cargo pgrx install"; }
}

# Start the pgrx-managed Postgres unless one is already running on PORT;
# an external server must already be up.
start_server() {
    if [[ $EXTERNAL == 1 ]]; then
        server_running || fail "external server $HOST:$PORT is not accepting connections"
        return
    fi
    if ! server_running; then
        log "starting Postgres on port $PORT"
        cargo pgrx start pg18 >/dev/null
        STARTED_SERVER=1
    fi
}

# For scripts that need the pgrx install (extension files) or run a
# scratch cluster of their own: refuse external mode instead of silently
# testing something else.
require_pgrx_mode() {
    [[ $EXTERNAL == 0 ]] || fail "$(basename "$0") needs the pgrx-managed Postgres; unset PG_AUTOMERGE_TEST_HOST"
}

# Drop and create DB.
create_db() {
    sql_on postgres -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" -c "CREATE DATABASE $DB"
}

cleanup() {
    local status=$?
    if declare -F on_exit >/dev/null; then on_exit "$status" || true; fi
    if ((${#DROP_DBS[@]})) && server_running; then
        for db in "${DROP_DBS[@]}"; do
            sql_on postgres -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" >/dev/null 2>&1 || true
        done
    fi
    if [[ $STARTED_SERVER == 1 ]]; then
        log "stopping Postgres"
        cargo pgrx stop pg18 >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
    exit "$status"
}
trap cleanup EXIT
