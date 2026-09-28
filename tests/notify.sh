#!/usr/bin/env bash
# End-to-end test of the automerge_notify() trigger (run via `mise run
# notify`, also part of `mise run test`).
#
# pg_tests run inside one transaction that is rolled back, so NOTIFY never
# delivers there. This script uses a real listener: a psql session that ran
# LISTEN and reads further commands from a FIFO, so the controller can make
# it print the notifications it has received ("Asynchronous notification
# ... with payload ...") whenever it wants. Payloads are then checked with
# SQL (parsed as jsonb) against the table.
#
# "No notification" is checked with a marker: after the write under test,
# the controller sends pg_notify(channel, 'marker-N'). Notifications are
# delivered in commit order, so once the marker has arrived, anything the
# write sent would have arrived before it.
#
# Checks: INSERT/UPDATE/DELETE payloads (key, heads, prev_heads); a listener
# fetching exactly the new changes with automerge_changes_bytes(doc,
# prev heads); no notification for no-op merges, updates of other columns,
# the "WHERE NOT automerge_contains" pattern and rolled-back transactions;
# identical events in one transaction (INSERT/DELETE/INSERT of the same
# row, a key flipping back and forth) all arriving, distinct by seq;
# a 150-head document degrading to "truncated" without failing the write.
#
# Env: PGRX_PG_PORT (default 28818), PG_CONFIG (default: pgrx's pg18).

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

PG_CONFIG="${PG_CONFIG:-$(sed -n 's/^pg18 *= *"\(.*\)"/\1/p' "${PGRX_HOME:-$HOME/.pgrx}/config.toml")}"
[[ -x "$PG_CONFIG" ]] || { echo "pg_config for pg18 not found (run mise run pgrx-init)" >&2; exit 1; }
BINDIR="$("$PG_CONFIG" --bindir)"
PORT="${PGRX_PG_PORT:-28818}"
DB=pg_automerge_notify
CHANNEL=docs_changed
WORK="$(mktemp -d)"
STARTED_SERVER=0
LISTENER_PID=

log() { printf '==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

sql_on() {
    local db="$1"; shift
    PGOPTIONS="-c client_min_messages=warning" "$BINDIR/psql" -X -q -At -v ON_ERROR_STOP=1 -h localhost -p "$PORT" -d "$db" "$@"
}
sql() { sql_on "$DB" "$@"; }

cleanup() {
    local status=$?
    exec 3>&- 2>/dev/null || true
    [[ -n "$LISTENER_PID" ]] && kill "$LISTENER_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    if [[ $STARTED_SERVER == 1 ]] || "$BINDIR/pg_isready" -q -h localhost -p "$PORT"; then
        sql_on postgres -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" >/dev/null 2>&1 || true
    fi
    if [[ $STARTED_SERVER == 1 ]]; then
        log "stopping Postgres"
        cargo pgrx stop pg18 >/dev/null 2>&1 || true
    fi
    if [[ $status != 0 && -f "$WORK/listener.out" ]]; then
        echo "--- listener output:" >&2
        cat "$WORK/listener.out" >&2
    fi
    rm -rf "$WORK"
    if [[ $status == 0 ]]; then log "all notify checks passed"; else echo "notify test FAILED" >&2; fi
    exit "$status"
}
trap cleanup EXIT

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
sql <<SQL
CREATE EXTENSION pg_automerge;
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL, title text);
CREATE TRIGGER docs_notify AFTER INSERT OR UPDATE OR DELETE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('$CHANNEL', 'id');
SQL

# The listener: psql reading commands from a FIFO kept open on fd 3.
mkfifo "$WORK/listener.in"
PGAPPNAME=notify_listener "$BINDIR/psql" -X -At -h localhost -p "$PORT" -d "$DB" \
    <"$WORK/listener.in" >"$WORK/listener.out" 2>&1 &
LISTENER_PID=$!
exec 3>"$WORK/listener.in"
echo "LISTEN $CHANNEL;" >&3
wait_for "the listener to LISTEN" \
    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                    WHERE application_name = 'notify_listener' AND state = 'idle'
                      AND query LIKE 'LISTEN%')"

SEEN=0      # notifications already consumed from listener.out
MARKER=0

