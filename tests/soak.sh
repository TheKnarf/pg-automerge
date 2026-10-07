#!/usr/bin/env bash
# Soak test: a long-running concurrent load against the release Docker
# image, sampled over time, to find what only shows after many thousands
# of statements per backend: memory that grows (Rust heap, Postgres memory
# contexts, RSS), latency that drifts, tables and TOAST that bloat.
# `mise run soak` (builds the image first); `SOAK_SMOKE=1` is a short run
# with small documents that tests/docker.sh includes. Results and usage:
# docs/src/pages/operations/soak-test.mdx.
#
# Setup: one container of the image (labelled pg-automerge-test, removed
# afterwards with its volume unless SOAK_KEEP=1) with a memory limit
# (--memory, no swap), lz4 TOAST compression, max_load_memory, autovacuum
# logging. The fixtures (crates/pg_automerge_core/examples/gen_soak.rs):
# 20 template documents from 1 kB to about SOAK_DOC_SIZE kB (list maps and
# rich texts), and for each 3 writer lanes of single-change edits plus full
# saves every SOAK_CHECKPOINT changes. Table soak_docs (id, slot = id % 20,
# doc automerge, a generated data jsonb column with a GIN index, an
# automerge_notify trigger); soak_cursor holds each (row, lane)'s position.
# The GIN index has fastupdate off (SOAK_GIN_FASTUPDATE): with the pending
# list, the planner stops using the index while writes keep it full, and
# the sequential scan detoasts every row's jsonb
# (docs/src/pages/design/benchmarks/2026-10-01.mdx, "Soak test (2026-10-01)").
#
# Load: pgbench (the image's own, in a second container sharing the
# server's network, so the host needs no pgbench) with SOAK_CLIENTS
# long-lived connections (prepared statements) for SOAK_DURATION, a
# weighted mix of the scripts in tests/soak/: incremental writes
# (UPDATE .. SET doc = merge(doc, changes)) to random rows (the big ones
# separately) and, from every client, to the four newest rows (concurrent
# writers on one row), upserts
# of full saves, new rows, re-sent changes skipped by NOT
# automerge_contains, jsonb operators (the big documents separately),
# fetching the bytes, GIN containment
# queries and reads of the generated column, automerge_spans, the history
# functions, merge_agg, and self-sampling of automerge_memory_usage() and
# pg_backend_memory_contexts into soak_mem. A psql session LISTENs to the
# trigger's channel throughout. VACUUM (ANALYZE) runs every
# SOAK_VACUUM_INTERVAL on top of autovacuum.
#
# Samples every SOAK_SAMPLE_INTERVAL (in SOAK_OUT): proc.csv (every postgres
# process's RSS, anonymous/file/shared, and title), cgroup.csv (the
# container's memory), db.csv (rows, heap/TOAST/index sizes, live and dead
# tuples, pgstattuple_approx free and dead space, autovacuum counts, WAL
# bytes, notification queue, commits/rollbacks/deadlocks, average stored
# size and change count per size class), activity.csv (the pgbench
# backends' pids), listener.csv (notifications received), vacuum.csv; at
# the end soak_mem as mem.csv, pgbench's per-transaction logs and summary,
# the server log. tests/soak_report.py turns them into report.txt:
# latency percentiles per script and time window, memory per backend
# over time with growth rates, table and TOAST growth and bloat, WAL.
#
# Checks (the exit status): no pgbench client aborted and no failed
# transaction; no ERROR, crash, panic or restart in the server log, the
# container not OOM-killed; every row has exactly the changes its lanes
# wrote (automerge_change_count = template + lane positions: no lost or
# duplicated write under concurrency); the generated column equals
# doc::jsonb; the listener received exactly one well-formed notification
# per write that changed a document, and none for the re-sends; every
# memory sample has no live document and at most SOAK_ALLOC_SLACK bytes
# allocated between statements, and no backend's memory contexts growing
# by more than 1 MB from the first to the second half; with
# SOAK_MAX_RSS_GROWTH_MB (default 64 for runs of 20 minutes or more, off
# for shorter ones), no pgbench
# backend's anonymous RSS reaches a new high in the second half of the run
# more than that above its high of the first half (after the warm-up): a
# leak keeps raising the high-water mark, while memory malloc keeps for
# reuse goes up and down below a level set by the largest documents.
#
# Env:
#   SOAK_DURATION        run time: 3600, 90s, 60m, 1h (default 60m; smoke 60s)
#   SOAK_CLIENTS         pgbench connections (default 8; smoke 4)
#   SOAK_DOC_SIZE        largest documents in kB (default 1000; smoke 200)
#   SOAK_ROWS            rows before the run (default 200; smoke 60)
#   SOAK_LANE_CHANGES    changes per lane (default 250; smoke 60)
#   SOAK_CHECKPOINT      full save every N lane changes (default 25; smoke 20)
#   SOAK_MEMORY          container memory limit (default 4g; smoke 2g)
#   SOAK_MAX_LOAD_MEMORY pg_automerge.max_load_memory (default 512MB)
#   SOAK_SHARED_BUFFERS  shared_buffers (default 256MB)
#   SOAK_SAMPLE_INTERVAL seconds between samples (default 30; smoke 5)
#   SOAK_VACUUM_INTERVAL seconds between manual VACUUMs, 0: none
#                        (default 600; smoke 20)
#   SOAK_WEIGHTS         override script weights, e.g. "write=30 spans=0"
#   SOAK_TRIM_THRESHOLD  pg_automerge.trim_threshold (default: the server's)
#   SOAK_GIN_FASTUPDATE  the GIN index's fastupdate (default off, as
#                        docs/src/pages/operations/index.mdx recommends;
#                        on: Postgres' default)
#   SOAK_ALLOC_SLACK     bytes allowed in allocated_bytes (default 65536)
#   SOAK_OUT             output directory (default target/soak/<UTC time>)
#   SOAK_KEEP=1          keep the container (and its volume) afterwards
#   SOAK_SMOKE=1         the short defaults above
#   PG_AUTOMERGE_IMAGE   image (default pg-automerge:<Cargo.toml version>)
#   PG_CONFIG            the client tools (psql; default pgrx's pg18)

