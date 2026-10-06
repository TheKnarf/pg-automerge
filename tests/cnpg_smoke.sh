#!/usr/bin/env bash
# Smoke test of the CloudNativePG extension image (the cnpg-extension target
# of docker/Dockerfile) without Kubernetes: CNPG's own PostgreSQL 18 operand
# image (pinned below) with the extension image mounted read-only at
# /extensions/pg-automerge, and the two settings CNPG's operator writes for
# a Cluster's spec.postgresql.extensions entry named pg-automerge:
#
#   extension_control_path = '$system:/extensions/pg-automerge/share'
#   dynamic_library_path   = '$libdir:/extensions/pg-automerge/lib'
#
# (cloudnative-pg pkg/postgres/configuration.go). Run by `mise run
# cnpg-smoke` and by tests/docker.sh. Checks:
#   - the image: labels (OCI and io.cloudnativepg.*), from scratch with no
#     entrypoint, and exactly the files of CNPG's layout (/lib, the control
#     file and SQL scripts in /share/extension, the license), root-owned,
#     readable by everyone;
#   - the operand has no pg_automerge of its own; the mounted one is
#     available at the crate version with its upgrade path from 0.1.0;
#   - CREATE EXTENSION as the superuser (a plain role is refused: not
#     trusted), the backend maps the library from the image volume, a
#     setting given in postgresql.conf before the library loads applies
#     (as spec.postgresql.parameters would), merge, jsonb reads and a GIN
#     index, automerge_spans, ALTER EXTENSION UPDATE a no-op, DROP;
#   - the mount is read-only, and the server log has no crash.
#
# The mount is a Docker image mount (`--mount type=image`, Docker 28+), the
# same read-only image volume CNPG asks the kubelet for; on an older Docker
# the image's files are copied out (docker create + docker cp) and
# bind-mounted read-only instead. CNPG_SMOKE_MOUNT=image or copy forces one.
#
# Env: PG_AUTOMERGE_CNPG_IMAGE (default pg-automerge-cnpg:<version>-18-trixie),
# CNPG_OPERAND_IMAGE (default the pinned minimal operand of the extension
# image's Debian release, below), CNPG_SMOKE_MOUNT (auto, image or copy).
# Containers are labelled pg-automerge-test and removed on exit.

# Read before docker_lib.sh, which takes PG_AUTOMERGE_IMAGE as the image
# under test.
VERSION="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')"
PG_AUTOMERGE_IMAGE="${PG_AUTOMERGE_CNPG_IMAGE:-pg-automerge-cnpg:$VERSION-18-trixie}"
# shellcheck source=tests/docker_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/docker_lib.sh"

# CNPG's PostgreSQL 18 operand images (postgres-containers), by Debian
# release, pinned by the digest of the multi-arch index. To update:
#   docker buildx imagetools inspect ghcr.io/cloudnative-pg/postgresql:18-minimal-trixie
# (The minimal and standard variants share the base, so the minimal one
# stands for both. A bookworm operand cannot load the trixie build: its
# glibc 2.36 lacks the GLIBC_2.38 symbols the library uses, and
# CREATE EXTENSION fails; CNPG_OPERAND_IMAGE=<bookworm operand> shows it.)
declare -A OPERANDS=(
    [trixie]=ghcr.io/cloudnative-pg/postgresql:18-minimal-trixie@sha256:37ade18dbdddba430858c72725aceeec66f33fa5333e82ea1df4942f6c1c83a3
)
MOUNT=/extensions/pg-automerge   # what CNPG mounts for extensions[].name: pg-automerge

on_exit() {
    if [[ $1 == 0 ]]; then log "all CNPG smoke checks passed"; else echo "CNPG smoke test FAILED" >&2; fi
}

# ---------------------------------------------------------------------------
log "extension image $IMAGE: labels and layout"
label() { docker image inspect -f "{{index .Config.Labels \"$1\"}}" "$IMAGE"; }
expect "version label" "$VERSION" "$(label org.opencontainers.image.version)"
expect "license label" MIT "$(label org.opencontainers.image.licenses)"
expect "base label" scratch "$(label org.opencontainers.image.base.name)"
expect "pgmajor label" 18 "$(label io.cloudnativepg.image.base.pgmajor)"
expect "sql.version label" "$VERSION" "$(label io.cloudnativepg.image.sql.version)"
DEBIAN="$(label io.cloudnativepg.image.base.os)"
[[ -v "OPERANDS[$DEBIAN]" ]] || fail "io.cloudnativepg.image.base.os '$DEBIAN': no pinned CNPG operand for it"
expect "base.name label" "ghcr.io/cloudnative-pg/postgresql:18-minimal-$DEBIAN" "$(label io.cloudnativepg.image.base.name)"
expect "tag" "$VERSION-18-$DEBIAN" "${IMAGE##*:}"
OPERAND="${CNPG_OPERAND_IMAGE:-${OPERANDS[$DEBIAN]}}"
expect "entrypoint, cmd, user" "null null 65532:65532" \
    "$(docker image inspect -f '{{json .Config.Entrypoint}} {{json .Config.Cmd}} {{.Config.User}}' "$IMAGE")"
