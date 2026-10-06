#!/usr/bin/env bash
# End-to-end test of the CloudNativePG extension image with the real
# operator: a throwaway kind cluster (pinned kind and node image; image
# volumes are GA from Kubernetes 1.36), the CNPG operator at a pinned
# release (manifest checked against its sha256), and a one-instance Cluster
# on CNPG's PostgreSQL 18 operand image (the digest tests/cnpg_smoke.sh
# pins) with the extension in spec.postgresql.extensions. Run by
# `mise run cnpg-e2e` and the cnpg-e2e CI job; not part of `mise run ci` or
# docker-test (minutes, and it needs kind).
#
# It walks the life cycle docs/src/pages/guide/cloudnativepg.mdx describes:
#   1. the Cluster mounts an "old" extension image: the image under test
#      with the previous release's SQL (sql/snapshots/pg_automerge--OLD.sql,
#      the control file's default_version OLD; the same library), standing
#      in for that release's image, which was never built;
#   2. a Database resource with extensions: [{name: pg_automerge, version:
#      OLD}]: the operator runs CREATE EXTENSION; the settings the operator
#      writes (SHOW extension_control_path, dynamic_library_path), the
#      library mapped from the image volume, a spec.postgresql.parameters
#      setting of the extension, the app role refused CREATE EXTENSION,
#      data written;
#   3. the Cluster's extension image changed to the image under test: the
#      pod rolls (a new pod, its image volume the new reference, the
#      Cluster's status.pgDataImageInfo too), the data survives, the
#      catalog is still OLD with the update path to the new version found;
#   4. the Database's version bumped: the operator runs ALTER EXTENSION
#      UPDATE TO it; then merge, jsonb reads with a GIN index,
#      automerge_spans, automerge_memory_usage() on the regress fixtures;
#   5. removal: ensure: absent drops the extension, deleting the
#      extensions entry rolls the pod without the image volume;
#   and the server log has no crash.
#
# Images reach the node with `kind load image-archive` (no registry): the
# extensions entries say pullPolicy: Never. The node pulls the operator and
# operand images from ghcr.io itself.
#
# Too many open files: kind's node runs as root of the Docker daemon, and
# every inotify instance in it counts against that user's
# fs.inotify.max_user_instances (kind's known issues ask for 512; Debian's
# default is 128). Where other root processes already hold most of them
# (a k3s on the host, say), kube-proxy and the CNPG operator crash-loop with
# "fsnotify watcher init: too many open files". Either raise the limit
# (sudo sysctl -w fs.inotify.max_user_instances=512), or point
# CNPG_E2E_DOCKER_HOST at a rootless Docker daemon of your own user, whose
# containers count against your uid instead (the images are built by the
# default daemon and moved over with docker save):
#   dockerd-rootless.sh --data-root DIR -H unix://$XDG_RUNTIME_DIR/e2e/docker.sock &
#   CNPG_E2E_DOCKER_HOST=unix://$XDG_RUNTIME_DIR/e2e/docker.sock mise run cnpg-e2e
#
# Env: PG_AUTOMERGE_CNPG_IMAGE (default pg-automerge-cnpg:<version>-18-trixie,
# in the default Docker daemon), CNPG_E2E_DOCKER_HOST (the Docker daemon
# kind runs on; default the default), CNPG_E2E_NODE_IMAGE (default the
# pinned node below), CNPG_E2E_KEEP=1 (leave the kind cluster for a look;
# `kind delete cluster --name <name>` removes it; its kubeconfig is
# printed). The kind cluster and the image tags made here are named
# pg-automerge-test-<pid>, labelled pg-automerge-test, and removed on exit.

VERSION="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')"
PG_AUTOMERGE_IMAGE="${PG_AUTOMERGE_CNPG_IMAGE:-pg-automerge-cnpg:$VERSION-18-trixie}"
# shellcheck source=tests/docker_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/docker_lib.sh"