# shellcheck source=tests/docker_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/docker_lib.sh"

SMOKE="${SOAK_SMOKE:-0}"
if [[ $SMOKE == 1 ]]; then
    d_duration=60s d_clients=4 d_doc=200 d_rows=60 d_lane=60 d_ckpt=20 d_mem=2g d_sample=5 d_vacuum=20
else
    d_duration=60m d_clients=8 d_doc=1000 d_rows=200 d_lane=250 d_ckpt=25 d_mem=4g d_sample=30 d_vacuum=600
fi
duration_s() {
    local v="$1"
    case "$v" in
        *h) echo $((${v%h} * 3600)) ;; *m) echo $((${v%m} * 60)) ;;
        *s) echo "${v%s}" ;; *) echo "$v" ;;
    esac
}
DURATION="$(duration_s "${SOAK_DURATION:-$d_duration}")"
CLIENTS="${SOAK_CLIENTS:-$d_clients}"
DOC_SIZE="${SOAK_DOC_SIZE:-$d_doc}"
ROWS="${SOAK_ROWS:-$d_rows}"
LANE_CHANGES="${SOAK_LANE_CHANGES:-$d_lane}"
CHECKPOINT="${SOAK_CHECKPOINT:-$d_ckpt}"
MEMORY="${SOAK_MEMORY:-$d_mem}"
MAX_LOAD="${SOAK_MAX_LOAD_MEMORY:-512MB}"
SHARED_BUFFERS="${SOAK_SHARED_BUFFERS:-256MB}"
SAMPLE="${SOAK_SAMPLE_INTERVAL:-$d_sample}"
VACUUM_EVERY="${SOAK_VACUUM_INTERVAL:-$d_vacuum}"
ALLOC_SLACK="${SOAK_ALLOC_SLACK:-65536}"
if ((DURATION >= 1200)); then d_growth=64; else d_growth=0; fi
MAX_GROWTH="${SOAK_MAX_RSS_GROWTH_MB:-$d_growth}"
TRIM="${SOAK_TRIM_THRESHOLD:-}"
FASTUPDATE="${SOAK_GIN_FASTUPDATE:-off}"
[[ $FASTUPDATE == on || $FASTUPDATE == off ]] || fail "SOAK_GIN_FASTUPDATE: on or off"
OUT="${SOAK_OUT:-target/soak/$(date -u +%Y%m%dT%H%M%SZ)}"
for n in DURATION CLIENTS DOC_SIZE ROWS LANE_CHANGES CHECKPOINT SAMPLE VACUUM_EVERY ALLOC_SLACK; do
    [[ "${!n}" =~ ^[0-9]+$ ]] || fail "$n: not a number: ${!n}"
