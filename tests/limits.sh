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
#      So do the inputs of the estimate's rebuild terms: a document chunk
#      under 1 kB whose 6,000 changes share one 100 kB message (a repeat
#      run, which every rebuilt change copies), and a 100 kB change chunk
#      of 16,000 puts of one 100 kB key (a repeat run, which applying
#      copies per op), and a 100 kB document chunk of 6,000 changes by one
#      actor whose id is 100 kB long (every rebuilt change copies it), and
#      a 24 kB compressed change chunk whose column metadata lists
#      12,000,000 empty columns.
#      This proves the inputs and the cap reproduce the crash.
#   2. With the default limit, the same inputs through every path (text
#      input, the bytea cast, merge(automerge, bytea), automerge_contains,
#      COPY) get a clean ERROR 53400 with its DETAIL and HINT (the actor
#      list "at least": the scan stops at its declared length), and nothing
#      restarts: no new "terminated by signal" in the log, a session opened
#      before is still connected, pg_postmaster_start_time() unchanged, and
#      ordinary writes still work.
#   3. A 14 kB document whose text holds a block nested 5,000 levels deep
#      (Automerge's recursive rendering of it needs about 65 MB of stack;
#      the server runs with an 8 MB stack, `ulimit -s`, as in Docker) and
#      the same document as change chunks: inserted into a table with a
#      stored generated doc::jsonb column and two expression indexes,
#      merged into a stored row, read with ::jsonb, ->>, the heads
#      overload of automerge_to_jsonb and merge_agg, all succeed with the
#      expected values; automerge_spans on that text is a clean 54000; and
#      nothing crashed (the checks of step 2, which run after this).
#
# Container mode (LIMITS_CONTAINER=<name>, with PG_AUTOMERGE_TEST_HOST and
# friends pointing at it, see tests/lib.sh; tests/docker.sh does this):
# instead of a scratch cluster, a running container of the Docker image
# started with a memory limit (docker run --memory, no swap) and the
# default pg_automerge.max_load_memory. The inputs are copied into it
# (the server reads them with pg_read_binary_file), the server log is
# `docker logs`, and a load past the cap is killed by the kernel's OOM
# killer (signal 9) or fails to allocate (signal 6); either way the
# postmaster restarts every session. The checks are the same, except that
# the compressed chunk of 20,000,000 empty other actors may get Automerge's
# own 22P02 instead of a crash in step 1 (it reserves address space it
# hardly touches, which a cgroup does not count), plus: the container
# itself was neither restarted nor replaced (its start time and restart
# count). The tables are made in database pg_automerge_limits.
#
# Env: see tests/lib.sh; LIMITS_PORT (default 28829), LIMITS_AS_KB (the
# address space cap of the scratch server, default 1000000),
# LIMITS_SKIP_CRASH=1 (skip step 1), LIMITS_CONTAINER (container mode).

# shellcheck source=tests/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

CONTAINER="${LIMITS_CONTAINER:-}"
SPORT="${LIMITS_PORT:-28829}"
AS_KB="${LIMITS_AS_KB:-1000000}"
DATA="$WORK/data"
SLOG="$WORK/server.log"
IN="$WORK/in"
SENTINEL_PID=

if [[ -n "$CONTAINER" ]]; then
    [[ $EXTERNAL == 1 ]] || fail "LIMITS_CONTAINER needs PG_AUTOMERGE_TEST_HOST/PORT/PASSWORD for it"
    SDB=pg_automerge_limits
    DROP_DBS=("$SDB")
    S_CONN=("${CONN[@]}")
    # Where the server reads the inputs and writes the COPY file.
    SIN=/tmp/pg-automerge-limits
    server_log() { docker logs "$CONTAINER" 2>&1; }
    container_state() { docker inspect -f '{{.State.Running}} {{.State.StartedAt}} {{.RestartCount}}' "$CONTAINER"; }
else
    require_pgrx_mode
    SDB=postgres
    S_CONN=(-h localhost -p "$SPORT")
    SIN="$IN"
    server_log() { cat "$SLOG"; }
fi

