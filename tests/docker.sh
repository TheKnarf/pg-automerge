#!/usr/bin/env bash
# Smoke test of the Docker image (run via `mise run docker-test`, which
# builds it first). Not part of `mise run test`: it needs Docker and a
# release build inside the image.
#
# Checks, each against a fresh container (all labelled pg-automerge-test and
# removed on exit, with their volumes):
#   - the image: OCI labels, the version file, no toolchain, lz4 support,
#     only the expected extension files;
#   - default init: the container turns healthy, pg_automerge is installed
#     in POSTGRES_DB (at the Cargo.toml version) and nowhere else, a merge
#     of two concurrent saves and jsonb reads work, settings passed with -c
#     apply, lz4 TOAST compression works on automerge columns;
#   - a restart on the same volume keeps the data and does not re-run init;
#     ALTER EXTENSION pg_automerge UPDATE is a no-op at the current version;
#   - PG_AUTOMERGE_CREATE_EXTENSION=0 skips the extension (and it can then
#     be created by hand into a schema); an invalid value fails init;
#   - compose.yaml: `docker compose up --wait` turns healthy, the extension
#     is there, and `down -v` removes everything.
#
# Env: PG_AUTOMERGE_IMAGE (default pg-automerge:<Cargo.toml version>).
set -euo pipefail
cd "$(dirname "$0")/.."