done
((ROWS >= 20)) || fail "SOAK_ROWS must be at least 20 (one row per template)"

# script=weight (tests/soak/<script>.sql), in pgbench's script order.
declare -A WEIGHT=(
    [write]=27 [write_big]=3 [write_hot]=8 [upsert]=5 [create]=2 [resend]=3 [read_ops]=18
    [read_big]=2 [fetch]=5 [read_gin]=10 [spans]=5 [history]=5 [merge_agg]=4 [monitor]=3
)
SCRIPTS=(write write_big write_hot upsert create resend read_ops read_big fetch read_gin spans history merge_agg monitor)
for kv in ${SOAK_WEIGHTS:-}; do
    [[ -n "${WEIGHT[${kv%%=*}]+x}" && "${kv#*=}" =~ ^[0-9]+$ ]] || fail "SOAK_WEIGHTS: bad entry $kv"
    WEIGHT[${kv%%=*}]="${kv#*=}"
done

PG_CONFIG="${PG_CONFIG:-$(sed -n 's/^pg18 *= *"\(.*\)"/\1/p' "${PGRX_HOME:-$HOME/.pgrx}/config.toml" 2>/dev/null)}"
[[ -x "$PG_CONFIG" ]] || fail "needs Postgres 18 client tools (psql): PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config or pgrx's (mise run pgrx-init)"
BINDIR="$("$PG_CONFIG" --bindir)"

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
LISTENER_PID=
SAMPLER_PID=
on_exit() {
    touch "$OUT/stop" 2>/dev/null || true
    [[ -n "$SAMPLER_PID" ]] && kill "$SAMPLER_PID" 2>/dev/null
    [[ -n "$LISTENER_PID" ]] && kill "$LISTENER_PID" 2>/dev/null
    wait 2>/dev/null || true
    if [[ "${SOAK_KEEP:-0}" == 1 && -n "${SERVER:-}" ]]; then
        # Leave it out of the cleanup list: docker_cleanup removes only
        # what is listed.
        CONTAINERS=(); VOLUMES=()
        log "kept container $SERVER (docker rm -f -v $SERVER; docker volume rm $PROJECT-soak)"
    fi
    if [[ $1 == 0 ]]; then log "soak passed; results in $OUT"; else echo "soak FAILED; results in $OUT" >&2; fi
}

cat >"$OUT/params.txt" <<EOF
image=$IMAGE
duration_s=$DURATION
clients=$CLIENTS
doc_size_kb=$DOC_SIZE
rows=$ROWS
lane_changes=$LANE_CHANGES
checkpoint=$CHECKPOINT
memory=$MEMORY
max_load_memory=$MAX_LOAD
shared_buffers=$SHARED_BUFFERS
sample_interval_s=$SAMPLE
vacuum_interval_s=$VACUUM_EVERY
weights=$(for s in "${SCRIPTS[@]}"; do printf '%s=%s ' "$s" "${WEIGHT[$s]}"; done)
max_rss_growth_mb=$MAX_GROWTH
trim_threshold=${TRIM:-default}
gin_fastupdate=$FASTUPDATE
alloc_slack=$ALLOC_SLACK
EOF
log "parameters ($OUT/params.txt): $(tr '\n' ' ' <"$OUT/params.txt")"

