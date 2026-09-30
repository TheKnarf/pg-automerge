# shellcheck shell=bash
# Shared helpers of the Docker image tests (tests/docker.sh,
# tests/docker_bench.sh). Sourced, not run.
#
# Sets ROOT_DIR (and cd's there), VERSION (Cargo.toml's), IMAGE, LABEL,
# PROJECT (a per-run name prefix) and DWORK (a scratch directory), and an
# EXIT trap that calls the script's on_exit function (if any) with the exit
# status, then removes every container and volume start() created (and
# the compose project PROJECT), and DWORK.
#
# Containers are named "$PROJECT-<name>" and labelled pg-automerge-test;
# every container gets the password $PG_PASSWORD, POSTGRES_DB=app, a named
# volume, and its port 5432 published on a free port of 127.0.0.1 (see
# host_port).
#
# Env: PG_AUTOMERGE_IMAGE (default pg-automerge:<Cargo.toml version>).

set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

log() { printf '==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

VERSION="$(sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')"
IMAGE="${PG_AUTOMERGE_IMAGE:-pg-automerge:$VERSION}"
LABEL=pg-automerge-test
PROJECT="pg-automerge-test-$$"
PG_PASSWORD="test"
DWORK="$(mktemp -d)"
CONTAINERS=()
VOLUMES=()

docker_cleanup() {
    local status=$?
    if declare -F on_exit >/dev/null; then on_exit "$status" || true; fi
    for c in "${CONTAINERS[@]}"; do docker rm -f -v "$c" >/dev/null 2>&1 || true; done
    for v in "${VOLUMES[@]}"; do docker volume rm -f "$v" >/dev/null 2>&1 || true; done
    PG_AUTOMERGE_IMAGE_TAG="${IMAGE#*:}" PG_AUTOMERGE_PORT=0 \
        docker compose -p "$PROJECT" down -v >/dev/null 2>&1 || true
    rm -rf "$DWORK"
    exit "$status"
}
trap docker_cleanup EXIT

docker image inspect "$IMAGE" >/dev/null 2>&1 || fail "image $IMAGE not found (run mise run docker-build)"

# start NAME VOLUME [docker run args...] [-- postgres args...]
# Starts container $PROJECT-NAME (use cname NAME for the full name) with
# volume $PROJECT-VOLUME (created and registered for removal if new).
start() {
    local name="$PROJECT-$1" vol="$PROJECT-$2"; shift 2
    local run_args=() pg_args=()
    while (($#)); do
        if [[ $1 == -- ]]; then shift; pg_args=("$@"); break; fi
        run_args+=("$1"); shift
    done
    docker volume inspect "$vol" >/dev/null 2>&1 || { docker volume create --label "$LABEL" "$vol" >/dev/null; VOLUMES+=("$vol"); }
    docker run -d --name "$name" --label "$LABEL" \
        -e POSTGRES_PASSWORD="$PG_PASSWORD" -e POSTGRES_DB=app \
        -p 127.0.0.1::5432 \
        -v "$vol:/var/lib/postgresql" "${run_args[@]}" "$IMAGE" postgres "${pg_args[@]}" >/dev/null
    CONTAINERS+=("$name")
}
cname() { printf '%s-%s' "$PROJECT" "$1"; }

# Wait until the real server accepts TCP connections (the entrypoint's
# temporary init server listens only on the socket); returns 1 if the
# container exits first.
wait_ready() {
    local name; name="$(cname "$1")"
    for _ in $(seq 1 300); do
        [[ "$(docker inspect -f '{{.State.Running}}' "$name")" == true ]] || return 1
        docker exec "$name" pg_isready -q -h 127.0.0.1 -U postgres && return 0
        sleep 0.2
    done
    fail "$name: not ready after 60s"
}
# start_ready NAME VOLUME ...: start, and fail with the log if it dies.
start_ready() {
    start "$@"
    wait_ready "$1" || { docker logs "$(cname "$1")" >&2; fail "$(cname "$1") exited during startup"; }
}

# The host port container NAME's 5432 is published on.
host_port() {
    local p; p="$(docker port "$(cname "$1")" 5432/tcp | sed -n 's/^127\.0\.0\.1://p' | head -1)"
    [[ -n "$p" ]] || fail "$(cname "$1"): port 5432 not published"
    echo "$p"
}

# Environment for the tests/lib.sh scripts (external mode) against NAME.
external_env() {
    printf '%s\n' PG_AUTOMERGE_TEST_HOST=127.0.0.1 "PG_AUTOMERGE_TEST_PORT=$(host_port "$1")" \
        PG_AUTOMERGE_TEST_USER=postgres "PG_AUTOMERGE_TEST_PASSWORD=$PG_PASSWORD"
}

# psql in container NAME against database DB as postgres (the local
# socket); extra args passed through.
psql_in() {
    local name; name="$(cname "$1")"; local db="$2"; shift 2
    docker exec -i "$name" psql -X -q -At -v ON_ERROR_STOP=1 -U postgres -d "$db" "$@"
}

expect() { # <what> <expected> <actual>
    [[ "$3" == "$2" ]] || fail "$1: expected '$2', got '$3'"
}