# Payloads of all notifications received so far (one per line).
payloads() {
    sed -n "s/^Asynchronous notification \"$CHANNEL\" with payload \"\(.*\)\" received from server process with PID [0-9]*\.\$/\1/p" \
        "$WORK/listener.out"
}

# next_payloads: send a marker notification, poke the listener until the
# marker has arrived, and set PAYLOADS to the array of payloads received
# before it (since the previous call).
next_payloads() {
    MARKER=$((MARKER + 1))
    sql -c "SELECT pg_notify('$CHANNEL', 'marker-$MARKER')" >/dev/null
    local all=()
    for _ in $(seq 1 500); do
        echo "SELECT 1;" >&3
        sleep 0.02
        mapfile -t all < <(payloads)
        if printf '%s\n' "${all[@]}" | grep -qx "marker-$MARKER"; then
            PAYLOADS=("${all[@]:SEEN:${#all[@]}-SEEN-1}")
            SEEN=${#all[@]}
            return 0
        fi
    done
    fail "marker-$MARKER never arrived"
}

expect_count() {
    local n="$1" what="$2"
    next_payloads
    [[ ${#PAYLOADS[@]} == "$n" ]] || fail "$what: expected $n notification(s), got ${#PAYLOADS[@]}: ${PAYLOADS[*]}"
}

# check PAYLOAD EXPR EXPECTED: evaluate EXPR with p = the payload as jsonb.
check() {
    local payload="$1" expr="$2" expected="$3" got
    got="$(sql -v p="$payload" <<SQL
SELECT $expr FROM (SELECT :'p'::jsonb AS p) s;
SQL
)"
    [[ "$got" == "$expected" ]] || fail "payload $payload: $expr = '$got', expected '$expected'"
}

# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------

log "INSERT notifies with the key and heads"
sql -v base="$BASE" <<'SQL'
INSERT INTO docs VALUES (1, :'base'::bytea, 'first');
SQL
expect_count 1 "INSERT"
p="${PAYLOADS[0]}"
check "$p" "p->>'table' || ' ' || (p->>'op') || ' ' || (p->'key')::text" 'public.docs INSERT {"id": 1}'
check "$p" "p->'columns'->'doc'->'heads' = (SELECT to_jsonb(automerge_heads(doc)) FROM docs WHERE id = 1)" t
check "$p" "p->'columns'->'doc' ? 'prev_heads'" f
BASE_HEADS_JSON="$(sql -c "SELECT to_jsonb(automerge_heads(doc)) FROM docs WHERE id = 1")"

log "UPDATE with new changes notifies with heads and prev_heads"
sql -v a="$INC_A" <<'SQL'
UPDATE docs SET doc = merge(doc, :'a'::bytea) WHERE id = 1;
SQL
expect_count 1 "UPDATE"
p="${PAYLOADS[0]}"
check "$p" "p->>'op'" UPDATE
check "$p" "p->'columns'->'doc'->'prev_heads' = '$BASE_HEADS_JSON'::jsonb" t
check "$p" "p->'columns'->'doc'->'heads' = (SELECT to_jsonb(automerge_heads(doc)) FROM docs WHERE id = 1)" t

log "a listener at prev_heads fetches exactly the new changes"
check "$p" "(SELECT automerge_changes_bytes(doc, ARRAY(SELECT jsonb_array_elements_text(p->'columns'->'doc'->'prev_heads'))) = '$INC_A'::bytea
             FROM docs WHERE id = (p->'key'->>'id')::int)" t
# A listener that is already at the new heads (e.g. the writer) gets nothing.
check "$p" "(SELECT octet_length(automerge_changes_bytes(doc, ARRAY(SELECT jsonb_array_elements_text(p->'columns'->'doc'->'heads'))))
             FROM docs WHERE id = 1)" 0

log "no notification for no-op merges, other columns, the contains pattern, rollbacks"
sql -v a="$INC_A" -v base="$BASE" -v b="$INC_B" <<'SQL'
UPDATE docs SET doc = merge(doc, :'a'::bytea) WHERE id = 1;
UPDATE docs SET doc = merge(doc, :'base'::bytea::automerge) WHERE id = 1;
UPDATE docs SET title = 'renamed' WHERE id = 1;
UPDATE docs SET doc = merge(doc, :'a'::bytea)
    WHERE id = 1 AND NOT automerge_contains(doc, :'a'::bytea);
BEGIN;
UPDATE docs SET doc = merge(doc, :'b'::bytea) WHERE id = 1;
ROLLBACK;
SQL
expect_count 0 "no-op writes"

log "two changes in one transaction: one notification per row change"
sql -v b="$INC_B" -v base="$BASE" <<'SQL'
BEGIN;
UPDATE docs SET doc = merge(doc, :'b'::bytea) WHERE id = 1;
INSERT INTO docs VALUES (3, :'base'::bytea);
COMMIT;
SQL
expect_count 2 "transaction"
check "${PAYLOADS[0]}" "p->>'op' || (p->'key')::text || jsonb_array_length(p->'columns'->'doc'->'heads')" 'UPDATE{"id": 1}2'
check "${PAYLOADS[1]}" "p->>'op' || (p->'key')::text" 'INSERT{"id": 3}'

log "identical events in one transaction are all delivered (payloads differ in seq)"
# NOTIFY drops a notification whose payload equals an earlier one of the same
# transaction; without seq the second INSERT (and the third UPDATE) would be
# lost and a listener would end with the wrong state.
sql -v base="$BASE" <<'SQL'
BEGIN;
INSERT INTO docs VALUES (10, :'base'::bytea);
DELETE FROM docs WHERE id = 10;
INSERT INTO docs VALUES (10, :'base'::bytea);
UPDATE docs SET id = 11 WHERE id = 10;
UPDATE docs SET id = 10 WHERE id = 11;
UPDATE docs SET id = 11 WHERE id = 10;
COMMIT;
SQL
expect_count 6 "repeated identical events"
got=""
for p in "${PAYLOADS[@]}"; do
    got+="$(sql -v p="$p" <<'SQL'
SELECT (p->>'op') || (p->'key')::text || coalesce((p->'old_key')::text, '')
FROM (SELECT :'p'::jsonb AS p) s;
SQL
) "
done
[[ "$got" == 'INSERT{"id": 10} DELETE{"id": 10} INSERT{"id": 10} UPDATE{"id": 11}{"id": 10} UPDATE{"id": 10}{"id": 11} UPDATE{"id": 11}{"id": 10} ' ]] \
    || fail "repeated identical events: got $got"