log() { printf '==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

VERSION="$(sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')"
IMAGE="${PG_AUTOMERGE_IMAGE:-pg-automerge:$VERSION}"
LABEL=pg-automerge-test
PROJECT="pg-automerge-test-$$"
CONTAINERS=()
VOLUMES=()

cleanup() {
    local status=$?
    for c in "${CONTAINERS[@]}"; do docker rm -f -v "$c" >/dev/null 2>&1 || true; done
    for v in "${VOLUMES[@]}"; do docker volume rm -f "$v" >/dev/null 2>&1 || true; done
    PG_AUTOMERGE_IMAGE_TAG="${IMAGE#*:}" PG_AUTOMERGE_PORT=0 \
        docker compose -p "$PROJECT" down -v >/dev/null 2>&1 || true
    if [[ $status == 0 ]]; then log "all docker checks passed"; else echo "docker test FAILED" >&2; fi
    exit "$status"
}
trap cleanup EXIT

docker image inspect "$IMAGE" >/dev/null 2>&1 || fail "image $IMAGE not found (run mise run docker-build)"

# Fixture documents: the regress examples' base shopping list, and alice's
# and bob's concurrent edits of it.
fixture() { sed -n "s/^\\\\set $1 '\\\\\\\\x\\([0-9a-f]*\\)'\$/\\1/p" tests/pg_regress/sql/automerge.sql; }
BASE="$(fixture base)"; ALICE="$(fixture alice)"; BOB="$(fixture bob)"
[[ -n "$BASE" && -n "$ALICE" && -n "$BOB" ]] || fail "fixtures not found in tests/pg_regress/sql/automerge.sql"

# start NAME VOLUME [docker run args...] [-- postgres args...]
start() {
    local name="$1" vol="$2"; shift 2
    local run_args=() pg_args=()
    while (($#)); do
        if [[ $1 == -- ]]; then shift; pg_args=("$@"); break; fi
        run_args+=("$1"); shift
    done
    docker volume inspect "$vol" >/dev/null 2>&1 || { docker volume create --label "$LABEL" "$vol" >/dev/null; VOLUMES+=("$vol"); }
    docker run -d --name "$name" --label "$LABEL" \
        -e POSTGRES_PASSWORD=test -e POSTGRES_DB=app \
        -v "$vol:/var/lib/postgresql" "${run_args[@]}" "$IMAGE" postgres "${pg_args[@]}" >/dev/null
    CONTAINERS+=("$name")
}

# Wait until the real server accepts TCP connections (the entrypoint's
# temporary init server listens only on the socket), or the container dies.
wait_ready() {
    local name="$1"
    for _ in $(seq 1 300); do
        [[ "$(docker inspect -f '{{.State.Running}}' "$name")" == true ]] || return 1
        docker exec "$name" pg_isready -q -h 127.0.0.1 -U postgres && return 0
        sleep 0.2
    done
    fail "$name: not ready after 60s"
}

# psql in container NAME against database DB; extra args passed through.
psql_in() {
    local name="$1" db="$2"; shift 2
    docker exec -i "$name" psql -X -q -At -v ON_ERROR_STOP=1 -U postgres -d "$db" "$@"
}

expect() { # <what> <expected> <actual>
    [[ "$3" == "$2" ]] || fail "$1: expected '$2', got '$3'"
}

# ---------------------------------------------------------------------------
log "image metadata and contents"
labels="$(docker image inspect -f '{{json .Config.Labels}}' "$IMAGE")"
for kv in "\"org.opencontainers.image.version\":\"$VERSION\"" '"org.opencontainers.image.licenses":"MIT"' '"org.opencontainers.image.title":"pg_automerge"'; do
    grep -qF "$kv" <<<"$labels" || fail "label $kv missing: $labels"
done
entrypoint="$(docker image inspect -f '{{json .Config.Entrypoint}} {{json .Config.Cmd}}' "$IMAGE")"
expect "entrypoint and cmd (the official image's)" '["docker-entrypoint.sh"] ["postgres"]' "$entrypoint"
contents="$(docker run --rm --label "$LABEL" --entrypoint bash "$IMAGE" -euc "
    [ \"\$(cat /usr/share/doc/pg_automerge/VERSION)\" = '$VERSION' ]
    grep -q 'MIT License' /usr/share/doc/pg_automerge/copyright
    for tool in cargo rustc rustup cargo-pgrx clang gcc; do
        if command -v \$tool >/dev/null; then echo \"toolchain in image: \$tool\" >&2; exit 1; fi
    done
    [ ! -e /opt/cargo ] && [ ! -e /opt/rustup ] && [ ! -e /src ]
    pg_config --configure | grep -q -- --with-lz4
    [ -x /docker-entrypoint-initdb.d/10-pg-automerge.sh ]
    cd /usr/share/postgresql/18/extension && ls pg_automerge*
    cd /usr/lib/postgresql/18/lib && ls pg_automerge*
" 2>&1)" || { echo "$contents" >&2; fail "image contents"; }
files="$(sort <<<"$contents" | tr '\n' ' ')"
expect "extension files" "pg_automerge--$VERSION.sql pg_automerge.control pg_automerge.so " "$files"

# ---------------------------------------------------------------------------
log "default init: healthy, extension in POSTGRES_DB only, settings from -c"
C1="$PROJECT-default"; V1="$PROJECT-data"
start "$C1" "$V1" -- -c default_toast_compression=lz4 -c pg_automerge.max_load_memory=512MB
wait_ready "$C1" || { docker logs "$C1" >&2; fail "$C1 exited during startup"; }
grep -q "pg_automerge $VERSION installed" <<<"$(docker logs "$C1" 2>&1)" || { docker logs "$C1" >&2; fail "init script did not report the install"; }
expect "extension version in app" "$VERSION" "$(psql_in "$C1" app -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"
expect "extension in postgres db" "" "$(psql_in "$C1" postgres -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"
expect "extension in template1" "" "$(psql_in "$C1" template1 -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"
expect "server is a release build" "off" "$(psql_in "$C1" app -c 'SHOW debug_assertions')"
expect "server version" "18" "$(psql_in "$C1" app -c "SELECT current_setting('server_version_num')::int / 10000")"

log "merge and jsonb reads"
out="$(psql_in "$C1" app -v base="\\x$BASE" -v alice="\\x$ALICE" -v bob="\\x$BOB" <<'SQL'
SHOW pg_automerge.max_load_memory;
SHOW default_toast_compression;
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
INSERT INTO docs VALUES (1, :'base'::bytea);
UPDATE docs SET doc = merge(doc, :'alice'::automerge) WHERE id = 1;
UPDATE docs SET doc = merge(doc, :'bob'::bytea) WHERE id = 1;
SELECT doc->>'title', jsonb_path_query_array(doc, '$.items[*].name'),
       doc @> '{"status": "open"}', cardinality(automerge_heads(doc))
FROM docs WHERE id = 1;
-- lz4 TOAST compression: the server supports it, and automerge columns
-- (storage extended) accept it.
CREATE TABLE big (doc automerge COMPRESSION lz4, pad text);
INSERT INTO big SELECT merge_agg(d), repeat('x', 100000)
FROM (SELECT :'base'::automerge d UNION ALL SELECT :'alice' UNION ALL SELECT :'bob') s;
SELECT pg_column_compression(pad), doc->>'title' FROM big;
SELECT attcompression FROM pg_attribute WHERE attrelid = 'big'::regclass AND attname = 'doc';
SQL
)"
expect "merge/jsonb output" "$(printf '%s\n' 512MB lz4 'Groceries for Sunday|["milk", "eggs"]|t|2' 'lz4|Groceries for Sunday' l)" "$out"

# ---------------------------------------------------------------------------
log "restart on the same volume: data kept, init not re-run, UPDATE is a no-op"
docker rm -f "$C1" >/dev/null
C2="$PROJECT-restart"
start "$C2" "$V1"
wait_ready "$C2" || { docker logs "$C2" >&2; fail "$C2 exited during startup"; }
grep -q 'Skipping initialization' <<<"$(docker logs "$C2" 2>&1)" || { docker logs "$C2" >&2; fail "restart re-ran init"; }
expect "data after restart" "Groceries for Sunday" "$(psql_in "$C2" app -c "SELECT doc->>'title' FROM docs WHERE id = 1")"
psql_in "$C2" app -c 'ALTER EXTENSION pg_automerge UPDATE'
expect "version after UPDATE" "$VERSION" "$(psql_in "$C2" app -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"

# ---------------------------------------------------------------------------
log "PG_AUTOMERGE_CREATE_EXTENSION=0 skips it; CREATE EXTENSION .. SCHEMA by hand works"
C3="$PROJECT-skip"
start "$C3" "$PROJECT-skip" -e PG_AUTOMERGE_CREATE_EXTENSION=0
wait_ready "$C3" || { docker logs "$C3" >&2; fail "$C3 exited during startup"; }
expect "extension with =0" "" "$(psql_in "$C3" app -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"
expect "manual install into a schema" "automerge|{}" "$(psql_in "$C3" app -c 'CREATE SCHEMA automerge' -c 'CREATE EXTENSION pg_automerge SCHEMA automerge' \
    -c "SELECT n.nspname, automerge.automerge_to_jsonb(''::bytea::automerge.automerge) FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace WHERE extname = 'pg_automerge'")"

log "PG_AUTOMERGE_CREATE_EXTENSION=yes fails initialization"
C4="$PROJECT-bad"
start "$C4" "$PROJECT-bad" -e PG_AUTOMERGE_CREATE_EXTENSION=yes
if wait_ready "$C4"; then fail "$C4 started with an invalid PG_AUTOMERGE_CREATE_EXTENSION"; fi
grep -qF "PG_AUTOMERGE_CREATE_EXTENSION must be 0 or 1, got 'yes'" <<<"$(docker logs "$C4" 2>&1)" || { docker logs "$C4" >&2; fail "no error message for the invalid value"; }

# ---------------------------------------------------------------------------
log "compose.yaml: up --wait, extension present, down -v"
export PG_AUTOMERGE_IMAGE_TAG="${IMAGE#*:}" PG_AUTOMERGE_PORT=0
[[ "${IMAGE%%:*}" == pg-automerge ]] || fail "compose.yaml uses the pg-automerge repository, not ${IMAGE%%:*}"
docker compose -p "$PROJECT" up -d --wait --wait-timeout 120 >/dev/null 2>&1 \
    || { docker compose -p "$PROJECT" logs >&2; fail "compose up"; }
cid="$(docker compose -p "$PROJECT" ps -q postgres)"
expect "compose: extension" "$VERSION|lz4|1GB" "$(docker exec "$cid" psql -X -At -U postgres -d app \
    -c "SELECT extversion, current_setting('default_toast_compression'), current_setting('pg_automerge.max_load_memory') FROM pg_extension WHERE extname = 'pg_automerge'")"
port="$(docker compose -p "$PROJECT" port postgres 5432)"
[[ "$port" == 127.0.0.1:* ]] || fail "compose publishes on $port, not 127.0.0.1"
docker compose -p "$PROJECT" down -v >/dev/null 2>&1
[[ -z "$(docker volume ls -q --filter "label=com.docker.compose.project=$PROJECT")" ]] || fail "compose down -v left a volume"
