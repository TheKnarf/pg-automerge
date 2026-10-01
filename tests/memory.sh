#!/usr/bin/env bash
# automerge_memory_usage() in real sessions (run via `mise run memory`,
# also part of `mise run test`). See docs/DESIGN.md, "Memory
# observability". The pg_tests (src/tests/memory.rs) check the counters
# inside one call; this checks them across top-level statements and
# transactions, which is where a leak would build up in production:
#
#   - one session runs many rounds of everyday work (upserts with merge,
#     incremental changes, jsonb reads, heads, merge_agg, history rows,
#     a target-list set-returning function stopped by LIMIT and a cursor
#     left open until COMMIT, PL/pgSQL in-memory values, ROLLBACK) and of
#     errors in the middle of operations (bad input, missing
#     dependencies, an exception raised while an in-memory value is
#     alive, statement_timeout cancelling a merge_agg whose state holds a
#     document); after a warm-up, the bytes the library holds, its live
#     documents and the bytes counted at the end are the same as at the
#     start of the rounds (within a few kB), and the loads were counted;
#   - automerge_memory_reset() zeroes the loads and restarts the peak;
#   - every backend has its own counters: a new session starts with no
#     loads, and its reads do not change the first session's;
#   - pg_automerge.trim_threshold: after reading a document that takes
#     tens of MB, the backend's anonymous RSS is back near where it was
#     at the end of the transaction (malloc_trim), not inside it, and
#     stays up with -1 (logged).
#
# Env: see tests/lib.sh; MEMORY_ROUNDS (default 100).

# shellcheck source=tests/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DB=pg_automerge_memory
DROP_DBS=("$DB")
ROUNDS="${MEMORY_ROUNDS:-100}"
# Bytes the library may hold more after the rounds than before (lazily
# initialized state; measured: none).
SLACK=8192

on_exit() {
    if [[ $1 == 0 ]]; then log "all memory checks passed"; else echo "memory test FAILED" >&2; fi
}

log "generating fixture documents"
eval "$(cargo run -q -p pg_automerge_core --example gen_concurrency)"

install_extension
start_server
create_db

sql -v base="$BASE" -v a="$A" -v b="$B" -v inc_a="$INC_A" -v inc_b="$INC_B" -v many="$MANY" <<'SQL'
CREATE EXTENSION pg_automerge;
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
CREATE TABLE inputs (name text PRIMARY KEY, bytes bytea NOT NULL);
INSERT INTO inputs VALUES ('base', :'base'), ('a', :'a'), ('b', :'b'),
    ('inc_a', :'inc_a'), ('inc_b', :'inc_b'), ('many', :'many');
INSERT INTO docs SELECT g, (SELECT bytes FROM inputs WHERE name = 'many') FROM generate_series(1, 20) g;
SQL

