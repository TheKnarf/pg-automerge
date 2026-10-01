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
# are rejected without touching the row. Scenarios 7 and 8 check the
# "UPDATE .. WHERE NOT automerge_contains(doc, $1)" pattern: after waiting
# for the lock, B's WHERE clause is re-evaluated against A's committed row
# (EvalPlanQual), so B skips the row if A already wrote the same changes
# and merges into A's version otherwise.
#
# Also checks: that the harness detects a lost update (plain overwrite
# control), the INSERT .. ON CONFLICT DO UPDATE upsert for existing and new
# rows, and REPEATABLE READ raising a serialization failure. (Dump and
# restore: tests/dump.sh.)
#
# Exits non-zero on the first failure. Stops the server on exit if this script
# started it, and drops its scratch databases.
#
# Env: see tests/lib.sh.

# shellcheck source=tests/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DB=pg_automerge_concurrency
DROP_DBS=("$DB")
BG_PIDS=()

on_exit() {
    for pid in "${BG_PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    if [[ $1 == 0 ]]; then log "all concurrency checks passed"; else echo "concurrency test FAILED" >&2; fi
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

log "generating fixture documents"
eval "$(cargo run -q -p pg_automerge_core --example gen_concurrency)"

install_extension
start_server
create_db
sql -v base="$BASE" <<'SQL'
CREATE EXTENSION pg_automerge;
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
CREATE TABLE release (x int);
-- Rows 1..8 start from the shared base; row 4 (new-row upsert) does not exist.
INSERT INTO docs SELECT i, :'base'::bytea FROM generate_series(1, 8) i WHERE i <> 4;
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
#    with a serialization error (docs/src/pages/design/index.mdx); retrying it then merges.
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

# 7. Update only when not contained, both sessions sending the same
#    changes: B's WHERE is rechecked against A's committed version, which
#    already contains them, so B updates no row (and writes no row version).
CONTAINS_UPDATE="WITH u AS (UPDATE docs SET doc = merge(doc, :'%s'::bytea)
                  WHERE id = %s AND NOT automerge_contains(doc, :'%s'::bytea) RETURNING 1)
                  SELECT 'updated ' || count(*) FROM u"
# shellcheck disable=SC2059
A_DOC="$INC_A" B_DOC="$INC_A" run_pair "UPDATE ... WHERE NOT automerge_contains(doc, \$1) (same changes)" \
    "$(printf "$CONTAINS_UPDATE" a 7 a)" "$(printf "$CONTAINS_UPDATE" b 7 b)"
[[ $B_STATUS == 0 ]] || fail "session B: $B_OUT"
[[ "$B_OUT" == "updated 0" ]] || fail "session B should skip the row, got: $B_OUT"
assert_row 7 "doc->>'inc_a'" true
assert_row 7 "cardinality(automerge_heads(doc))" 1

# 8. The same with different changes: the recheck finds B's changes missing
#    from A's version, so B merges them into it.
# shellcheck disable=SC2059
A_DOC="$INC_A" B_DOC="$INC_B" run_pair "UPDATE ... WHERE NOT automerge_contains(doc, \$1) (different changes)" \
    "$(printf "$CONTAINS_UPDATE" a 8 a)" "$(printf "$CONTAINS_UPDATE" b 8 b)"
[[ $B_STATUS == 0 ]] || fail "session B: $B_OUT"
[[ "$B_OUT" == "updated 1" ]] || fail "session B should update the row, got: $B_OUT"
assert_both 8 inc_a inc_b