# kind v0.33.0's node image for Kubernetes 1.36 (CNPG 1.30 supports
# 1.34-1.36; ImageVolume is GA, on by default, from 1.36). From the kind
# release notes: https://github.com/kubernetes-sigs/kind/releases/tag/v0.33.0
NODE_IMAGE="${CNPG_E2E_NODE_IMAGE:-kindest/node:v1.36.4@sha256:099e049362a1526b2db71494e1947aae99bd16290d7c895f2b7ea312e3cbfaed}"
# The operator's release manifest and its sha256. To update: the
# cnpg-<version>.yaml asset of https://github.com/cloudnative-pg/cloudnative-pg/releases
CNPG_VERSION=1.30.1
CNPG_MANIFEST_SHA256=37237f145d8138256ea25ae830f87759255665ff08f8d552fdd8224a5ec032fb
# CNPG's PostgreSQL 18 operand of the extension image's Debian release:
# the one tests/cnpg_smoke.sh pins (tests/check_ci.sh keeps them equal).
OPERAND=ghcr.io/cloudnative-pg/postgresql:18-minimal-trixie@sha256:37ade18dbdddba430858c72725aceeec66f33fa5333e82ea1df4942f6c1c83a3

KIND_CLUSTER="$PROJECT"
KIND_DOCKER_HOST="${CNPG_E2E_DOCKER_HOST:-}"
export KUBECONFIG="$DWORK/kubeconfig"   # never the user's own
NS=default
PGC=automerge                          # the Cluster
EXT=pg-automerge                       # its extensions[].name: /extensions/pg-automerge
KIND_CREATED=

for tool in kind kubectl jq; do command -v "$tool" >/dev/null || fail "$tool not found (mise install)"; done
# kind and the docker CLI calls about its node use the kind daemon.
kdocker() { if [[ -n "$KIND_DOCKER_HOST" ]]; then DOCKER_HOST="$KIND_DOCKER_HOST" "$@"; else "$@"; fi; }
k() { kubectl "$@"; }

diagnostics() {
    echo "---- diagnostics" >&2
    k get nodes,pods -A -o wide >&2 2>&1 || true
    k get events -A --sort-by=.lastTimestamp 2>&1 | tail -30 >&2 || true
    k -n cnpg-system logs deploy/cnpg-controller-manager --tail=40 >&2 2>&1 || true
    k -n "$NS" get cluster "$PGC" -o json 2>/dev/null | jq '.status | {phase, phaseReason, pgDataImageInfo}' >&2 || true
    k -n "$NS" logs "$PGC-1" -c postgres --tail=40 >&2 2>&1 || true
    if inotify_starved; then inotify_help; fi
}

# The node is out of inotify instances: a system pod's log (this run's or
# the previous, crashed one's) says "too many open files".
inotify_starved() {
    local ns sel pod logs
    while read -r ns sel; do
        for pod in $(k -n "$ns" get pods -l "$sel" -o name 2>/dev/null); do
            # (Not a pipe into grep -q: with pipefail, an early exit of grep
            # or a missing previous log would hide the match.)
            logs="$(k -n "$ns" logs "$pod" --tail=50 2>&1; k -n "$ns" logs "$pod" --previous --tail=50 2>&1)" || true
            [[ "$logs" == *"too many open files"* ]] && return 0
        done
    done <<'PODS'
kube-system k8s-app=kube-proxy
cnpg-system app.kubernetes.io/name=cloudnative-pg
local-path-storage app=local-path-provisioner
PODS
    return 1
}
inotify_help() {
    cat >&2 <<EOF
---- "too many open files": the kind node ran out of inotify instances
(fs.inotify.max_user_instances is $(sysctl -n fs.inotify.max_user_instances 2>/dev/null || echo '?'), counted per user
across the host). Raise it (sudo sysctl -w fs.inotify.max_user_instances=512),
or run kind on a rootless Docker daemon of your own with CNPG_E2E_DOCKER_HOST;
see the top of $0.
EOF
}