log "generating fixtures (DOC_SIZE=$DOC_SIZE kB, $LANE_CHANGES changes per lane)"
# The smoke run's small documents need no release build (the suites of
# tests/docker.sh share the dev build; dependencies are optimized anyway).
PROFILE=(--release); [[ $SMOKE == 1 ]] && PROFILE=()
cargo run "${PROFILE[@]}" -q -p pg_automerge_core --example gen_soak -- "$DOC_SIZE" "$LANE_CHANGES" "$CHECKPOINT" \
    >"$DWORK/fixtures.sql" 2>"$OUT/templates.txt" || { cat "$OUT/templates.txt" >&2; fail "gen_soak"; }

log "starting the server container (--memory=$MEMORY)"
start_ready soak soak --memory="$MEMORY" --memory-swap="$MEMORY" --shm-size=256m -- \
    -c default_toast_compression=lz4 -c "pg_automerge.max_load_memory=$MAX_LOAD" \
    -c "shared_buffers=$SHARED_BUFFERS" -c log_autovacuum_min_duration=0 -c track_io_timing=on \
    -c 'log_line_prefix=%m [%p] %a ' ${TRIM:+-c "pg_automerge.trim_threshold=$TRIM"}
SERVER="$(cname soak)"
PORT="$(host_port soak)"
export PGPASSWORD="$PG_PASSWORD"
CONN=(-h 127.0.0.1 -p "$PORT" -U "$PG_USER" -d app)
q() { PGAPPNAME="${APP:-soak_control}" "$BINDIR/psql" -X -q -At -v ON_ERROR_STOP=1 "${CONN[@]}" "$@"; }
START_TIME="$(q -c "SELECT pg_postmaster_start_time()")"

log "loading fixtures and $ROWS rows"
q -f "$DWORK/fixtures.sql" >/dev/null
q >/dev/null <<SQL
CREATE EXTENSION pgstattuple;
CREATE TABLE soak_base AS
    SELECT slot, automerge_heads(base::automerge) AS heads, automerge_change_count(base::automerge) AS changes
    FROM soak_template;
