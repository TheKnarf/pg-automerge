#!/usr/bin/env bash
# Real two-session concurrency test for `merge` (run via `mise run concurrency`).
#
# Installs the extension into the pgrx-managed Postgres 18, starts it, and
# for each scenario runs two psql sessions against one row:
#
#   session A: BEGIN; <write with merge>; <wait until released>; COMMIT;
#   session B: <write with merge>            -- must block on A's row lock
#
# The controller only releases A once pg_stat_activity shows B waiting on a
# lock, so the interleaving is deterministic rather than timing-based. Every
# document written is a full save of a fork of one shared base document with
# its own edit; the final jsonb must contain both sessions' edits.
#
# Scenario 6 persists only incremental changes (bare change chunks) with
# merge(doc, $1::bytea), and checks that changes with missing dependencies
# are rejected without touching the row.
#
# Also checks: that the harness detects a lost update (plain overwrite
# control), the INSERT .. ON CONFLICT DO UPDATE upsert for existing and new
# rows, REPEATABLE READ raising a serialization failure, and a pg_dump ->
# psql restore round trip preserving bytes and heads.
#
# Exits non-zero on the first failure. Stops the server on exit if this script
# started it, and drops its scratch databases.
#
# Env: PGRX_PG_PORT (default 28818), PG_CONFIG (default: pgrx's pg18).

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

PG_CONFIG="${PG_CONFIG:-$(sed -n 's/^pg18 *= *"\(.*\)"/\1/p' "${PGRX_HOME:-$HOME/.pgrx}/config.toml")}"
[[ -x "$PG_CONFIG" ]] || { echo "pg_config for pg18 not found (run mise run pgrx-init)" >&2; exit 1; }
BINDIR="$("$PG_CONFIG" --bindir)"
PORT="${PGRX_PG_PORT:-28818}"
DB=pg_automerge_concurrency
DB_RESTORE=pg_automerge_concurrency_restore
WORK="$(mktemp -d)"
STARTED_SERVER=0
BG_PIDS=()