# One round of work; every statement that fails is expected to (psql
# carries on: ON_ERROR_STOP is off for the rounds).
round() {
    cat <<'SQL'
INSERT INTO docs VALUES (100, (SELECT bytes FROM inputs WHERE name = 'base'))
    ON CONFLICT (id) DO UPDATE SET doc = merge(docs.doc, excluded.doc);
UPDATE docs SET doc = merge(doc, (SELECT bytes FROM inputs WHERE name = 'inc_a')) WHERE id = 100;
UPDATE docs SET doc = merge(doc, (SELECT bytes FROM inputs WHERE name = 'b')::automerge) WHERE id = 100;
SELECT doc->>'base', doc @> '{"a": true}' FROM docs WHERE id = 100 \g /dev/null
SELECT count(*) FROM docs WHERE doc ? 'many3' \g /dev/null
SELECT cardinality(automerge_heads(doc)) FROM docs \g /dev/null
SELECT merge_agg(doc)::jsonb FROM docs \g /dev/null
SELECT count(*) FROM docs, automerge_changes_meta(doc) \g /dev/null
SELECT automerge_changes(doc) FROM docs WHERE id = 1 LIMIT 2 \g /dev/null
BEGIN;
DECLARE c CURSOR FOR SELECT automerge_changes(doc) FROM docs WHERE id = 2;
FETCH 1 FROM c \g /dev/null
DELETE FROM docs WHERE id = 100;
COMMIT;
BEGIN;
UPDATE docs SET doc = merge(doc, (SELECT bytes FROM inputs WHERE name = 'inc_b')) WHERE id = 3;
ROLLBACK;
DO $$
DECLARE d automerge; ch bytea; j jsonb;
BEGIN
    SELECT doc INTO d FROM docs WHERE id = 4;
    SELECT bytes INTO ch FROM inputs WHERE name = 'inc_a';
    d := merge(d, ch);
    j := d::jsonb;
    SELECT bytes INTO ch FROM inputs WHERE name = 'inc_b';
    d := merge(d, ch);
END $$;
-- Errors part way.
SELECT '\x856f4a83ffffffff'::bytea::automerge;
SELECT merge(''::bytea::automerge, (SELECT bytes FROM inputs WHERE name = 'inc_a'));
DO $$
DECLARE d automerge; ch bytea;
BEGIN
    SELECT doc INTO d FROM docs WHERE id = 5;
    SELECT bytes INTO ch FROM inputs WHERE name = 'inc_a';
    d := merge(d, ch);
    RAISE EXCEPTION 'failing with an in-memory value alive';
END $$;
-- Row 1 is loaded into the state, row 2 (concurrent) merged into it,
-- and the rest are contained in it, until the timeout.
SET statement_timeout = '30ms';
SELECT merge_agg(CASE WHEN g = 1 THEN (SELECT doc FROM docs WHERE id = 1)
                      ELSE (SELECT bytes::automerge FROM inputs WHERE name = 'a') END)
FROM generate_series(1, 100000000) g \g /dev/null
RESET statement_timeout;
SQL
}

# `usage`: the session's counters as "allocated live loads" (psql
# variables u_allocated, u_live, u_loads).
USAGE="SELECT allocated_bytes AS u_allocated, live_documents AS u_live, loads AS u_loads FROM automerge_memory_usage() \\gset"

log "$ROUNDS rounds of work and errors in one session"
{
    echo '\set ON_ERROR_STOP 0'
    for _ in 1 2 3; do round; done # warm-up
    echo "$USAGE"
    echo '\set a0 :u_allocated'
    echo '\set d0 :u_live'
    echo 'SELECT automerge_memory_reset();'
    for _ in $(seq 1 "$ROUNDS"); do round; done
    echo "$USAGE"
    echo '\set ON_ERROR_STOP 1'
    # allocated before, after, live before, after, loads, peak above start,
    # and the SQLSTATEs seen are checked by the shell below.
    echo "SELECT :a0, :u_allocated, :d0, :u_live, :u_loads, peak_allocated_bytes - :a0 FROM automerge_memory_usage();"
} >"$WORK/rounds.sql"
PGOPTIONS="-c client_min_messages=warning" "$BINDIR/psql" -X -q -At "${CONN[@]}" -d "$DB" \
    -f "$WORK/rounds.sql" >"$WORK/rounds.out" 2>"$WORK/rounds.err" || fail "psql failed: $(tail -5 "$WORK/rounds.err")"
IFS='|' read -r a0 a1 d0 d1 loads peak < <(tail -1 "$WORK/rounds.out")
log "allocated $a0 -> $a1 bytes, live documents $d0 -> $d1, $loads loads, peak $peak bytes above the start"
# Every expected error happened in every round, and no other.
for err in 'invalid automerge document' 'invalid automerge changes: missing' \
    'failing with an in-memory value alive' 'canceling statement due to statement timeout'; do
    n="$(grep -c "$err" "$WORK/rounds.err" || true)"
    [[ "$n" == $((ROUNDS + 3)) ]] || fail "expected $((ROUNDS + 3)) errors '$err', got $n: $(grep ERROR "$WORK/rounds.err" | sort | uniq -c)"