source_label="$(label org.opencontainers.image.source)"
[[ -z "$source_label" || "$source_label" =~ ^https://[^@/]+/[^@]+$ ]] || fail "source label '$source_label'"

# The files of every layer (docker save; GNU tar reads plain or gzipped
# layers), with their mode and owner.
mkdir -p "$DWORK/save"
docker image save "$IMAGE" | tar -x -C "$DWORK/save"
listing="$(jq -r '.[0].Layers[]' "$DWORK/save/manifest.json" | while read -r layer; do
    tar -tvf "$DWORK/save/$layer" --numeric-owner; done \
    | awk '{sub(/^\.?\//, "", $6); sub(/\/$/, "", $6); if ($6 != "") print $1, $2, $6}' | sort -k3 -u)"
expected="$( {
    printf 'drwxr-xr-x 0/0 %s\n' lib licenses licenses/pg_automerge share share/extension
    echo "-rwxr-xr-x 0/0 lib/pg_automerge.so"
    echo "-rw-r--r-- 0/0 licenses/pg_automerge/copyright"
    printf -- '-rw-r--r-- 0/0 share/extension/%s\n' pg_automerge.control "pg_automerge--$VERSION.sql"
    for f in sql/pg_automerge--*--*.sql; do if [[ -e "$f" ]]; then echo "-rw-r--r-- 0/0 share/extension/$(basename "$f")"; fi; done
    } | sort -k3)"
expect "image files (mode owner path)" "$expected" "$listing"
rm -rf "$DWORK/save"

# ---------------------------------------------------------------------------
mode="${CNPG_SMOKE_MOUNT:-auto}"
if [[ "$mode" == auto ]]; then
    server="$(docker version -f '{{.Server.Version}}')"
    if (( ${server%%.*} >= 28 )); then mode=image; else mode=copy; fi
fi
case "$mode" in
    image) mount=(--mount "type=image,source=$IMAGE,target=$MOUNT") ;;
    copy)
        # A container is never started from it: it has no command.
        tmp="$PROJECT-files"
        docker create --label "$LABEL" --name "$tmp" "$IMAGE" /none >/dev/null; CONTAINERS+=("$tmp")
        mkdir -m 0755 "$DWORK/ext"
        for d in lib share licenses; do docker cp "$tmp:/$d" "$DWORK/ext/" >/dev/null; done
        docker rm "$tmp" >/dev/null
        chmod -R a+rX "$DWORK/ext"
        mount=(--mount "type=bind,source=$DWORK/ext,target=$MOUNT,readonly") ;;
    *) fail "CNPG_SMOKE_MOUNT must be auto, image or copy, not '$mode'" ;;
esac

log "CNPG operand $OPERAND with the extension at $MOUNT ($mode mount)"
docker image inspect "$OPERAND" >/dev/null 2>&1 || docker pull -q "$OPERAND" >/dev/null
C="$(cname cnpg)"
# CNPG's instance manager is not here: initdb and postgres by hand, as the
# image's user (postgres, uid 26), with the settings the operator writes.
# pg_automerge.max_load_memory is set before the library loads, as a
# Cluster's spec.postgresql.parameters would set it.
docker run -d --name "$C" --label "$LABEL" "${mount[@]}" "$OPERAND" bash -euc "
    initdb -D /tmp/pgdata -U postgres --auth=trust -E UTF8 --locale=C >/tmp/initdb.log
    cat >>/tmp/pgdata/postgresql.conf <<'CONF'
extension_control_path = '\$system:$MOUNT/share'
dynamic_library_path = '\$libdir:$MOUNT/lib'
pg_automerge.max_load_memory = '1GB'
listen_addresses = ''
CONF
    exec postgres -D /tmp/pgdata" >/dev/null
CONTAINERS+=("$C")
for _ in $(seq 1 150); do
    [[ "$(docker inspect -f '{{.State.Running}}' "$C")" == true ]] || { docker logs "$C" >&2; fail "$C exited"; }
    docker exec "$C" pg_isready -q && break
    sleep 0.2
done
docker exec "$C" pg_isready -q || { docker logs "$C" >&2; fail "$C: not ready"; }
docker exec "$C" createdb -U postgres app