scratch_ctl() { "$BINDIR/pg_ctl" -D "$DATA" -l "$SLOG" "$@"; }
# psql against the capped server's test database.
ssql() {
    PGOPTIONS="-c client_min_messages=warning" "$BINDIR/psql" -X -q -At -v ON_ERROR_STOP=1 \
        "${S_CONN[@]}" -d "$SDB" "$@"
}
crashes() { server_log | grep -c "was terminated by signal" || true; }

on_exit() {
    [[ -n "$SENTINEL_PID" ]] && kill "$SENTINEL_PID" 2>/dev/null || true
    if [[ -f "$DATA/postmaster.pid" ]]; then scratch_ctl stop -m immediate >/dev/null 2>&1 || true; fi
    if [[ -n "$CONTAINER" ]]; then docker exec -u 0 "$CONTAINER" rm -rf "$SIN" >/dev/null 2>&1 || true; fi
    if [[ $1 == 0 ]]; then
        log "all limit checks passed"
    else
        echo "limits test FAILED; server log:" >&2
        server_log | tail -30 >&2 || true
    fi
}

install_extension
mkdir -p "$IN"
log "generating inputs"
cargo run -q -p pg_automerge_core --example gen_limits -- "$IN"

if [[ -n "$CONTAINER" ]]; then
    mem="$(docker inspect -f '{{.HostConfig.Memory}} {{.HostConfig.MemorySwap}}' "$CONTAINER")" \
        || fail "container $CONTAINER not found"
    read -r mem_bytes swap_bytes <<<"$mem"
    # Without a cap, step 1 would take the host's memory instead.
    [[ "$mem_bytes" -gt 0 && "$swap_bytes" == "$mem_bytes" ]] \
        || fail "container $CONTAINER needs --memory and --memory-swap set to the same value (got $mem)"
    log "container $CONTAINER, memory capped at $((mem_bytes / 1048576)) MB (no swap)"
    CONTAINER_STATE="$(container_state)"
    [[ "$CONTAINER_STATE" == "true "* ]] || fail "container $CONTAINER is not running"
    docker cp -q "$IN/." "$CONTAINER:$SIN" && docker exec -u 0 "$CONTAINER" chown -R postgres:postgres "$SIN" \
        || fail "copying the inputs into $CONTAINER"
    start_server
    sql_on postgres -c "DROP DATABASE IF EXISTS $SDB WITH (FORCE)" -c "CREATE DATABASE $SDB"
else
    log "scratch cluster on port $SPORT, address space capped at $AS_KB kB"
    "$BINDIR/initdb" -D "$DATA" -A trust --no-sync >"$WORK/initdb.log" 2>&1 \
        || { cat "$WORK/initdb.log" >&2; fail "initdb"; }
    (
        ulimit -c 0
        ulimit -v "$AS_KB"
        # Deep structures must not need more than the usual 8 MB (step 3).
        ulimit -s 8192
        scratch_ctl -w -o "-p $SPORT -c listen_addresses=localhost -k $WORK -c shared_buffers=16MB \
            -c max_connections=20 -c restart_after_crash=on" start >/dev/null
    ) || fail "the capped server did not start"
fi
ssql -c "CREATE EXTENSION pg_automerge" \
    -c "CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL)" \
    -c "INSERT INTO docs VALUES (1, pg_read_binary_file('$SIN/small.bin')::automerge)"

TEXT="pg_read_binary_file('$SIN/text.bin')"
OPS="pg_read_binary_file('$SIN/ops.bin')"
OTHERS="pg_read_binary_file('$SIN/others.bin')"
MESSAGES="pg_read_binary_file('$SIN/messages.bin')"
KEYS="pg_read_binary_file('$SIN/keys.bin')"
ACTORS="pg_read_binary_file('$SIN/actors.bin')"
COLUMNS="pg_read_binary_file('$SIN/columns.bin')"
DEEP="pg_read_binary_file('$SIN/deep_block.bin')"
DEEP_CHANGES="pg_read_binary_file('$SIN/deep_changes.bin')"

wait_ready() {
    for _ in $(seq 1 300); do
        if ssql -c "SELECT 1" >/dev/null 2>&1; then return 0; fi
        sleep 0.1
    done
    fail "the scratch server did not come back"
}