done
others="$(grep ERROR "$WORK/rounds.err" | grep -cvE 'invalid automerge document|invalid automerge changes: missing|failing with an in-memory value alive|canceling statement due to statement timeout' || true)"
[[ "$others" == 0 ]] || fail "unexpected errors: $(grep ERROR "$WORK/rounds.err" | sort | uniq -c)"
[[ "$d0" == 0 && "$d1" == 0 ]] || fail "live documents between statements: $d0 before, $d1 after"
((a1 - a0 < SLACK)) || fail "the library holds $((a1 - a0)) bytes more after $ROUNDS rounds ($a0 -> $a1)"
((loads >= 10 * ROUNDS)) || fail "only $loads loads counted in $ROUNDS rounds"
((peak > 0)) || fail "no peak above the start: $peak"

log "reset, and one set of counters per backend"
# In one session: a read counts a load; the reset zeroes the loads and
# starts the peak over; another session's work does not show here.
out="$(sql <<'SQL'
SELECT doc::jsonb IS NOT NULL FROM docs WHERE id = 1;
SELECT loads >= 1, peak_allocated_bytes >= allocated_bytes FROM automerge_memory_usage();
SELECT automerge_memory_reset();
SELECT loads, load_time, peak_allocated_bytes - allocated_bytes < 8192 FROM automerge_memory_usage();
SQL
)"
[[ "$out" == $'t\nt|t\n\n0|0|t' ]] || fail "reset: $out"
out="$(sql -c "SELECT loads, live_documents FROM automerge_memory_usage()")"
[[ "$out" == "0|0" ]] || fail "a new session's counters: $out"

log "freed memory returned to the system at transaction end (pg_automerge.trim_threshold)"
# The backend's anonymous resident memory (its private heap), read by the
# backend itself (pg_read_file: a superuser), in MB.
RSS="(SELECT (regexp_match(pg_read_file('/proc/self/status'), 'RssAnon:\s+(\d+)'))[1]::bigint / 1024)"
# (A heredoc, not -v: the literal exceeds the kernel's limit on one argument.)
sql <<SQL
CREATE TABLE big AS SELECT '$BIG'::bytea::automerge AS doc;
SQL
# Each read in its own transaction; the peak shows how much it took.
READ_BIG="SELECT length(doc::jsonb->>'text') FROM big \g /dev/null"
out="$(sql <<SQL
SET pg_automerge.trim_threshold = -1;
SELECT $RSS AS r0 \gset
$READ_BIG
$READ_BIG
SELECT $RSS AS r1, peak_allocated_bytes / 1048576 AS peak FROM automerge_memory_usage() \gset
SET pg_automerge.trim_threshold = '16MB';
$READ_BIG
SELECT $RSS AS r2 \gset
BEGIN;
$READ_BIG
SELECT $RSS AS r3 \gset
COMMIT;
SELECT $RSS AS r4 \gset
RESET pg_automerge.trim_threshold;
$READ_BIG
SELECT :r0, :r1, :r2, :r3, :r4, $RSS, :peak;
SQL
)"
IFS='|' read -r r0 r1 r2 r3 r4 r5 peak <<<"$out"
log "RssAnon (MB): $r0 at the start; $r1 after two reads with -1 (each took up to $peak MB); $r2 after one with 16MB; $r3 before and $r4 after the COMMIT of another; $r5 after one with the default (64MB)"
((peak >= 24)) || fail "the big document's read took only $peak MB"
# Without trimming, malloc keeps the freed document for reuse (measured:
# about as much as the read took); with it, the end of each transaction
# hands it back, and inside one nothing is trimmed.
((r2 - r0 <= 8)) || fail "not trimmed at the end of the transaction: RssAnon $r0 -> $r2 MB"
((r3 - r0 >= 16)) || fail "trimmed inside a transaction (or nothing kept): RssAnon $r0 -> $r3 MB"
((r4 - r0 <= 8)) || fail "not trimmed at COMMIT: RssAnon $r0 -> $r4 MB"