log() { printf '==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# psql against a database; extra args are passed through.
sql_on() {
    local db="$1"; shift
    PGOPTIONS="-c client_min_messages=warning" "$BINDIR/psql" -X -q -At -v ON_ERROR_STOP=1 -h localhost -p "$PORT" -d "$db" "$@"
}
sql() { sql_on "$DB" "$@"; }

cleanup() {
    local status=$?
    for pid in "${BG_PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    if [[ $STARTED_SERVER == 1 ]] || "$BINDIR/pg_isready" -q -h localhost -p "$PORT"; then
        sql_on postgres -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" >/dev/null 2>&1 || true
        sql_on postgres -c "DROP DATABASE IF EXISTS $DB_RESTORE WITH (FORCE)" >/dev/null 2>&1 || true
    fi
    if [[ $STARTED_SERVER == 1 ]]; then
        log "stopping Postgres"
        cargo pgrx stop pg18 >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
    if [[ $status == 0 ]]; then log "all concurrency checks passed"; else echo "concurrency test FAILED" >&2; fi
    exit "$status"
}
trap cleanup EXIT

# Poll until `query` returns t (10s timeout).
wait_for() {
    local what="$1" query="$2"
    for _ in $(seq 1 500); do
        [[ "$(sql -c "$query")" == t ]] && return 0
        sleep 0.02
    done
    fail "timed out waiting for: $what"
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

log "generating fixture documents"
eval "$(cargo run -q -p pg_automerge_core --example gen_concurrency)"

log "installing extension"
cargo pgrx install --pg-config "$PG_CONFIG" >"$WORK/install.log" 2>&1 \
    || { cat "$WORK/install.log" >&2; fail "cargo pgrx install"; }

if ! "$BINDIR/pg_isready" -q -h localhost -p "$PORT"; then
    log "starting Postgres on port $PORT"
    cargo pgrx start pg18 >/dev/null
    STARTED_SERVER=1
fi

sql_on postgres -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" -c "CREATE DATABASE $DB"
sql -v base="$BASE" <<'SQL'
CREATE EXTENSION pg_automerge;
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
CREATE TABLE release (x int);
-- Rows 1..6 start from the shared base; row 4 (new-row upsert) does not exist.
INSERT INTO docs SELECT i, :'base'::bytea FROM generate_series(1, 6) i WHERE i <> 4;
SQL

# ---------------------------------------------------------------------------
# Two-session driver
# ---------------------------------------------------------------------------

# run_pair NAME A_SQL B_SQL: A runs A_SQL in a transaction and holds it open;
# B runs B_SQL (its own statements, may include BEGIN/COMMIT) once A holds the
# lock. Psql variables :'a' and :'b' are the two sessions' documents, set
# with A_DOC/B_DOC. B's exit status and output land in $B_STATUS/$B_OUT.
run_pair() {
    local name="$1" a_sql="$2" b_sql="$3"
    log "$name"
    sql -c "TRUNCATE release"

    PGAPPNAME=conc_a sql -v a="$A_DOC" >"$WORK/a.out" 2>&1 <<SQL &
BEGIN;
$a_sql;
-- Hold the transaction (and A's row lock) open until the controller says so.
-- Each loop iteration takes a fresh READ COMMITTED snapshot.
DO \$\$ BEGIN
    WHILE NOT EXISTS (SELECT 1 FROM release) LOOP PERFORM pg_sleep(0.01); END LOOP;
END \$\$;
COMMIT;
SQL
    local pid_a=$!
    BG_PIDS+=("$pid_a")
    wait_for "session A to hold its lock" \
        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                        WHERE application_name = 'conc_a' AND state = 'active'
                          AND query LIKE '%WHILE NOT EXISTS%')"

    PGAPPNAME=conc_b sql -v b="$B_DOC" >"$WORK/b.out" 2>&1 <<SQL &
$b_sql;
SQL
    local pid_b=$!
    BG_PIDS+=("$pid_b")
    wait_for "session B to block on session A" \
        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                        WHERE application_name = 'conc_b' AND wait_event_type = 'Lock')"

    sql -c "INSERT INTO release VALUES (1)"
    wait "$pid_a" || { cat "$WORK/a.out" >&2; fail "$name: session A failed"; }
    B_STATUS=0
    wait "$pid_b" || B_STATUS=$?
    B_OUT="$(cat "$WORK/b.out")"
    BG_PIDS=()
}

# assert_row ID QUERY_ON_DOC EXPECTED: evaluate an expression over row ID's doc.
assert_row() {
    local id="$1" expr="$2" expected="$3" got
    got="$(sql -c "SELECT $expr FROM docs WHERE id = $id")"
    [[ "$got" == "$expected" ]] || fail "row $id: $expr = '$got', expected '$expected'"
}

# Both sessions' edits (and the base) are visible and the history has two heads.
assert_both() {
    local id="$1" ka="$2" kb="$3"
    assert_row "$id" "doc->>'base'" true
    assert_row "$id" "doc->>'$ka'" true
    assert_row "$id" "doc->>'$kb'" true
    assert_row "$id" "doc->'items' @> '[\"from base\", \"from $ka\", \"from $kb\"]'" t
    assert_row "$id" "jsonb_array_length(doc->'items')" 3
    assert_row "$id" "cardinality(automerge_heads(doc))" 2
}

# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

# 1. UPDATE .. SET doc = merge(doc, $new): B blocks, then re-evaluates merge()
#    against A's committed row version (EvalPlanQual).
A_DOC="$A" B_DOC="$B" run_pair "UPDATE ... SET doc = merge(doc, ...)" \
    "UPDATE docs SET doc = merge(doc, :'a'::bytea::automerge) WHERE id = 1" \
    "UPDATE docs SET doc = merge(doc, :'b'::bytea::automerge) WHERE id = 1"
[[ $B_STATUS == 0 ]] || fail "session B: $B_OUT"
assert_both 1 a b

# 2. Control: a plain overwrite in the same interleaving loses A's edit. This
#    proves the harness can see a lost update at all.
A_DOC="$A" B_DOC="$B" run_pair "control: plain overwrite loses an update" \
    "UPDATE docs SET doc = merge(doc, :'a'::bytea::automerge) WHERE id = 2" \
    "UPDATE docs SET doc = :'b'::bytea WHERE id = 2"
[[ $B_STATUS == 0 ]] || fail "session B: $B_OUT"
assert_row 2 "doc ? 'a'" f
assert_row 2 "doc->>'b'" true

# 3. Upsert onto an existing row: B's ON CONFLICT DO UPDATE waits for A's row
#    lock, then merges into A's version.
UPSERT="INSERT INTO docs VALUES (3, :'%s'::bytea) ON CONFLICT (id) DO UPDATE SET doc = merge(docs.doc, EXCLUDED.doc)"
# shellcheck disable=SC2059
A_DOC="$UPSERT_A" B_DOC="$UPSERT_B" run_pair "INSERT ... ON CONFLICT DO UPDATE (existing row)" \
    "$(printf "$UPSERT" a)" "$(printf "$UPSERT" b)"
[[ $B_STATUS == 0 ]] || fail "session B: $B_OUT"
assert_both 3 upsert_a upsert_b

# 4. Upsert of a row that does not exist yet: both sessions insert; B waits on
#    A's uncommitted insert, then takes the conflict path and merges.
# shellcheck disable=SC2059
A_DOC="$NEW_A" B_DOC="$NEW_B" run_pair "INSERT ... ON CONFLICT DO UPDATE (new row)" \
    "$(printf "${UPSERT/(3,/(4,}" a)" "$(printf "${UPSERT/(3,/(4,}" b)"
[[ $B_STATUS == 0 ]] || fail "session B: $B_OUT"
assert_both 4 new_a new_b

# 5. REPEATABLE READ: B cannot re-evaluate against a newer version and fails
#    with a serialization error (DESIGN.md); retrying it then merges.
A_DOC="$RR_A" B_DOC="$RR_B" run_pair "REPEATABLE READ raises a serialization failure" \
    "UPDATE docs SET doc = merge(doc, :'a'::bytea::automerge) WHERE id = 5" \
    "BEGIN ISOLATION LEVEL REPEATABLE READ;
     UPDATE docs SET doc = merge(doc, :'b'::bytea::automerge) WHERE id = 5;
     COMMIT"
[[ $B_STATUS != 0 ]] || fail "REPEATABLE READ session B unexpectedly succeeded"
grep -q "could not serialize access due to concurrent update" <<<"$B_OUT" \
    || fail "unexpected REPEATABLE READ error: $B_OUT"
assert_row 5 "doc->>'rr_a'" true
assert_row 5 "doc ? 'rr_b'" f
# (psql only interpolates :'b' in script input, not in -c.)
sql -v b="$RR_B" <<'SQL'
BEGIN ISOLATION LEVEL REPEATABLE READ;
UPDATE docs SET doc = merge(doc, :'b'::bytea::automerge) WHERE id = 5;
COMMIT;
SQL
assert_both 5 rr_a rr_b

# 6. Incremental persistence: each session sends only its own changes (bare
#    change chunks that depend on the base) as a typed bytea, merged with the
#    merge(automerge, bytea) overload. B blocks, then applies its chunk on
#    top of A's committed version.
A_DOC="$INC_A" B_DOC="$INC_B" run_pair "UPDATE ... SET doc = merge(doc, \$1::bytea) (incremental changes only)" \
    "UPDATE docs SET doc = merge(doc, :'a'::bytea) WHERE id = 6" \
    "UPDATE docs SET doc = merge(doc, :'b'::bytea) WHERE id = 6"
[[ $B_STATUS == 0 ]] || fail "session B: $B_OUT"
assert_both 6 inc_a inc_b
# The same chunks again are a no-op, byte for byte.
before="$(sql -c "SELECT md5(doc::bytea) FROM docs WHERE id = 6")"
sql -v a="$INC_A" -v b="$INC_B" <<'SQL'
UPDATE docs SET doc = merge(doc, :'a'::bytea) WHERE id = 6;
UPDATE docs SET doc = doc || :'b'::bytea WHERE id = 6;
SQL
assert_row 6 "md5(doc::bytea)" "$before"
# Changes whose base the row lacks are rejected (22P02) and the row is kept.
# Row 4 was created from NEW_A/NEW_B, not from the base.
sql -c "UPDATE docs SET doc = ''::bytea WHERE id = 4"
if out="$(sql -v a="$INC_A" 2>&1 <<'SQL'
UPDATE docs SET doc = merge(doc, :'a'::bytea) WHERE id = 4;
SQL
)"; then fail "orphaned incremental changes were accepted"; fi
grep -q "missing 1 dependency" <<<"$out" || fail "unexpected error for orphaned changes: $out"
assert_row 4 "doc::jsonb" "{}"

# ---------------------------------------------------------------------------
# pg_dump round trip
# ---------------------------------------------------------------------------

log "pg_dump -> psql restore round trip"
sql_on postgres -c "DROP DATABASE IF EXISTS $DB_RESTORE WITH (FORCE)" -c "CREATE DATABASE $DB_RESTORE"
"$BINDIR/pg_dump" -h localhost -p "$PORT" -d "$DB" >"$WORK/dump.sql"
grep -q "CREATE EXTENSION IF NOT EXISTS pg_automerge" "$WORK/dump.sql" || fail "dump lacks CREATE EXTENSION"
sql_on "$DB_RESTORE" -f "$WORK/dump.sql" >/dev/null
fingerprint="SELECT string_agg(id || ':' || md5(doc::bytea) || ':' || automerge_heads(doc)::text
                               || ':' || md5(doc::jsonb::text), ',' ORDER BY id) FROM docs"
before="$(sql -c "$fingerprint")"
after="$(sql_on "$DB_RESTORE" -c "$fingerprint")"
[[ -n "$before" && "$before" == "$after" ]] || fail "restore differs: $before <> $after"
