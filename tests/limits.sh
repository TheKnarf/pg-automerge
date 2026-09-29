#!/usr/bin/env bash
# The load memory limit against a real crash (run via `mise run limits`,
# also part of `mise run test`). See docs/DESIGN.md, "Resource limits".
#
# A scratch cluster (initdb into a temporary directory, its own port) runs
# with its address space capped (`ulimit -v`, LIMITS_AS_KB, default about
# 1 GB), so a load that needs more than that fails to allocate: Rust
# aborts the backend (signal 6) and the postmaster restarts every session.
#
#   1. With pg_automerge.max_load_memory = -1 (no limit), the inputs that
#      crashed a capped server before the limit existed still do: a 12 kB
#      compressed save of a 12,000,000-character text and a 113-byte
#      crafted change chunk of 20,000,000 ops each abort the backend and
#      the cluster restarts (checked in the log), and so does a 19 kB
#      compressed change chunk listing 20,000,000 empty other actors.
#      This proves the inputs and the cap reproduce the crash.
#   2. With the default limit, the same inputs through every path (text
#      input, the bytea cast, merge(automerge, bytea), automerge_contains,
#      COPY) get a clean ERROR 53400 with its DETAIL and HINT (the actor
#      list "at least": the scan stops at its declared length), and nothing
#      restarts: no new "terminated by signal" in the log, a session opened
#      before is still connected, pg_postmaster_start_time() unchanged, and
#      ordinary writes still work.
#
# Env: see tests/lib.sh; LIMITS_PORT (default 28829), LIMITS_AS_KB (the
# address space cap of the scratch server, default 1000000),
# LIMITS_SKIP_CRASH=1 (skip step 1).

# shellcheck source=tests/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

SPORT="${LIMITS_PORT:-28829}"
AS_KB="${LIMITS_AS_KB:-1000000}"
DATA="$WORK/data"
SLOG="$WORK/server.log"
IN="$WORK/in"
SENTINEL_PID=

scratch_ctl() { "$BINDIR/pg_ctl" -D "$DATA" -l "$SLOG" "$@"; }
# psql against the scratch cluster's postgres database.
ssql() {
    PGOPTIONS="-c client_min_messages=warning" "$BINDIR/psql" -X -q -At -v ON_ERROR_STOP=1 \
        -h localhost -p "$SPORT" -d postgres "$@"
}
crashes() { grep -c "was terminated by signal" "$SLOG" || true; }

on_exit() {
    [[ -n "$SENTINEL_PID" ]] && kill "$SENTINEL_PID" 2>/dev/null || true
    if [[ -f "$DATA/postmaster.pid" ]]; then scratch_ctl stop -m immediate >/dev/null 2>&1 || true; fi
    if [[ $1 == 0 ]]; then
        log "all limit checks passed"
    else
        echo "limits test FAILED; server log:" >&2
        tail -30 "$SLOG" >&2 || true
    fi
}

install_extension
mkdir -p "$IN"
log "generating inputs"
cargo run -q -p pg_automerge_core --example gen_limits -- "$IN"

log "scratch cluster on port $SPORT, address space capped at $AS_KB kB"
"$BINDIR/initdb" -D "$DATA" -A trust --no-sync >"$WORK/initdb.log" 2>&1 \
    || { cat "$WORK/initdb.log" >&2; fail "initdb"; }
(
    ulimit -c 0
    ulimit -v "$AS_KB"
    scratch_ctl -w -o "-p $SPORT -c listen_addresses=localhost -k $WORK -c shared_buffers=16MB \
        -c max_connections=20 -c restart_after_crash=on" start >/dev/null
) || fail "the capped server did not start"
ssql -c "CREATE EXTENSION pg_automerge" \
    -c "CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL)" \
    -c "INSERT INTO docs VALUES (1, pg_read_binary_file('$IN/small.bin')::automerge)"

TEXT="pg_read_binary_file('$IN/text.bin')"
OPS="pg_read_binary_file('$IN/ops.bin')"
OTHERS="pg_read_binary_file('$IN/others.bin')"

wait_ready() {
    for _ in $(seq 1 300); do
        if ssql -c "SELECT 1" >/dev/null 2>&1; then return 0; fi
        sleep 0.1
    done
    fail "the scratch server did not come back"
}