# Replaying the events leaves the listener with row 11, which exists.
[[ "$(sql -c "SELECT count(*) FROM docs WHERE id = 11")" == 1 ]] || fail "row 11 missing"
check "${PAYLOADS[0]}" "(p->>'seq')::bigint < ('${PAYLOADS[2]}'::jsonb->>'seq')::bigint" t
sql -c "DELETE FROM docs WHERE id = 11"
expect_count 1 "cleanup delete"

log "150 heads: the payload drops the heads, the write succeeds"
sql -v many="$MANY" <<'SQL'
INSERT INTO docs VALUES (2, :'many'::bytea);
SQL
expect_count 1 "many heads"
p="${PAYLOADS[0]}"
check "$p" "p - 'seq'" '{"op": "INSERT", "key": {"id": 2}, "table": "public.docs", "columns": {"doc": {}}, "truncated": true}'
[[ ${#p} -lt 8000 ]] || fail "payload too long: ${#p}"
[[ "$(sql -c "SELECT cardinality(automerge_heads(doc)) FROM docs WHERE id = 2")" == 150 ]] \
    || fail "row 2 not written"

log "DELETE notifies with prev_heads"
LAST_HEADS_JSON="$(sql -c "SELECT to_jsonb(automerge_heads(doc)) FROM docs WHERE id = 1")"
sql -c "DELETE FROM docs WHERE id = 1" -c "DELETE FROM docs WHERE id = 2"
expect_count 2 "DELETE"
check "${PAYLOADS[0]}" "p->>'op' || (p->'key')::text" 'DELETE{"id": 1}'
check "${PAYLOADS[0]}" "p->'columns'->'doc'->'prev_heads' = '$LAST_HEADS_JSON'::jsonb" t
check "${PAYLOADS[1]}" "p->>'op' || (p->'key')::text || (p->>'truncated')" 'DELETE{"id": 2}true'