if [[ "${LIMITS_SKIP_CRASH:-0}" != 1 ]]; then
    for input in "$TEXT" "$OPS" "$OTHERS" "$MESSAGES" "$KEYS" "$ACTORS" "$COLUMNS"; do
        log "no limit: $input aborts the backend and restarts the cluster"
        before="$(crashes)"
        if out="$(ssql -c "SET pg_automerge.max_load_memory = -1" \
                -c "SELECT length($input::automerge::bytea)" 2>&1)"; then
            fail "the load succeeded under the cap ($out): raise LIMITS_TEXT_CHARS or lower LIMITS_AS_KB"
        fi
        if [[ -n "$CONTAINER" && "$input" == "$OTHERS" ]] && grep -q "ERROR:  invalid automerge document" <<<"$out"; then
            # Its 20,000,000-entry actor table is reserved but hardly
            # touched before Automerge rejects the chunk: under a cap on
            # address space (ulimit -v) the reservation fails and aborts,
            # under a cap on used memory (the cgroup) it does not.
            log "  no crash under the cgroup cap (the reservation is not touched): ${out:0:80}"
            continue
        fi
        grep -q "server closed the connection unexpectedly" <<<"$out" || fail "no crash: $out"
        wait_ready
        [[ "$(crashes)" -gt "$before" ]] || fail "no \"terminated by signal\" in the log"
        if [[ -z "$CONTAINER" ]]; then
            grep -q "memory allocation of .* bytes failed" "$SLOG" || fail "not an allocation failure"
        else
            # The cgroup's OOM killer (9) or a failed allocation (6).
            log "  $(server_log | grep -o "was terminated by signal [0-9]*: [A-Za-z ]*" | tail -1)"
        fi
    done
    if [[ -n "$CONTAINER" ]]; then
        # The postmaster (the container's PID 1) survived its backends' deaths.
        [[ "$(container_state)" == "$CONTAINER_STATE" ]] \
            || fail "the container restarted: $CONTAINER_STATE -> $(container_state)"
    fi
fi

log "default limit: clean errors, nothing restarts"
[[ "$(ssql -c "LOAD 'pg_automerge'" -c "SHOW pg_automerge.max_load_memory")" == 2GB ]] || fail "default is not 2GB"
START="$(ssql -c "SELECT pg_postmaster_start_time()")"
BEFORE="$(crashes)"
# A session that a crash restart would terminate.
PGAPPNAME=limits_sentinel "$BINDIR/psql" -X -q "${S_CONN[@]}" \
    -d "$SDB" -c "SELECT pg_sleep(600)" >/dev/null 2>&1 &
SENTINEL_PID=$!
for _ in $(seq 1 100); do
    [[ "$(ssql -c "SELECT count(*) FROM pg_stat_activity WHERE application_name = 'limits_sentinel'")" == 1 ]] \
        && break
    sleep 0.05
done
SENTINEL_BACKEND="$(ssql -c "SELECT pid FROM pg_stat_activity WHERE application_name = 'limits_sentinel'")"
[[ -n "$SENTINEL_BACKEND" ]] || fail "sentinel session did not connect"

TEXT_HEX="$(ssql -c "SELECT encode($TEXT, 'hex')")"
ssql -c "COPY (SELECT $TEXT) TO '$SIN/text.copy'"
for stmt in \
    "SELECT $TEXT::automerge" \
    "SELECT $OPS::automerge" \
    "SELECT '\\x$TEXT_HEX'::automerge" \
    "UPDATE docs SET doc = merge(doc, $TEXT) WHERE id = 1" \
    "UPDATE docs SET doc = doc || $OPS WHERE id = 1" \
    "SELECT automerge_contains(doc, $OPS) FROM docs" \
    "INSERT INTO docs VALUES (2, $TEXT)" \
    "COPY docs (doc) FROM '$SIN/text.copy'" \
    "SELECT $OTHERS::automerge" \
    "UPDATE docs SET doc = merge(doc, $OTHERS) WHERE id = 1" \
    "SELECT automerge_contains(doc, $OTHERS) FROM docs" \
    "SELECT $MESSAGES::automerge" \
    "INSERT INTO docs VALUES (4, $MESSAGES)" \
    "SELECT $KEYS::automerge" \
    "UPDATE docs SET doc = doc || $KEYS WHERE id = 1" \
    "SELECT automerge_contains(doc, $KEYS) FROM docs" \
    "SELECT $ACTORS::automerge" \
    "INSERT INTO docs VALUES (5, $ACTORS)" \
    "SELECT $COLUMNS::automerge" \
    "UPDATE docs SET doc = merge(doc, $COLUMNS) WHERE id = 1" \
    "SELECT automerge_contains(doc, $COLUMNS) FROM docs"; do
    out="$(ssql -v VERBOSITY=verbose -c "$stmt" 2>&1)" && fail "no error: ${stmt:0:80}"
    grep -q 'ERROR:  53400: estimated memory to load automerge input exceeds "pg_automerge.max_load_memory" (2048 MB)' \
        <<<"$out" || fail "${stmt:0:80}: $out"
    grep -Eq "DETAIL:  Loading it could take (up to|at least) [0-9]* MB \(" <<<"$out" \
        || fail "${stmt:0:80}: no DETAIL: $out"
    grep -q 'HINT:  A superuser can raise "pg_automerge.max_load_memory".' <<<"$out" \
        || fail "${stmt:0:80}: no HINT: $out"