on_exit() {
    if [[ $1 != 0 && -n "$KIND_CREATED" ]]; then diagnostics; fi
    if [[ -n "$KIND_CREATED" ]]; then
        if [[ "${CNPG_E2E_KEEP:-}" == 1 ]]; then
            cp "$KUBECONFIG" "$ROOT_DIR/target/$KIND_CLUSTER.kubeconfig" 2>/dev/null \
                && echo "kept kind cluster $KIND_CLUSTER; KUBECONFIG=$ROOT_DIR/target/$KIND_CLUSTER.kubeconfig" >&2
        else
            kdocker kind delete cluster --name "$KIND_CLUSTER" >/dev/null 2>&1 || true
        fi
    fi
    if [[ $1 == 0 ]]; then log "all CNPG end-to-end checks passed (${SECONDS}s)"; else echo "CNPG end-to-end test FAILED" >&2; fi
}

# wait_until WHAT SECONDS CMD...: retry CMD every 2 s until it succeeds.
wait_until() {
    local what="$1" secs="$2"; shift 2
    local end=$((SECONDS + secs))
    until "$@" >/dev/null 2>&1; do
        ((SECONDS < end)) || fail "timed out after ${secs}s waiting for $what"
        sleep 2
    done
}

# The reference containerd knows a `kind load`ed Docker image by.
node_ref() {
    local first="${1%%/*}"
    if [[ "$1" != */* ]]; then echo "docker.io/library/$1"
    elif [[ "$first" == *.* || "$first" == *:* || "$first" == localhost ]]; then echo "$1"
    else echo "docker.io/$1"; fi
}

primary() { k -n "$NS" get cluster "$PGC" -o jsonpath='{.status.currentPrimary}'; }
# psql as the postgres superuser in the primary's postgres container.
pg() { k -n "$NS" exec -i "$(primary)" -c postgres -- psql -X -q -v ON_ERROR_STOP=1 -At -d app "$@"; }
extversion() { pg -c "SELECT coalesce((SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'), 'none')"; }
ext_is() { [[ "$(extversion)" == "$1" ]]; }
db_applied() { [[ "$(k -n "$NS" get database "$PGC-app" -o jsonpath='{.status.applied}')" == true ]]; }
# apply NAME GENERATOR ARGS...: write the generator's YAML to $DWORK/NAME.yaml
# and kubectl apply it.
apply() { local f="$DWORK/$1.yaml"; shift; "$@" >"$f"; k apply -f "$f" >/dev/null; }

cluster_yaml() { # <extension image reference, or empty for none>
    cat <<EOF
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: $PGC
  namespace: $NS
spec:
  imageName: $OPERAND
  instances: 1
  storage:
    size: 1Gi
  postgresql:
    parameters:
      pg_automerge.max_load_memory: "1GB"
EOF
    if [[ -n "$1" ]]; then
        cat <<EOF
    extensions:
      - name: $EXT
        image:
          reference: $1
          pullPolicy: Never
EOF
    fi
}

database_yaml() { # <version> <ensure>
    cat <<EOF
apiVersion: postgresql.cnpg.io/v1
kind: Database
metadata:
  name: $PGC-app
  namespace: $NS
spec:
  name: app
  owner: app
  cluster:
    name: $PGC
  extensions:
    - name: pg_automerge
      version: "$1"
      ensure: $2
EOF
}

pod_uid() { k -n "$NS" get pod "$PGC-1" -o jsonpath='{.metadata.uid}'; }
pod_ext_ref() { k -n "$NS" get pod "$PGC-1" -o json | jq -r '[.spec.volumes[] | select(.image) | .image.reference] | join(",")'; }
status_ext_ref() { k -n "$NS" get cluster "$PGC" -o json | jq -r '[.status.pgDataImageInfo.extensions // [] | .[].image.reference] | join(",")'; }
# rolled OLD_UID REF: a new pod, Ready, with REF as its image volume (empty:
# none), and the Cluster healthy with REF in its status.
rolled() {
    local uid; uid="$(pod_uid)" || return 1
    [[ -n "$uid" && "$uid" != "$1" ]] || return 1
    [[ "$(pod_ext_ref)" == "$2" && "$(status_ext_ref)" == "$2" ]] || return 1
    [[ "$(k -n "$NS" get pod "$PGC-1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" == True ]] || return 1
    [[ "$(k -n "$NS" get cluster "$PGC" -o jsonpath='{.status.phase}')" == "Cluster in healthy state" ]]
}

# ---------------------------------------------------------------------------
# The previous release: the version the upgrade script to VERSION starts at.
old_script=(sql/pg_automerge--*--"$VERSION".sql)
[[ -e "${old_script[0]}" ]] || fail "no upgrade script to $VERSION in sql/"
OLD="$(basename "${old_script[0]}" .sql)"; OLD="${OLD#pg_automerge--}"; OLD="${OLD%--*}"
[[ -f "sql/snapshots/pg_automerge--$OLD.sql" ]] || fail "no snapshot sql/snapshots/pg_automerge--$OLD.sql"
DEBIAN="$(docker image inspect -f '{{index .Config.Labels "io.cloudnativepg.image.base.os"}}' "$IMAGE")"
[[ "$DEBIAN" == trixie ]] || fail "$IMAGE is for Debian '$DEBIAN'; the pinned operand is trixie"

log "the $OLD extension image: $IMAGE's library and license, $OLD's SQL"
files="$(cname files)"
docker create --label "$LABEL" --name "$files" "$IMAGE" /none >/dev/null; CONTAINERS+=("$files")
mkdir -p "$DWORK/old/share/extension"
docker cp "$files:/lib" "$DWORK/old/" >/dev/null
docker cp "$files:/licenses" "$DWORK/old/" >/dev/null
docker cp "$files:/share/extension/pg_automerge.control" "$DWORK/old/share/extension/" >/dev/null
docker rm "$files" >/dev/null
sed -i "s/^default_version = .*/default_version = '$OLD'/" "$DWORK/old/share/extension/pg_automerge.control"
cp "sql/snapshots/pg_automerge--$OLD.sql" "$DWORK/old/share/extension/"
for f in sql/pg_automerge--*--*.sql; do
    [[ "$f" == "${old_script[0]}" ]] || cp "$f" "$DWORK/old/share/extension/"
done
chmod -R a+rX,go-w "$DWORK/old"
cat >"$DWORK/old.Dockerfile" <<EOF
FROM scratch
COPY old/ /
LABEL org.opencontainers.image.version="$OLD" io.cloudnativepg.image.sql.version="$OLD" io.cloudnativepg.image.base.os="$DEBIAN"
USER 65532:65532
EOF
OLD_IMAGE="pg-automerge-cnpg:$OLD-$PROJECT-18-$DEBIAN"
docker buildx build -q --load --label "$LABEL" -f "$DWORK/old.Dockerfile" -t "$OLD_IMAGE" "$DWORK" >/dev/null
IMAGES+=("$OLD_IMAGE")
NEW_REF="$(node_ref "$IMAGE")"; OLD_REF="$(node_ref "$OLD_IMAGE")"

# ---------------------------------------------------------------------------
log "kind cluster $KIND_CLUSTER on $NODE_IMAGE${KIND_DOCKER_HOST:+ (Docker at $KIND_DOCKER_HOST)}"
if ! kdocker docker info -f '{{.SecurityOptions}}' | grep -q rootless; then
    limit="$(sysctl -n fs.inotify.max_user_instances 2>/dev/null || echo 0)"
    ((limit >= 512)) || log "note: fs.inotify.max_user_instances=$limit (kind asks for 512); on \"too many open files\" see the top of $0"
fi
cat >"$DWORK/kind.yaml" <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
EOF
KIND_CREATED=1
kdocker kind create cluster --name "$KIND_CLUSTER" --image "$NODE_IMAGE" --config "$DWORK/kind.yaml" \
    --kubeconfig "$KUBECONFIG" --wait 180s >/dev/null 2>"$DWORK/kind.log" || { cat "$DWORK/kind.log" >&2; fail "kind create cluster"; }
k version -o json | jq -r '"server " + .serverVersion.gitVersion'

log "load $IMAGE and $OLD_IMAGE into the node"
docker image save "$IMAGE" "$OLD_IMAGE" -o "$DWORK/images.tar"
kdocker kind load image-archive "$DWORK/images.tar" --name "$KIND_CLUSTER" >/dev/null
rm -f "$DWORK/images.tar"

log "CNPG operator $CNPG_VERSION"
curl -fsSL --retry 3 -o "$DWORK/cnpg.yaml" \
    "https://github.com/cloudnative-pg/cloudnative-pg/releases/download/v$CNPG_VERSION/cnpg-$CNPG_VERSION.yaml"
echo "$CNPG_MANIFEST_SHA256  $DWORK/cnpg.yaml" | sha256sum -c --quiet - || fail "cnpg-$CNPG_VERSION.yaml: sha256 mismatch"
k apply --server-side -f "$DWORK/cnpg.yaml" >/dev/null
# Fails early when the node is starved of inotify instances (see the top).
end=$((SECONDS + 300))
until [[ "$(k -n cnpg-system get deploy/cnpg-controller-manager \
    -o jsonpath='{.status.conditions[?(@.type=="Available")].status}')" == True ]]; do
    if inotify_starved; then fail "the node cannot run the CNPG operator: too many open files (help below)"; fi
    ((SECONDS < end)) || fail "the CNPG operator did not become available"
    sleep 5
done

log "Cluster $PGC with the $OLD extension image"
# The webhook may take a moment after the deployment is available.
wait_until "the CNPG webhook to accept the Cluster" 120 apply cluster cluster_yaml "$OLD_REF"
wait_until "Cluster $PGC Ready" 600 k -n "$NS" wait --for=condition=Ready "cluster/$PGC" --timeout=10s
expect "pod image volume" "$OLD_REF" "$(pod_ext_ref)"
expect "status.pgDataImageInfo.extensions" "$OLD_REF" "$(status_ext_ref)"
# The operand ships no pg_automerge of its own.
k -n "$NS" exec "$(primary)" -c postgres -- bash -c \
    '! ls /usr/lib/postgresql/18/lib/pg_automerge* /usr/share/postgresql/18/extension/pg_automerge* 2>/dev/null' \
    || fail "the operand has its own pg_automerge"
expect "the settings CNPG writes" \
    "$(printf '%s\n' "\$system:/extensions/$EXT/share" "\$libdir:/extensions/$EXT/lib" 1GB "$OLD|t" none)" \
    "$(pg -c 'SHOW extension_control_path' -c 'SHOW dynamic_library_path' -c 'SHOW pg_automerge.max_load_memory' \
        -c "SELECT default_version, installed_version IS NULL FROM pg_available_extensions WHERE name = 'pg_automerge'" \
        -c "SELECT coalesce((SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'), 'none')")"

log "the app role cannot CREATE EXTENSION (not trusted)"
if out="$(pg -c 'SET ROLE app' -c 'CREATE EXTENSION pg_automerge' 2>&1)"; then fail "the app role created the extension"; fi
grep -q 'permission denied to create extension' <<<"$out" || fail "app role: $out"

log "Database resource: extension pg_automerge $OLD (the operator runs CREATE EXTENSION)"
wait_until "the CNPG webhook to accept the Database" 60 apply database database_yaml "$OLD" present
wait_until "pg_automerge $OLD in database app" 180 ext_is "$OLD"
wait_until "the Database applied" 60 db_applied

BASE="$(fixture base)"; ALICE="$(fixture alice)"; BOB="$(fixture bob)"; NOTE="$(fixture note)"
[[ -n "$BASE" && -n "$ALICE" && -n "$BOB" && -n "$NOTE" ]] || fail "fixtures not found in tests/pg_regress/sql/automerge.sql"
out="$(pg -v base="\\x$BASE" -v alice="\\x$ALICE" <<'SQL'
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
CREATE INDEX docs_gin ON docs USING gin ((doc::jsonb) jsonb_path_ops);
INSERT INTO docs VALUES (1, :'base'::bytea);
UPDATE docs SET doc = merge(doc, :'alice'::automerge) WHERE id = 1;
SELECT doc->>'title', cardinality(automerge_heads(doc)) FROM docs;
-- The backend mapped the library from the image volume.
SELECT position('/extensions/pg-automerge/lib/pg_automerge.so' IN pg_read_file('/proc/self/maps')) > 0;
SQL
)" || fail "SQL at $OLD: $out"
expect "SQL at $OLD" "$(printf '%s\n' 'Groceries for Sunday|1' t)" "$out"

# ---------------------------------------------------------------------------
log "change the Cluster's extension image to $IMAGE: the pod rolls"
uid="$(pod_uid)"
apply cluster cluster_yaml "$NEW_REF"
wait_until "the pod to roll onto $NEW_REF" 600 rolled "$uid" "$NEW_REF"
expect "after the roll: the data, the catalog still $OLD, the update path found" \
    "$(printf '%s\n' 'Groceries for Sunday' "$OLD" "$VERSION|t" t)" \
    "$(pg -c "SELECT doc->>'title' FROM docs" -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'" \
        -c "SELECT default_version, installed_version = '$OLD' FROM pg_available_extensions WHERE name = 'pg_automerge'" \
        -c "SELECT path IS NOT NULL FROM pg_extension_update_paths('pg_automerge') WHERE source = '$OLD' AND target = '$VERSION'")"

log "bump the Database's extension version to $VERSION (the operator runs ALTER EXTENSION UPDATE)"
apply database database_yaml "$VERSION" present
wait_until "pg_automerge $VERSION in database app" 180 ext_is "$VERSION"

out="$(pg -v bob="\\x$BOB" -v note="\\x$NOTE" <<'SQL'
SHOW pg_automerge.max_load_memory;
UPDATE docs SET doc = merge(doc, :'bob'::bytea) WHERE id = 1;
SELECT doc->>'title', jsonb_path_query_array(doc, '$.items[*].name'), doc #>> '{items,1,name}',
       cardinality(automerge_heads(doc))
FROM docs;
SET enable_seqscan = off;
SELECT id FROM docs WHERE doc @> '{"items": [{"name": "eggs"}]}';
SELECT string_agg(s->>'value', '' ORDER BY ord) FILTER (WHERE s->>'type' = 'text')
FROM jsonb_array_elements(automerge_spans(:'note'::automerge, '{body}')) WITH ORDINALITY AS t(s, ord);
-- This session loaded documents and holds none after the statements.
SELECT live_documents = 0, loads > 0 FROM automerge_memory_usage();
SELECT position('/extensions/pg-automerge/lib/pg_automerge.so' IN pg_read_file('/proc/self/maps')) > 0;
SET client_min_messages = warning;
ALTER EXTENSION pg_automerge UPDATE;
SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge';
SQL
)" || fail "SQL at $VERSION: $out"
expect "SQL at $VERSION" "$(printf '%s\n' 1GB 'Groceries for Sunday|["milk", "eggs"]|eggs|2' 1 \
    'Shopping tipsBuy fresh milk on Sunday.' 't|t' t "$VERSION")" "$out"

# ---------------------------------------------------------------------------
log "removal: ensure: absent (DROP EXTENSION), then no extensions entry (the pod rolls)"
pg -c 'DROP TABLE docs' >/dev/null
apply database database_yaml "$VERSION" absent
wait_until "pg_automerge dropped" 180 ext_is none
# The server log of the pod that ran everything since the roll.
bad="$(k -n "$NS" logs "$PGC-1" -c postgres 2>&1 \
    | grep -E 'TRAP:|PANIC:|failed assertion|panicked|core dumped|terminated by signal|server process.* exited with exit code|memory allocation of' || true)"
[[ -z "$bad" ]] || fail "server log:
$bad"
uid="$(pod_uid)"
apply cluster cluster_yaml ""
wait_until "the pod to roll without the extension image" 600 rolled "$uid" ""
expect "after removal" "$(printf '%s\n' "\$system" "\$libdir" f)" \
    "$(pg -c 'SHOW extension_control_path' -c 'SHOW dynamic_library_path' \
        -c "SELECT EXISTS (SELECT FROM pg_available_extensions WHERE name = 'pg_automerge')")"
