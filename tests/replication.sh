#!/usr/bin/env bash
# Logical replication of automerge columns (run via `mise run
# replication`; not part of `mise run test`: it runs its own scratch
# cluster with wal_level = logical).
#
# One publisher and two subscriber databases in a scratch cluster, one
# subscription in text mode and one in binary mode. Checks:
#
# - INSERT (a compressed save, normalized on the publisher), UPDATE via
#   merge(doc, bytea) and DELETE replicate with a primary-key replica
#   identity; the subscriber's values are byte-identical (the subscriber
#   validates every value again: automerge_in or automerge_recv);
# - an automerge_notify() trigger on the subscriber does not fire for
#   replicated rows, until it is ENABLE ALWAYS;
# - REPLICA IDENTITY FULL on a table with an automerge column: UPDATE and
#   DELETE cannot be applied (the type has no equality operator), the
#   apply worker fails and retries, and the row is not changed.
#
# Env: see tests/lib.sh; REPL_PORT (default 28819) for the scratch cluster.

# shellcheck source=tests/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

PORT="${REPL_PORT:-28819}"
CLUSTER="$WORK/cluster"
LISTENER_PID=
DB=pub

on_exit() {
    exec 3>&- 2>/dev/null || true
    [[ -n "$LISTENER_PID" ]] && kill "$LISTENER_PID" 2>/dev/null || true
    if [[ -d "$CLUSTER" ]]; then
        "$BINDIR/pg_ctl" -D "$CLUSTER" -m immediate stop >/dev/null 2>&1 || true
    fi
    if [[ $1 != 0 && -f "$WORK/cluster.log" ]]; then
        echo "--- cluster log (tail):" >&2
        tail -30 "$WORK/cluster.log" >&2
    fi
    if [[ $1 == 0 ]]; then log "all replication checks passed"; else echo "replication test FAILED" >&2; fi
}

log "generating fixture documents"
eval "$(cargo run -q -p pg_automerge_core --example gen_concurrency)"

install_extension

log "starting a scratch cluster with wal_level = logical on port $PORT"
"$BINDIR/initdb" -D "$CLUSTER" -A trust >/dev/null
"$BINDIR/pg_ctl" -D "$CLUSTER" -l "$WORK/cluster.log" -w start \
    -o "-p $PORT -k $WORK -c listen_addresses=localhost -c wal_level=logical -c max_wal_senders=10 -c max_replication_slots=10 -c wal_retrieve_retry_interval=200ms" \
    >/dev/null

for db in pub sub_text sub_bin; do
    sql_on postgres -c "CREATE DATABASE $db"
    sql_on "$db" <<'SQL'
CREATE EXTENSION pg_automerge;
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
CREATE TABLE docs_full (id int, doc automerge NOT NULL);
ALTER TABLE docs_full REPLICA IDENTITY FULL;
SQL
done
sql_on pub <<'SQL'
CREATE PUBLICATION docs_pub FOR TABLE docs;
CREATE PUBLICATION docs_full_pub FOR TABLE docs_full;
SQL
# A subscription to the same cluster cannot create its slot itself.
for mode in text bin; do
    binary=false
    [[ $mode == bin ]] && binary=true
    sql_on pub -c "SELECT pg_create_logical_replication_slot('docs_$mode', 'pgoutput')" >/dev/null
    sql_on "sub_$mode" -c "CREATE SUBSCRIPTION docs_sub CONNECTION 'host=localhost port=$PORT dbname=pub'
        PUBLICATION docs_pub WITH (create_slot = false, slot_name = 'docs_$mode', binary = $binary)"
done
sql_on pub -c "SELECT pg_create_logical_replication_slot('docs_full', 'pgoutput')" >/dev/null
sql_on sub_text -c "CREATE SUBSCRIPTION docs_full_sub CONNECTION 'host=localhost port=$PORT dbname=pub'
    PUBLICATION docs_full_pub WITH (create_slot = false, slot_name = 'docs_full')"