done

log "deep blocks: every read works, spans refuse cleanly, no stack overflow"
ssql -c "CREATE TABLE deep (id int PRIMARY KEY, doc automerge NOT NULL,
             data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED)" \
    -c "CREATE INDEX deep_gin ON deep USING gin ((doc::jsonb))" \
    -c "CREATE INDEX deep_status ON deep ((doc->>'status'))" \
    -c "INSERT INTO deep VALUES (1, $DEEP)" \
    -c "INSERT INTO docs VALUES (10, pg_read_binary_file('$SIN/small.bin'))" \
    -c "UPDATE docs SET doc = merge(doc, $DEEP_CHANGES) WHERE id = 10" \
    || fail "storing the deep-block document"
expected='{"body": "\uFFFCx", "status": "deep"}'
for q in \
    "SELECT data FROM deep" \
    "SELECT doc::jsonb FROM deep" \
    "SELECT $DEEP::automerge::jsonb" \
    "SELECT automerge_to_jsonb(doc, automerge_heads(doc)) FROM deep" \
    "SELECT merge_agg(doc)::jsonb FROM deep" \
    "SELECT doc::jsonb FROM docs WHERE id = 10"; do
    out="$(ssql -c "SELECT ($q) = '$expected'::jsonb" 2>&1)" || fail "${q:0:80}: $out"
    [[ "$out" == t ]] || fail "${q:0:80}: not the expected jsonb"
done
# Its status and the small one's conflict; the deep one's actor wins.
[[ "$(ssql -c "SELECT doc->>'status' FROM docs WHERE id = 10")" == deep ]] \
    || fail "the merged row's status"
out="$(ssql -v VERBOSITY=verbose -c "SELECT automerge_spans(doc, '{body}') FROM deep" 2>&1)" \
    && fail "automerge_spans of the deep block succeeded"
grep -q "ERROR:  54000: automerge text block is nested more than 32 levels deep" <<<"$out" \
    || fail "automerge_spans: $out"

[[ "$(crashes)" == "$BEFORE" ]] || fail "a backend was terminated by a signal"
[[ "$(ssql -c "SELECT pg_postmaster_start_time()")" == "$START" ]] || fail "the postmaster restarted"
[[ "$(ssql -c "SELECT pid FROM pg_stat_activity WHERE application_name = 'limits_sentinel'")" \
   == "$SENTINEL_BACKEND" ]] || fail "the sentinel session was terminated (a crash restart)"
ssql -c "UPDATE docs SET doc = merge(doc, pg_read_binary_file('$SIN/small.bin')) WHERE id = 1" \
    -c "INSERT INTO docs VALUES (3, pg_read_binary_file('$SIN/small.bin'))"
[[ "$(ssql -c "SELECT count(*) FROM docs WHERE doc->>'status' = 'small'")" == 2 ]] \
    || fail "ordinary writes after the rejected ones"
if [[ -n "$CONTAINER" ]]; then
    [[ "$(container_state)" == "$CONTAINER_STATE" ]] \
        || fail "the container restarted: $CONTAINER_STATE -> $(container_state)"
fi