if [[ "${LIMITS_SKIP_CRASH:-0}" != 1 ]]; then
    for input in "$TEXT" "$OPS" "$OTHERS"; do
        log "no limit: $input aborts the backend and restarts the cluster"
        before="$(crashes)"
        if out="$(ssql -c "SET pg_automerge.max_load_memory = -1" \
                -c "SELECT length($input::automerge::bytea)" 2>&1)"; then
            fail "the load succeeded under the cap ($out): raise LIMITS_TEXT_CHARS or lower LIMITS_AS_KB"
        fi
        grep -q "server closed the connection unexpectedly" <<<"$out" || fail "no crash: $out"
        wait_ready
        [[ "$(crashes)" -gt "$before" ]] || fail "no \"terminated by signal\" in the log"
        grep -q "memory allocation of .* bytes failed" "$SLOG" || fail "not an allocation failure"
    done
fi

log "default limit: clean errors, nothing restarts"
[[ "$(ssql -c "LOAD 'pg_automerge'" -c "SHOW pg_automerge.max_load_memory")" == 2GB ]] || fail "default is not 2GB"
START="$(ssql -c "SELECT pg_postmaster_start_time()")"
BEFORE="$(crashes)"
# A session that a crash restart would terminate.
PGAPPNAME=limits_sentinel "$BINDIR/psql" -X -q -h localhost -p "$SPORT" \
    -d postgres -c "SELECT pg_sleep(600)" >/dev/null 2>&1 &
SENTINEL_PID=$!
for _ in $(seq 1 100); do
    [[ "$(ssql -c "SELECT count(*) FROM pg_stat_activity WHERE application_name = 'limits_sentinel'")" == 1 ]] \
        && break
    sleep 0.05
done
SENTINEL_BACKEND="$(ssql -c "SELECT pid FROM pg_stat_activity WHERE application_name = 'limits_sentinel'")"
[[ -n "$SENTINEL_BACKEND" ]] || fail "sentinel session did not connect"

TEXT_HEX="$(ssql -c "SELECT encode($TEXT, 'hex')")"
ssql -c "COPY (SELECT $TEXT) TO '$WORK/text.copy'"
for stmt in \
    "SELECT $TEXT::automerge" \
    "SELECT $OPS::automerge" \
    "SELECT '\\x$TEXT_HEX'::automerge" \
    "UPDATE docs SET doc = merge(doc, $TEXT) WHERE id = 1" \
    "UPDATE docs SET doc = doc || $OPS WHERE id = 1" \
    "SELECT automerge_contains(doc, $OPS) FROM docs" \
    "INSERT INTO docs VALUES (2, $TEXT)" \
    "COPY docs (doc) FROM '$WORK/text.copy'" \
    "SELECT $OTHERS::automerge" \
    "UPDATE docs SET doc = merge(doc, $OTHERS) WHERE id = 1" \
    "SELECT automerge_contains(doc, $OTHERS) FROM docs"; do
    out="$(ssql -v VERBOSITY=verbose -c "$stmt" 2>&1)" && fail "no error: ${stmt:0:80}"
    grep -q 'ERROR:  53400: estimated memory to load automerge input exceeds "pg_automerge.max_load_memory" (2048 MB)' \
        <<<"$out" || fail "${stmt:0:80}: $out"
    grep -Eq "DETAIL:  Loading it could take (up to|at least) [0-9]* MB \(" <<<"$out" \
        || fail "${stmt:0:80}: no DETAIL: $out"
    grep -q 'HINT:  A superuser can raise "pg_automerge.max_load_memory".' <<<"$out" \
        || fail "${stmt:0:80}: no HINT: $out"
done

[[ "$(crashes)" == "$BEFORE" ]] || fail "a backend was terminated by a signal"
[[ "$(ssql -c "SELECT pg_postmaster_start_time()")" == "$START" ]] || fail "the postmaster restarted"
[[ "$(ssql -c "SELECT pid FROM pg_stat_activity WHERE application_name = 'limits_sentinel'")" \
   == "$SENTINEL_BACKEND" ]] || fail "the sentinel session was terminated (a crash restart)"
ssql -c "UPDATE docs SET doc = merge(doc, pg_read_binary_file('$IN/small.bin')) WHERE id = 1" \
    -c "INSERT INTO docs VALUES (3, pg_read_binary_file('$IN/small.bin'))"
[[ "$(ssql -c "SELECT count(*) FROM docs WHERE doc->>'status' = 'small'")" == 2 ]] \
    || fail "ordinary writes after the rejected ones"