# The trigger on the subscriber, and a listener there.
sql_on sub_text -c "CREATE TRIGGER docs_notify AFTER INSERT OR UPDATE OR DELETE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'id')"
mkfifo "$WORK/listener.in"
"$BINDIR/psql" -X -At -h localhost -p "$PORT" -d sub_text <"$WORK/listener.in" >"$WORK/listener.out" 2>&1 &
LISTENER_PID=$!
exec 3>"$WORK/listener.in"
echo "LISTEN docs_changed;" >&3

FINGERPRINT="SELECT coalesce(string_agg(id || ':' || md5(doc::bytea) || ':' || automerge_heads(doc)::text, ',' ORDER BY id), '')
             FROM docs"

# Wait until both subscribers have what the publisher has.
wait_synced() {
    local what="$1" want got
    want="$(sql_on pub -c "$FINGERPRINT")"
    for _ in $(seq 1 500); do
        if [[ "$(sql_on sub_text -c "$FINGERPRINT")" == "$want" && "$(sql_on sub_bin -c "$FINGERPRINT")" == "$want" ]]; then
            return 0
        fi
        sleep 0.02
    done
    got="$(sql_on sub_text -c "$FINGERPRINT") / $(sql_on sub_bin -c "$FINGERPRINT")"
    fail "$what: subscribers not in sync: want $want, got $got"
}

# Notifications the listener has received so far.
notifications() {
    echo "SELECT 'poll';" >&3
    for _ in $(seq 1 100); do
        grep -q '^poll$' "$WORK/listener.out" && break
        sleep 0.02
    done
    grep -c '^Asynchronous notification "docs_changed"' "$WORK/listener.out" || true
    : >"$WORK/listener.out"
}

log "INSERT, UPDATE (merge), DELETE with a primary-key replica identity"
sql_on pub -v base="$BASE" -v a="$NEW_A" <<'SQL'
INSERT INTO docs VALUES (1, :'base'), (2, :'a'), (3, :'base');
SQL
wait_synced "insert"
sql_on pub -v inc="$INC_B" <<<"UPDATE docs SET doc = merge(doc, :'inc'::bytea) WHERE id = 1"
wait_synced "update"
[[ "$(sql_on sub_bin -c "SELECT doc->>'inc_b' IS NOT NULL FROM docs WHERE id = 1")" == t ]] \
    || fail "the merged change did not arrive"
sql_on pub -c "DELETE FROM docs WHERE id = 3"
wait_synced "delete"
[[ "$(notifications)" == 0 ]] || fail "the trigger fired for replicated rows"

log "ENABLE ALWAYS: the trigger fires for replicated rows"
sql_on sub_text -c "ALTER TABLE docs ENABLE ALWAYS TRIGGER docs_notify"
sql_on pub -v inc="$INC_A" <<<"UPDATE docs SET doc = merge(doc, :'inc'::bytea) WHERE id = 1"
wait_synced "update after ENABLE ALWAYS"
[[ "$(notifications)" == 1 ]] || fail "the ENABLE ALWAYS trigger did not fire once"

log "REPLICA IDENTITY FULL: inserts replicate, updates and deletes cannot be applied"
sql_on pub -v base="$BASE" <<<"INSERT INTO docs_full VALUES (1, :'base')"
wait_for_full() {
    for _ in $(seq 1 500); do
        [[ "$(sql_on sub_text -c "$1")" == t ]] && return 0
        sleep 0.02
    done
    fail "timed out: $1"
}
wait_for_full "SELECT count(*) = 1 FROM docs_full"
sql_on pub -v inc="$INC_A" <<<"UPDATE docs_full SET doc = merge(doc, :'inc'::bytea)"
wait_for_full "SELECT apply_error_count > 0 FROM pg_stat_subscription_stats WHERE subname = 'docs_full_sub'"
grep -q 'could not identify an equality operator for type .*automerge' "$WORK/cluster.log" \
    || fail "unexpected apply error: $(grep -i error "$WORK/cluster.log" | tail -3)"
[[ "$(sql_on sub_text -c "SELECT doc->>'inc_a' IS NULL FROM docs_full")" == t ]] \
    || fail "the FULL-identity update was applied"
sql_on sub_text -c "ALTER SUBSCRIPTION docs_full_sub DISABLE"