ALTER TABLE soak_base ADD PRIMARY KEY (slot);
CREATE SEQUENCE soak_id MINVALUE 0 START 0;
CREATE TABLE soak_docs (
    id bigint PRIMARY KEY,
    slot int NOT NULL,
    doc automerge NOT NULL,
    data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX soak_docs_data ON soak_docs USING gin (data jsonb_path_ops) WITH (fastupdate = $FASTUPDATE);
CREATE TRIGGER soak_notify AFTER INSERT OR UPDATE OR DELETE ON soak_docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('soak_changed', 'id');
CREATE TABLE soak_cursor (
    id bigint, lane int, slot int NOT NULL, pos int NOT NULL DEFAULT 0, writes int NOT NULL DEFAULT 0,
    PRIMARY KEY (id, lane)
);
CREATE TABLE soak_mem (
    t timestamptz NOT NULL DEFAULT clock_timestamp(), pid int NOT NULL,
    allocated_bytes bigint, peak_allocated_bytes bigint, live_documents bigint, loads bigint,
    load_time float8, contexts_total bigint, contexts_used bigint
);
INSERT INTO soak_cursor (id, lane, slot) SELECT i, l, i % 20 FROM generate_series(0, $ROWS - 1) i, generate_series(0, 2) l;
INSERT INTO soak_docs (id, slot, doc) SELECT i, i % 20, t.base FROM generate_series(0, $ROWS - 1) i JOIN soak_template t ON t.slot = i % 20;
SELECT setval('soak_id', $ROWS - 1);
VACUUM ANALYZE;
SQL
TEMPLATE_BYTES="$(q -c "SELECT sum(length(base)) FROM soak_template")"
log "templates: $TEMPLATE_BYTES bytes compressed in total; sizes in $OUT/templates.txt"

# ---------------------------------------------------------------------------
# Listener: LISTEN, then a tick every second; awk counts the notifications
# (well-formed: a JSON payload with the table, the op and the key) and
# writes epoch,received,malformed,marker per tick.
# ---------------------------------------------------------------------------
log "starting the LISTEN session"
(
    echo "LISTEN soak_changed;"
    while [[ ! -e "$OUT/stop" ]]; do
        echo "SELECT 'tick', extract(epoch FROM now())::bigint;"
        sleep 1
    done
) | PGAPPNAME=soak_listener "$BINDIR/psql" -X -q -At "${CONN[@]}" 2>&1 \
    | awk '
        /^Asynchronous notification "soak_changed" with payload/ {
            if ($0 ~ /payload "marker"/) { marker = 1; next }
            n++
            if ($0 !~ /"table":"public\.soak_docs"/ || $0 !~ /"op":"(INSERT|UPDATE)"/ || $0 !~ /"key":\{"id":[0-9]+\}/) bad++
            next
        }
        /^tick\|/ { split($0, a, "|"); printf "%s,%d,%d,%d\n", a[2], n, bad, marker; fflush(); next }
        { printf "listener: %s\n", $0 > "/dev/stderr" }
    ' >"$OUT/listener.csv" 2>"$OUT/listener.err" &
LISTENER_PID=$!

# ---------------------------------------------------------------------------
# Sampler
# ---------------------------------------------------------------------------
echo "t,pid,rss_kb,anon_kb,file_kb,shmem_kb,title" >"$OUT/proc.csv"
echo "t,current,anon,file,shmem" >"$OUT/cgroup.csv"
echo "t,pid" >"$OUT/activity.csv"
echo "t,ms" >"$OUT/vacuum.csv"
DB_COLUMNS="t,max_id,rows,heap_bytes,toast_bytes,index_bytes,total_bytes,live_tup,dead_tup,autovacuums,autoanalyzes,toast_live_tup,toast_dead_tup,toast_autovacuums,heap_free_pct,heap_dead_pct,toast_free_pct,toast_dead_pct,wal_bytes,notify_queue,commits,rollbacks,deadlocks,small_bytes,medium_bytes,big_bytes,small_changes,medium_changes,big_changes,cursor_writes"
echo "$DB_COLUMNS" >"$OUT/db.csv"
DB_SAMPLE="
SELECT extract(epoch FROM now())::bigint, (SELECT last_value FROM soak_id), (SELECT count(*) FROM soak_docs),
    pg_relation_size('soak_docs'), pg_total_relation_size(c.reltoastrelid), pg_indexes_size('soak_docs'), pg_total_relation_size('soak_docs'),
    s.n_live_tup, s.n_dead_tup, s.autovacuum_count, s.autoanalyze_count,
    ts.n_live_tup, ts.n_dead_tup, ts.autovacuum_count,
    round(h.approx_free_percent::numeric, 2), round(h.dead_tuple_percent::numeric, 2),
    round(tt.approx_free_percent::numeric, 2), round(tt.dead_tuple_percent::numeric, 2),
    (SELECT wal_bytes FROM pg_stat_wal), round(pg_notification_queue_usage()::numeric, 6),
    d.xact_commit, d.xact_rollback, d.deadlocks,
    z.small_bytes, z.medium_bytes, z.big_bytes, z.small_changes, z.medium_changes, z.big_changes,
    (SELECT sum(writes) FROM soak_cursor)
FROM pg_class c
JOIN pg_stat_all_tables s ON s.relid = c.oid
JOIN pg_stat_all_tables ts ON ts.relid = c.reltoastrelid
JOIN pg_stat_database d ON d.datname = current_database(),
    LATERAL pgstattuple_approx(c.oid) h, LATERAL pgstattuple_approx(c.reltoastrelid) tt,
    LATERAL (SELECT
        round(avg(pg_column_size(doc)) FILTER (WHERE slot < 14)), round(avg(pg_column_size(doc)) FILTER (WHERE slot BETWEEN 14 AND 17)),
        round(avg(pg_column_size(doc)) FILTER (WHERE slot >= 18)),
        round(avg(automerge_change_count(doc)) FILTER (WHERE slot < 14)), round(avg(automerge_change_count(doc)) FILTER (WHERE slot BETWEEN 14 AND 17)),
        round(avg(automerge_change_count(doc)) FILTER (WHERE slot >= 18))
        FROM soak_docs) z(small_bytes, medium_bytes, big_bytes, small_changes, medium_changes, big_changes)
WHERE c.oid = 'soak_docs'::regclass"
sample_once() {
    local t; t="$(date +%s)"
    docker exec -i "$SERVER" sh -s <tests/soak/proc_sample.sh >"$DWORK/proc.txt" 2>/dev/null || return 0
    sed -n "/^cgroup,/!s/^/$t,/p" "$DWORK/proc.txt" >>"$OUT/proc.csv"
    sed -n "s/^cgroup,/$t,/p" "$DWORK/proc.txt" >>"$OUT/cgroup.csv"
    APP=soak_sampler q -F, -c "$DB_SAMPLE" >>"$OUT/db.csv" 2>>"$OUT/sampler.err" || true
    APP=soak_sampler q -F, -c "SELECT extract(epoch FROM now())::bigint, pid FROM pg_stat_activity WHERE application_name = 'pgbench'" \
        >>"$OUT/activity.csv" 2>>"$OUT/sampler.err" || true
}
sampler() {
    local next_vacuum=$(( $(date +%s) + VACUUM_EVERY ))
    while [[ ! -e "$OUT/stop" ]]; do
        sample_once
        if ((VACUUM_EVERY > 0)) && (( $(date +%s) >= next_vacuum )); then
            local t0; t0="$(date +%s%N)"
            APP=soak_sampler q -c "VACUUM (ANALYZE) soak_docs, soak_cursor" 2>>"$OUT/sampler.err" || true
            echo "$(date +%s),$(( ($(date +%s%N) - t0) / 1000000 ))" >>"$OUT/vacuum.csv"
            next_vacuum=$(( $(date +%s) + VACUUM_EVERY ))
        fi
        for _ in $(seq 1 "$SAMPLE"); do [[ -e "$OUT/stop" ]] && break; sleep 1; done
    done
}
sampler &
SAMPLER_PID=$!

# ---------------------------------------------------------------------------
# The load
# ---------------------------------------------------------------------------
PGB_ARGS=()
{
    echo "script_no,script,weight"
    i=0
    for s in "${SCRIPTS[@]}"; do
        if ((WEIGHT[$s] > 0)); then
            PGB_ARGS+=(-f "/soak/$s.sql@${WEIGHT[$s]}")
            echo "$i,$s,${WEIGHT[$s]}"
            i=$((i + 1))
        fi
    done
} >"$OUT/scripts.csv"
JOBS=$(( CLIENTS < $(nproc) ? CLIENTS : $(nproc) ))
log "pgbench: $CLIENTS clients for ${DURATION}s (progress in $OUT/pgbench.out)"
mkdir -p "$OUT/pgbench_log"
# The image's pgbench (Debian ships it with the server, not in
# postgresql-client-18), as the invoking user so the logs are theirs.
PGBENCH="$PROJECT-pgbench"
CONTAINERS+=("$PGBENCH")
set +e
docker run --name "$PGBENCH" --label "$LABEL" --network "container:$SERVER" --user "$(id -u):$(id -g)" \
    -e PGPASSWORD="$PG_PASSWORD" -e PGAPPNAME=pgbench \
    -v "$ROOT_DIR/tests/soak:/soak:ro" -v "$OUT/pgbench_log:/log" -w /log --entrypoint pgbench "$IMAGE" \
    -n -M prepared -h 127.0.0.1 -p 5432 -U "$PG_USER" \
    -c "$CLIENTS" -j "$JOBS" -T "$DURATION" -P "$(( SAMPLE > 10 ? 60 : 10 ))" --progress-timestamp \
    -l --log-prefix=/log/pgbench -r --failures-detailed --max-tries=5 \
    -D lane_changes="$LANE_CHANGES" -D checkpoint="$CHECKPOINT" \
    "${PGB_ARGS[@]}" app >"$OUT/pgbench.out" 2>&1
PGB_STATUS=$?
set -e
docker rm -f "$PGBENCH" >/dev/null 2>&1
log "pgbench exited with $PGB_STATUS"

# Wait for the listener: a marker notification sent now arrives after
# every notification of the run (delivery is in commit order).
q -c "NOTIFY soak_changed, 'marker'"
for _ in $(seq 1 120); do
    [[ "$(tail -1 "$OUT/listener.csv" | cut -d, -f4)" == 1 ]] && break
    sleep 1
done
sample_once
touch "$OUT/stop"
wait "$SAMPLER_PID" 2>/dev/null || true; SAMPLER_PID=
wait "$LISTENER_PID" 2>/dev/null || true; LISTENER_PID=

# ---------------------------------------------------------------------------
# Collect and check
# ---------------------------------------------------------------------------
log "collecting results"
q -c "\\copy (SELECT extract(epoch FROM t), pid, allocated_bytes, peak_allocated_bytes, live_documents, loads, load_time, contexts_total, contexts_used FROM soak_mem ORDER BY t) TO '$OUT/mem.csv' WITH (FORMAT csv, HEADER false)"
sed -i '1i t,pid,allocated_bytes,peak_allocated_bytes,live_documents,loads,load_time,contexts_total,contexts_used' "$OUT/mem.csv"
docker logs "$SERVER" >"$OUT/server.log" 2>&1

FAILS=()
# ok CMD...: 1 if CMD succeeds, else 0 ("not CMD..." negates).
ok() {
    if [[ $1 == not ]]; then shift; if "$@"; then echo 0; else echo 1; fi
    elif "$@"; then echo 1; else echo 0; fi
}
check() { # <what> <ok: 0/1> [detail]
    if [[ $2 == 1 ]]; then log "ok: $1"; else FAILS+=("$1${3:+: $3}"); echo "FAIL: $1${3:+: $3}" >&2; fi
}
check "pgbench exit status" "$(ok [ "$PGB_STATUS" = 0 ])" "$PGB_STATUS"
failed="$(sed -n 's/^number of failed transactions: \([0-9]*\).*/\1/p' "$OUT/pgbench.out")"
check "no failed transactions" $(( ${failed:-1} == 0 )) "${failed:-no summary}"
check "no aborted client" "$(ok not grep -qiE 'client [0-9]+ aborted|Run was aborted' "$OUT/pgbench.out")"
bad_log="$(grep -E 'ERROR:|FATAL:|PANIC:|TRAP:|panicked|terminated by signal|exited with exit code|memory allocation of' "$OUT/server.log" \
    | grep -vE 'FATAL:  terminating connection due to administrator command|FATAL:  the database system is starting up|"logical replication launcher" \(PID [0-9]+\) exited with exit code 1|ERROR:  canceling autovacuum task' \
    | head -5 || true)"
check "server log without errors or crashes" "$(ok [ -z "$bad_log" ])" "$bad_log"
check "postmaster not restarted" "$(ok [ "$(q -c "SELECT pg_postmaster_start_time()")" = "$START_TIME" ])"
state="$(docker inspect -f '{{.State.OOMKilled}} {{.RestartCount}}' "$SERVER")"
check "container not OOM-killed or restarted" "$(ok [ "$state" = "false 0" ])" "$state"

lost="$(q -c "SELECT count(*) FROM soak_docs d JOIN (SELECT id, sum(pos) AS p FROM soak_cursor GROUP BY id) c USING (id)
              JOIN soak_base b USING (slot) WHERE automerge_change_count(d.doc) <> b.changes + c.p")"
check "every row has exactly its lanes' changes" $(( lost == 0 )) "$lost rows differ"
# Every small and medium row, and one big row in ten (the conversion of a
# big document takes about a second).
stale="$(q -c "SELECT count(*) FROM soak_docs WHERE (slot < 18 OR id % 200 < 20) AND data IS DISTINCT FROM doc::jsonb")"
check "generated column equals doc::jsonb" $(( stale == 0 )) "$stale rows differ"
heads="$(q -c "SELECT max(cardinality(automerge_heads(doc))) FROM soak_docs")"
check "at most 3 heads per row (one per lane)" $(( heads <= 3 )) "$heads"

IFS=, read -r _ received malformed marker < <(tail -1 "$OUT/listener.csv")
initial_rows="$ROWS"
expected="$(q -c "SELECT (SELECT sum(writes) FROM soak_cursor) + (SELECT count(*) FROM soak_docs) - $initial_rows")"
check "listener got the marker" $(( ${marker:-0} == 1 ))
check "one notification per write" $(( ${received:-0} == expected )) "received ${received:-0}, expected $expected"
check "well-formed notifications" $(( ${malformed:-1} == 0 )) "${malformed:-?} malformed"
check "listener without errors" "$(ok [ ! -s "$OUT/listener.err" ])" "$(head -3 "$OUT/listener.err")"

read -r samples live max_alloc < <(q -F' ' -c "SELECT count(*), coalesce(max(live_documents), 0), coalesce(max(allocated_bytes), 0) FROM soak_mem")
check "memory samples taken" $(( samples > 0 )) "$samples"
check "no live document between statements" $(( live == 0 )) "max $live"
check "allocated_bytes between statements <= $ALLOC_SLACK" $(( max_alloc <= ALLOC_SLACK )) "max $max_alloc"
# Each backend's memory contexts: the high of the second half of its
# samples against the first's.
ctx="$(q -c "SELECT coalesce(max(g), 0) FROM (SELECT max(contexts_total) FILTER (WHERE second) - max(contexts_total) FILTER (WHERE NOT second) AS g
              FROM (SELECT pid, contexts_total, t > (SELECT min(t) + (max(t) - min(t)) / 2 FROM soak_mem) AS second FROM soak_mem) s
              GROUP BY pid) x")"
check "memory contexts high-water growth <= 1 MB" $(( ctx <= 1048576 )) "$ctx bytes"

log "final VACUUM and sizes"
APP=soak_sampler q -c "VACUUM (ANALYZE) soak_docs" 2>>"$OUT/sampler.err" || true
sample_once

log "report"
python3 tests/soak_report.py "$OUT" >"$OUT/report.txt" || fail "tests/soak_report.py"
cat "$OUT/report.txt"
if ((MAX_GROWTH > 0)); then
    growth="$(sed -n 's/^max backend anon RSS high-water growth: \([0-9.-]*\) MB.*/\1/p' "$OUT/report.txt")"
    check "backend anon RSS high-water growth <= $MAX_GROWTH MB" "$(awk -v g="${growth:-999}" -v m="$MAX_GROWTH" 'BEGIN { print (g <= m) }')" "${growth:-?} MB"
fi
check_log soak

((${#FAILS[@]} == 0)) || fail "${#FAILS[@]} check(s) failed: ${FAILS[*]}"
