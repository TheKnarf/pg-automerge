#!/usr/bin/env bash
# Benchmark of the workloads where expanded (in-memory) automerge values
# matter: PL/pgSQL loops merging change sets into a variable, repeated reads
# of a variable, nested merges, merge(...)::jsonb and merge_agg(...)::jsonb,
# plus the single-UPDATE path that must not get slower. Run via
# `mise run bench-expanded`; prints average milliseconds per case.
#
# Installs a release build into the pgrx-managed pg18 and uses database
# pg_automerge_bench. Not part of `mise run test` (it takes minutes).
#
# Env: PGRX_PG_PORT (default 28818), PG_CONFIG (default: pgrx's pg18),
# BENCH_DOCS (default "text3mb items20k items2k"), BENCH_REPS (default 3),
# BENCH_CASES (a grep -E pattern selecting cases, default all),
# BENCH_REUSE=1 (keep the database and fixtures of a previous run).

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

PG_CONFIG="${PG_CONFIG:-$(sed -n 's/^pg18 *= *"\(.*\)"/\1/p' "${PGRX_HOME:-$HOME/.pgrx}/config.toml")}"
[[ -x "$PG_CONFIG" ]] || { echo "pg_config for pg18 not found (run mise run pgrx-init)" >&2; exit 1; }
BINDIR="$("$PG_CONFIG" --bindir)"
PORT="${PGRX_PG_PORT:-28818}"
DB=pg_automerge_bench
DOCS="${BENCH_DOCS:-text3mb items20k items2k}"
REPS="${BENCH_REPS:-3}"
CASE_FILTER="${BENCH_CASES:-.}"
WORK="$(mktemp -d)"
STARTED_SERVER=0

log() { printf '==> %s\n' "$*" >&2; }
sql_on() {
    local db="$1"; shift
    PGOPTIONS="-c client_min_messages=warning" "$BINDIR/psql" -X -q -At -v ON_ERROR_STOP=1 -h localhost -p "$PORT" -d "$db" "$@"
}
sql() { sql_on "$DB" "$@"; }

cleanup() {
    local status=$?
    if [[ $STARTED_SERVER == 1 ]]; then
        log "stopping Postgres"
        cargo pgrx stop pg18 >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
    exit "$status"
}
trap cleanup EXIT

log "generating fixtures"
cargo run --release -q -p pg_automerge_core --example gen_bench >"$WORK/fixtures.sql"

log "installing extension (release build)"
cargo pgrx install --release --pg-config "$PG_CONFIG" >"$WORK/install.log" 2>&1 \
    || { cat "$WORK/install.log" >&2; exit 1; }

if ! "$BINDIR/pg_isready" -q -h localhost -p "$PORT"; then
    log "starting Postgres on port $PORT"
    cargo pgrx start pg18 >/dev/null
    STARTED_SERVER=1
fi

if [[ "${BENCH_REUSE:-0}" == 1 ]] && [[ "$(sql_on postgres -c "SELECT count(*) FROM pg_database WHERE datname = '$DB'")" == 1 ]]; then
    log "reusing database $DB"
else
    sql_on postgres -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" -c "CREATE DATABASE $DB"
    sql -c "CREATE EXTENSION pg_automerge"
    log "loading fixtures"
    sql -f "$WORK/fixtures.sql" >/dev/null
    sql -c "CREATE TABLE bench_copy AS SELECT * FROM bench_doc"
fi

sql <<'SQL'
-- Merge the first n change sets into a local variable, one by one.
CREATE OR REPLACE FUNCTION loop_merge(p text, n int) RETURNS text[] LANGUAGE plpgsql AS $$
DECLARE d automerge; ch bytea;
BEGIN
    SELECT doc INTO d FROM bench_doc WHERE name = p;
    FOR ch IN SELECT c FROM bench_changes WHERE name = p AND i <= n ORDER BY i LOOP
        d := merge(d, ch);
    END LOOP;
    RETURN automerge_heads(d);
END $$;

-- The same, but each merge in a block with an EXCEPTION clause: the
-- variable is not local to that block, so PL/pgSQL may only hand merge a
-- read/write pointer through merge's support function.
CREATE OR REPLACE FUNCTION loop_merge_guarded(p text, n int) RETURNS text[] LANGUAGE plpgsql AS $$
DECLARE d automerge; ch bytea;
BEGIN
    SELECT doc INTO d FROM bench_doc WHERE name = p;
    FOR ch IN SELECT c FROM bench_changes WHERE name = p AND i <= n ORDER BY i LOOP
        BEGIN
            d := merge(d, ch);
        EXCEPTION WHEN invalid_text_representation THEN
            RAISE NOTICE 'skipped bad changes';
        END;
    END LOOP;
    RETURN automerge_heads(d);