# The operand ships no pg_automerge; the mount is the only copy, read-only.
docker exec "$C" bash -euc "
    [ \$(id -u) = 26 ]
    ! ls /usr/lib/postgresql/18/lib/pg_automerge* /usr/share/postgresql/18/extension/pg_automerge* 2>/dev/null
    [ -r $MOUNT/lib/pg_automerge.so ] && [ -r $MOUNT/share/extension/pg_automerge.control ]
    ! touch $MOUNT/lib/x 2>/dev/null
" || fail "operand: no pg_automerge of its own, the mounted one readable and read-only"

out="$(psql_in cnpg app -v version="$VERSION" <<'SQL'
SHOW extension_control_path;
SHOW dynamic_library_path;
SELECT default_version, installed_version IS NULL FROM pg_available_extensions WHERE name = 'pg_automerge';
-- The upgrade scripts are found next to the control file.
SELECT path IS NOT NULL FROM pg_extension_update_paths('pg_automerge') WHERE source = '0.1.0' AND target = :'version';
SQL
)" || fail "available extensions: $out"
expect "extension available from the mount" \
    "$(printf '%s\n' "\$system:$MOUNT/share" "\$libdir:$MOUNT/lib" "$VERSION|t" t)" "$out"

log "CREATE EXTENSION: refused for a plain role, then as the superuser"
if out="$(psql_in cnpg app -c 'CREATE ROLE app LOGIN' -c 'SET ROLE app' -c 'CREATE EXTENSION pg_automerge' 2>&1)"; then
    fail "a plain role created the extension"
fi
grep -q 'permission denied to create extension' <<<"$out" || fail "plain role: $out"
BASE="$(fixture base)"; ALICE="$(fixture alice)"; BOB="$(fixture bob)"; NOTE="$(fixture note)"
[[ -n "$BASE" && -n "$ALICE" && -n "$BOB" && -n "$NOTE" ]] || fail "fixtures not found in tests/pg_regress/sql/automerge.sql"
out="$(psql_in cnpg app -v base="\\x$BASE" -v alice="\\x$ALICE" -v bob="\\x$BOB" -v note="\\x$NOTE" <<'SQL'
CREATE EXTENSION pg_automerge;
SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge';
-- module_pathname is bare: dynamic_library_path finds it in the mount, and
-- the backend maps that file.
SELECT DISTINCT probin FROM pg_proc WHERE probin LIKE '%automerge%';
SELECT position('/extensions/pg-automerge/lib/pg_automerge.so' IN pg_read_file('/proc/self/maps')) > 0;
SHOW pg_automerge.max_load_memory;
SHOW pg_automerge.verify_writes;
-- merge, jsonb reads and a GIN expression index.
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
CREATE INDEX docs_gin ON docs USING gin ((doc::jsonb) jsonb_path_ops);
INSERT INTO docs VALUES (1, :'base'::bytea);
UPDATE docs SET doc = merge(doc, :'alice'::automerge) WHERE id = 1;
UPDATE docs SET doc = merge(doc, :'bob'::bytea) WHERE id = 1;
SELECT doc->>'title', jsonb_path_query_array(doc, '$.items[*].name'), doc #>> '{items,1,name}',
       cardinality(automerge_heads(doc))
FROM docs;
SET enable_seqscan = off;
SELECT id FROM docs WHERE doc @> '{"items": [{"name": "eggs"}]}';
-- Rich text.
SELECT string_agg(s->>'value', '' ORDER BY ord) FILTER (WHERE s->>'type' = 'text')
FROM jsonb_array_elements(automerge_spans(:'note'::automerge, '{body}')) WITH ORDINALITY AS t(s, ord);
SET client_min_messages = warning;
ALTER EXTENSION pg_automerge UPDATE;
SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge';
SQL
)" || fail "SQL against the mounted extension: $out"
expect "SQL output" "$(printf '%s\n' "$VERSION" pg_automerge t 1GB on \
    'Groceries for Sunday|["milk", "eggs"]|eggs|2' 1 'Shopping tipsBuy fresh milk on Sunday.' "$VERSION")" "$out"

log "DROP EXTENSION, then CREATE EXTENSION .. SCHEMA (relocatable)"
expect "drop and recreate in a schema" automerge "$(psql_in cnpg app -c 'DROP TABLE docs' -c 'DROP EXTENSION pg_automerge' \
    -c 'CREATE SCHEMA automerge' -c 'CREATE EXTENSION pg_automerge SCHEMA automerge' \
    -c "SELECT extnamespace::regnamespace FROM pg_extension WHERE extname = 'pg_automerge'")"

check_log cnpg