END $$;

-- Loop, then store the result (flattening it once).
CREATE OR REPLACE FUNCTION loop_merge_store(p text, n int) RETURNS void LANGUAGE plpgsql AS $$
DECLARE d automerge; ch bytea;
BEGIN
    SELECT doc INTO d FROM bench_doc WHERE name = p;
    FOR ch IN SELECT c FROM bench_changes WHERE name = p AND i <= n ORDER BY i LOOP
        d := merge(d, ch);
    END LOOP;
    UPDATE bench_copy SET doc = d WHERE name = p;
END $$;

-- Two merges into a variable, then ten reads of it (a jsonb conversion
-- each: from memory when the variable holds an expanded value).
CREATE OR REPLACE FUNCTION merge_then_read(p text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE d automerge; c1 bytea; c2 bytea; s text;
BEGIN
    SELECT doc INTO d FROM bench_doc WHERE name = p;
    SELECT c INTO c1 FROM bench_changes WHERE name = p AND i = 1;
    SELECT c INTO c2 FROM bench_changes WHERE name = p AND i = 2;
    d := merge(d, c1);
    d := merge(d, c2);
    FOR k IN 1..10 LOOP
        s := d->>'status';
    END LOOP;
    RETURN s;
END $$;

-- Average milliseconds of running `stmt` reps times; each run is rolled
-- back (a subtransaction aborted by a deliberate error).
CREATE OR REPLACE FUNCTION bench(stmt text, reps int) RETURNS numeric LANGUAGE plpgsql AS $$
DECLARE t0 timestamptz; total interval := '0';
BEGIN
    FOR r IN 1..reps LOOP
        t0 := clock_timestamp();
        BEGIN
            EXECUTE stmt;
            total := total + (clock_timestamp() - t0);
            RAISE EXCEPTION USING ERRCODE = 'P0099';
        EXCEPTION WHEN SQLSTATE 'P0099' THEN NULL;
        END;
    END LOOP;
    RETURN round((extract(epoch FROM total) * 1000 / reps)::numeric, 1);
END $$;
SQL

C1="(SELECT c FROM bench_changes WHERE name = '@DOC@' AND i = 1)"
C2="(SELECT c FROM bench_changes WHERE name = '@DOC@' AND i = 2)"
C3="(SELECT c FROM bench_changes WHERE name = '@DOC@' AND i = 3)"
DOC="(SELECT doc FROM bench_doc WHERE name = '@DOC@')"
CASES=(
    "UPDATE bench_copy SET doc = merge(doc, $C1) WHERE name = '@DOC@'"
    "SELECT automerge_heads(merge($DOC, $C1))"
    "SELECT merge($DOC, $C1)::jsonb->>'status'"
    "SELECT automerge_heads(merge(merge(merge($DOC, $C1), $C2), $C3))"
    "SELECT loop_merge('@DOC@', 1)"
    "SELECT loop_merge('@DOC@', 20)"
    "SELECT loop_merge_guarded('@DOC@', 20)"
    "SELECT loop_merge_store('@DOC@', 20)"
    "SELECT merge_then_read('@DOC@')"
    "SELECT merge_agg(doc)::jsonb->>'status' FROM (SELECT doc FROM bench_doc WHERE name = '@DOC@' UNION ALL SELECT doc FROM bench_forks WHERE name = '@DOC@') s"
)

printf '%-10s | %10s | %s\n' doc ms case
for doc in $DOCS; do
    for c in "${CASES[@]}"; do
        grep -qE "$CASE_FILTER" <<<"$c" || continue
        stmt="${c//@DOC@/$doc}"
        ms="$(sql -v stmt="$stmt" -v reps="$REPS" <<<"SELECT bench(:'stmt', :reps)")"
        label="$(sed -E -e "s/\(SELECT c FROM bench_changes WHERE name = '[^']*' AND i = ([0-9]+)\)/c\1/g" \
                        -e "s/\(SELECT doc FROM bench_doc WHERE name = '[^']*'\)/doc/g" <<<"$stmt")"
        printf '%-10s | %10s | %s\n' "$doc" "$ms" "$label"
    done
done
