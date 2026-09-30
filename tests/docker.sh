#!/usr/bin/env bash
# Tests of the Docker image (run via `mise run docker-test`, which builds
# it first). Not part of `mise run test`: it needs Docker and a release
# build inside the image.
#
# Every container runs the image as built (the official entrypoint, the
# PGDG Postgres 18) on a fresh named volume, with its port published on a
# free port of 127.0.0.1; all are labelled pg-automerge-test and removed on
# exit, with their volumes (tests/docker_lib.sh). Checks:
#   - the image: OCI labels (a source label without credentials), the
#     version file, no toolchain, lz4 support, only the expected extension
#     files (install script, control file, library, and every
#     sql/pg_automerge--*--*.sql upgrade script);
#   - the release scripts: scripts/docker-archive.sh saves the image as
#     pg-automerge-<version>-linux-<arch>.tar.gz, scripts/docker-push.sh
#     --dry-run loads it back (the same image ID), prints the per-arch push
#     and the multi-arch index it would make, puts back a local tag the
#     archive carries (pointing elsewhere, or absent) and refuses a
#     repository without a registry host, an archive of another version and
#     two of one architecture, leaving no tags behind;
#   - default init: the container turns healthy, the init script installed
#     pg_automerge in POSTGRES_DB (at the Cargo.toml version) and nowhere
#     else, the server is a release build (debug_assertions off);
#   - SQL, through the container's own psql: the type and its casts (text,
#     bytea, jsonb), merge / || / merge_agg of the regress fixtures'
#     concurrent edits, jsonb operators on automerge values, a GIN
#     expression index (used by `doc @> ...`) and a STORED generated
#     doc::jsonb column, the history functions (changes, meta, change
#     bytes since heads, get_change, jsonb at earlier heads),
#     automerge_spans of the regress rich-text fixture (current and at
#     earlier heads, 22023 for a non-text path, 54000 and no crash for the
#     2,000-level deep block in this release build, whose jsonb view and
#     STORED generated doc::jsonb column read fine), lz4 TOAST
#     compression of automerge columns, settings passed with -c;
#   - automerge_notify(): a separate LISTEN session receives the trigger's
#     payload, whose heads match the row;
#   - the settings: defaults (verify_writes on, max_load_memory 2GB) and
#     both superuser-only (a plain role's SET is refused);
#   - pg_dump -Fc in one container, pg_restore --exit-on-error into a
#     second (fresh, PG_AUTOMERGE_CREATE_EXTENSION=0): same bytes, heads,
#     jsonb and generated columns, the GIN index valid and used, the
#     trigger restored;
#   - a restart on the same volume keeps the data and does not re-run init;
#     ALTER EXTENSION pg_automerge UPDATE is a no-op at the current version;
#   - the upgrade path in the image: every sql/snapshots version (and
#     variant, sql/snapshots/variants) installed
#     under a scratch name with the image's upgrade scripts, documents
#     stored, ALTER EXTENSION UPDATE to the current version, same data;
#   - PG_AUTOMERGE_CREATE_EXTENSION=0 skips the extension (and it can then
#     be created by hand into a schema); an invalid value fails init;
#   - PGHOST/PGHOSTADDR in the container's environment do not break init,
#     and an init file mounted singly runs after ours; a directory mounted
#     over /docker-entrypoint-initdb.d can run /usr/local/bin/pg-automerge-initdb;
#   - the suites (unless DOCKER_TEST_SUITES=0; they need Postgres 18 client
#     tools, PG_CONFIG's or by default pgrx's, and cargo): the regress
#     examples (pg_regress --use-existing, the same expected output as
#     `mise run regress`), and concurrency.sh,
#     notify.sh, dump.sh and extension.sh in tests/lib.sh's external mode,
#     against one container; limits.sh in its container mode against a
#     container started with --memory (no swap): without the limit the
#     crafted inputs get the backend OOM-killed and the cluster restarts,
#     with the default limit they get 53400 and nothing restarts (the
#     postmaster start time, the container's start time and restart count);
#   - no container's server log (docker logs) has an assertion failure,
#     PANIC, Rust panic or a backend killed by a signal (the limits
#     container: none besides its deliberate crashes);
#   - compose.yaml: `docker compose up --wait` turns healthy, the extension
#     is there, and `down -v` removes everything.
#
# Env: PG_AUTOMERGE_IMAGE (default pg-automerge:<Cargo.toml version>),
# PG_CONFIG (the suites' client tools, default pgrx's pg18),
# DOCKER_TEST_SUITES=0 (skip the suites), DOCKER_TEST_MEMORY (the limits
# container's memory cap, default 1g).

# shellcheck source=tests/docker_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/docker_lib.sh"

on_exit() {
    if [[ $1 == 0 ]]; then log "all docker checks passed"; else echo "docker test FAILED" >&2; fi
}

# Fixture documents (fixture, tests/docker_lib.sh): the regress examples'
# base shopping list, alice's and bob's concurrent edits of it, and bob's
# changes alone.
BASE="$(fixture base)"; ALICE="$(fixture alice)"; BOB="$(fixture bob)"; BOB_CHANGES="$(fixture bob_changes)"
NOTE="$(fixture note)"; DEEP_BLOCK="$(fixture deep_block)"
[[ -n "$BASE" && -n "$ALICE" && -n "$BOB" && -n "$BOB_CHANGES" && -n "$NOTE" && -n "$DEEP_BLOCK" ]] || fail "fixtures not found in tests/pg_regress/sql/automerge.sql"
FIXTURES=(-v base="\\x$BASE" -v alice="\\x$ALICE" -v bob="\\x$BOB" -v bob_changes="\\x$BOB_CHANGES")

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
# The install script, the control file, the library, and every upgrade
# script in sql/ (cargo pgrx package ships them; see "Versioning and
# upgrades" in docs/DESIGN.md).
expected_files="$( { printf '%s\n' "pg_automerge--$VERSION.sql" pg_automerge.control pg_automerge.so
    for f in sql/pg_automerge--*--*.sql; do if [[ -e "$f" ]]; then basename "$f"; fi; done; } | sort | tr '\n' ' ')"
expect "extension files" "$expected_files" "$files"
# The source label is the normalised remote (scripts/oci-source-url.sh):
# empty, or an https URL without credentials.
source_label="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.source"}}' "$IMAGE")"
[[ -z "$source_label" || "$source_label" =~ ^https://[^@/]+/[^@]+$ ]] || fail "source label '$source_label'"

log "release archive (scripts/docker-archive.sh) and a push dry run (scripts/docker-push.sh)"
ARCH="$(docker image inspect -f '{{.Architecture}}' "$IMAGE")"
archive="$(bash scripts/docker-archive.sh "$IMAGE" "$DWORK/release")"
expect "archive name" "$DWORK/release/pg-automerge-$VERSION-linux-$ARCH.tar.gz" "$archive"
PUSH_REPO=registry.invalid/pg-automerge-test
IMAGE_ID="$(docker image inspect -f '{{.Id}}' "$IMAGE")"
out="$(bash scripts/docker-push.sh --dry-run "$PUSH_REPO" "$archive")"
expect "push dry run" "$(printf '%s\n' "loaded $archive $IMAGE_ID" "+ docker push $PUSH_REPO:$VERSION-$ARCH" \
    "+ docker buildx imagetools create -t $PUSH_REPO:$VERSION -t $PUSH_REPO:latest $PUSH_REPO:$VERSION-$ARCH")" "$out"
expect "dry run leaves no tags" "" "$(docker image ls -q "$PUSH_REPO")"
expect "the image is still $IMAGE" "$IMAGE_ID" "$(docker image inspect -f '{{.Id}}' "$IMAGE")"
# Loading an archive puts back the local tag it carries: an archive of
# SCRATCH:<version>, loaded while that tag points to another image (as
# pg-automerge:<version> does when an archive of another architecture is
# loaded), and while it does not exist.
SCRATCH="$PROJECT-img:$VERSION"
docker tag "$IMAGE" "$SCRATCH"; IMAGES+=("$SCRATCH")
archive2="$(bash scripts/docker-archive.sh "$SCRATCH" "$DWORK/release2")"
decoy="$(printf 'FROM %s\nLABEL pg-automerge-test.decoy=1\n' "$IMAGE" \
    | DOCKER_BUILDKIT=1 docker build -q --label "$LABEL" -t "$SCRATCH" -)"
[[ "$decoy" != "$IMAGE_ID" ]] || fail "decoy image is the image"
out="$(bash scripts/docker-push.sh --dry-run "$PUSH_REPO" "$archive2")"
expect "dry run loads the archive's image" "loaded $archive2 $IMAGE_ID" "$(head -1 <<<"$out")"
expect "dry run puts back the tag it replaced" "$decoy" "$(docker image inspect -f '{{.Id}}' "$SCRATCH")"
docker rmi "$SCRATCH" >/dev/null
bash scripts/docker-push.sh --dry-run "$PUSH_REPO" "$archive2" >/dev/null
if docker image inspect "$SCRATCH" >/dev/null 2>&1; then fail "dry run left the tag $SCRATCH it loaded"; fi
expect "the image is still $IMAGE after the loads" "$IMAGE_ID" "$(docker image inspect -f '{{.Id}}' "$IMAGE")"
expect "dry runs leave no tags" "" "$(docker image ls -q "$PUSH_REPO")"
rm -rf "$DWORK/release2"
# Refused: a repository without a registry host (Docker would pick Docker
# Hub), an archive of another version, the same architecture twice.
for bad in "pg-automerge|REPOSITORY must be registry-host/path" \
           "$PUSH_REPO $DWORK/release/pg-automerge-0.0.0-linux-$ARCH.tar.gz|version 0.0.0, Cargo.toml says $VERSION" \
           "$PUSH_REPO $archive|two archives for $ARCH"; do
    args="${bad%%|*}"; want="${bad#*|}"
    cp "$archive" "$DWORK/release/pg-automerge-0.0.0-linux-$ARCH.tar.gz"
    # shellcheck disable=SC2086 # args is a word list
    if out="$(bash scripts/docker-push.sh --dry-run $args "$archive" 2>&1)"; then fail "docker-push.sh accepted: $args"; fi
    grep -qF "$want" <<<"$out" || fail "docker-push.sh $args: expected '$want', got: $out"
done
expect "refusals leave no tags" "" "$(docker image ls -q "$PUSH_REPO")"
rm -rf "$DWORK/release"

# ---------------------------------------------------------------------------
log "default init: healthy, extension in POSTGRES_DB only, settings from -c"
start_ready default data -- -c default_toast_compression=lz4 -c pg_automerge.max_load_memory=512MB
C1="$(cname default)"
grep -q "pg_automerge $VERSION installed" <<<"$(docker logs "$C1" 2>&1)" || { docker logs "$C1" >&2; fail "init script did not report the install"; }
expect "extension version in app" "$VERSION" "$(psql_in default app -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"
expect "extension in postgres db" "" "$(psql_in default postgres -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"
expect "extension in template1" "" "$(psql_in default template1 -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"
expect "server is a release build" "off" "$(psql_in default app -c 'SHOW debug_assertions')"
expect "server version" "18" "$(psql_in default app -c "SELECT current_setting('server_version_num')::int / 10000")"

log "type, casts, merge / || / merge_agg, jsonb reads, lz4"
out="$(psql_in default app "${FIXTURES[@]}" <<'SQL'
SHOW pg_automerge.max_load_memory;
SHOW default_toast_compression;
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
-- bytea -> automerge is an assignment cast.
INSERT INTO docs VALUES (1, :'base'::bytea);
UPDATE docs SET doc = merge(doc, :'alice'::automerge) WHERE id = 1;
UPDATE docs SET doc = merge(doc, :'bob'::bytea) WHERE id = 1;
SELECT doc->>'title', jsonb_path_query_array(doc, '$.items[*].name'),
       doc @> '{"status": "open"}', doc ? 'items', doc #>> '{items,1,name}',
       cardinality(automerge_heads(doc))
FROM docs WHERE id = 1;
-- Text I/O and the bytea cast round-trip; automerge -> jsonb is implicit.
SELECT (doc::text)::automerge::bytea = doc::bytea, left(doc::text, 10) = '\x856f4a83',
       doc::jsonb = automerge_to_jsonb(doc)
FROM docs WHERE id = 1;
-- || is merge, for documents and for bare changes; merge_agg of all three
-- (in another order: the same heads and content, the bytes may differ).
SELECT automerge_heads(:'base'::automerge || :'alice'::automerge || :'bob'::automerge) = automerge_heads(doc),
       automerge_heads(merge(:'base'::automerge, :'alice'::automerge) || :'bob_changes'::bytea) = automerge_heads(doc),
       (SELECT automerge_heads(m) = automerge_heads(doc) AND m::jsonb = doc::jsonb
        FROM (SELECT merge_agg(d) AS m FROM (VALUES (:'bob'::automerge), (:'alice'), (:'base')) v(d)) s)
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
expect "SQL output" "$(printf '%s\n' 512MB lz4 'Groceries for Sunday|["milk", "eggs"]|t|t|eggs|2' 't|t|t' 't|t|t' 'lz4|Groceries for Sunday' l)" "$out"
if out="$(psql_in default app -v VERBOSITY=verbose -c "SELECT '\\x856f4a83deadbeef'::automerge" 2>&1)"; then fail "corrupt input accepted"; fi
grep -q "ERROR:  22P02: invalid automerge document" <<<"$out" || fail "corrupt input: $out"

log "GIN expression index and a generated column"
out="$(psql_in default app "${FIXTURES[@]}" <<'SQL'
CREATE TABLE notes (
    id int PRIMARY KEY,
    doc automerge NOT NULL,
    data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED
);
CREATE INDEX notes_gin ON notes USING gin ((doc::jsonb) jsonb_path_ops);
INSERT INTO notes (id, doc) SELECT i, :'base'::bytea FROM generate_series(1, 500) i;
UPDATE notes SET doc = doc || :'bob_changes'::bytea WHERE id = 7;
ANALYZE notes;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT id FROM notes WHERE doc @> '{"items": [{"name": "eggs"}]}';
SELECT string_agg(id::text, ',') FROM notes WHERE doc @> '{"items": [{"name": "eggs"}]}';
SELECT count(*) FROM notes WHERE data = doc::jsonb;
SELECT data #>> '{items,1,name}' FROM notes WHERE id = 7;
SQL
)"
grep -q "Bitmap Index Scan on notes_gin" <<<"$out" || fail "GIN expression index not used: $out"
expect "index scan and generated column" "$(printf '%s\n' 7 500 eggs)" "$(tail -3 <<<"$out")"

log "history functions"
out="$(psql_in default app "${FIXTURES[@]}" <<'SQL'
WITH d AS (SELECT doc FROM docs WHERE id = 1),
     b AS (SELECT automerge_heads(:'base'::automerge) AS heads)
SELECT automerge_change_count(doc) = (SELECT count(*) FROM automerge_changes(doc)),
       (SELECT count(*) FROM automerge_changes_meta(doc)) = automerge_change_count(doc),
       (SELECT count(*) FROM automerge_changes(doc, heads)) = automerge_change_count(doc) - automerge_change_count(:'base'::automerge),
       automerge_heads(:'base'::automerge || automerge_changes_bytes(doc, heads)) = automerge_heads(doc),
       automerge_to_jsonb(doc, heads) = :'base'::automerge::jsonb,
       (automerge_get_change(doc, (automerge_heads(doc))[1])).hash = (automerge_heads(doc))[1],
       automerge_contains(doc, :'alice'::automerge) AND NOT automerge_contains(:'alice'::automerge, doc)
FROM d, b;
SQL
)"
expect "history" 't|t|t|t|t|t|t' "$out"

log "rich text: automerge_spans"
out="$(psql_in default app -v note="\\x$NOTE" <<'SQL'
CREATE TABLE rich (id int PRIMARY KEY, doc automerge NOT NULL);
INSERT INTO rich VALUES (1, :'note'::bytea);
SELECT s->>'type', s->>'value', coalesce(s->'marks'->>'bold', s->'marks'->>'link', '')
FROM rich, jsonb_array_elements(automerge_spans(doc, '{body}')) s;
SELECT automerge_spans(doc, '{missing}') IS NULL,
       jsonb_array_length(automerge_spans(doc, '{body}', ARRAY[m.hash])),
       automerge_spans(doc, '{body}', automerge_heads(doc)) = automerge_spans(doc, '{body}')
FROM rich, automerge_changes_meta(doc) m WHERE m.message = 'write';
SQL
)"
expect "spans" "$(printf '%s\n' 'block|{"type": "heading", "attrs": {"level": 1}, "parents": []}|' 'text|Shopping tips|' \
    'block|{"type": "paragraph", "attrs": {}, "parents": []}|' 'text|Buy |' 'text|fresh milk|true' 'text| on |' \
    'text|Sunday|https://example.com/sunday' 'text|.|' 't|4|t')" "$out"
# Not a text object: 22023. A block nested 2,000 levels deep: 54000 in
# this release build, not a crash (check_log below would see a signal).
for bad in "note|{title}|22023: automerge value at path {title} is a string scalar, not a text object" \
           "deep_block|{body}|54000: automerge text block is nested more than 32 levels deep"; do
    IFS='|' read -r fx path want <<<"$bad"
    var="${fx^^}"
    if out="$(psql_in default app -v VERBOSITY=verbose -v doc="\\x${!var}" -v path="$path" \
        <<<"SELECT automerge_spans(:'doc'::automerge, :'path');" 2>&1)"; then
        fail "automerge_spans($fx, $path) did not fail"
    fi
    grep -qF "ERROR:  $want" <<<"$out" || fail "automerge_spans($fx, $path): $out"
done
# Everything else reads the deep block's document: its jsonb view shows the
# text as a string without rendering the block (Automerge's rendering of
# it would overflow the stack), also in a STORED generated column.
out="$(psql_in default app -v doc="\\x$DEEP_BLOCK" <<'SQL'
CREATE TABLE deep (doc automerge NOT NULL, data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED);
INSERT INTO deep VALUES (:'doc');
SELECT data = doc::jsonb, doc->>'body' = E'\uFFFCx',
       automerge_to_jsonb(doc, automerge_heads(doc)) = data FROM deep;
SQL
)"
expect "deep block jsonb" 't|t|t' "$out"

log "automerge_notify: a separate LISTEN session receives the payload"
psql_in default app -c "CREATE TABLE notify_done (x int)" \
    -c "CREATE TRIGGER notes_notify AFTER INSERT OR UPDATE OR DELETE ON notes
        FOR EACH ROW EXECUTE FUNCTION automerge_notify('notes_changed', 'id')"
# The listener holds a statement open until the writer is done; Postgres
# delivers the notification when that statement ends.
docker exec -i -e PGAPPNAME=docker_listener "$C1" psql -X -At -U postgres -d app >"$DWORK/listener.out" 2>&1 <<'SQL' &
LISTEN notes_changed;
DO $$ BEGIN
    WHILE NOT EXISTS (SELECT 1 FROM notify_done) LOOP PERFORM pg_sleep(0.01); END LOOP;
END $$;
SQL
listener=$!
for _ in $(seq 1 250); do
    [[ "$(psql_in default app -c "SELECT count(*) FROM pg_stat_activity WHERE application_name = 'docker_listener' AND query LIKE '%notify_done%'")" == 1 ]] && break
    sleep 0.02
done
psql_in default app "${FIXTURES[@]}" <<'SQL'
UPDATE notes SET doc = merge(doc, :'alice'::automerge) WHERE id = 3;
INSERT INTO notify_done VALUES (1);
SQL
wait "$listener" || { cat "$DWORK/listener.out" >&2; fail "listener session failed"; }
payload="$(sed -n 's/^Asynchronous notification "notes_changed" with payload "\(.*\)" received from server process with PID [0-9]*\.$/\1/p' "$DWORK/listener.out")"
[[ -n "$payload" ]] || { cat "$DWORK/listener.out" >&2; fail "no notification received"; }
fields="$(psql_in default app -v p="$payload" <<'SQL'
SELECT concat_ws(' ', p->>'table', p->>'op', p->'key',
       p->'columns'->'doc'->'heads' = (SELECT to_jsonb(automerge_heads(doc)) FROM notes WHERE id = 3),
       p->'columns'->'doc'->'prev_heads' = (SELECT to_jsonb(automerge_heads(doc)) FROM notes WHERE id = 4))
FROM (SELECT :'p'::jsonb AS p) s;
SQL
)"
expect "notification payload" 'public.notes UPDATE {"id": 3} t t' "$fields"

# ---------------------------------------------------------------------------
log "pg_dump -Fc here, pg_restore into a second container"
docker exec "$C1" pg_dump -U postgres -Fc app >"$DWORK/app.dump"
start_ready restored restored -e PG_AUTOMERGE_CREATE_EXTENSION=0
docker exec -i "$(cname restored)" pg_restore -U postgres -d app --exit-on-error <"$DWORK/app.dump" \
    || fail "pg_restore into the second container"
FINGERPRINT="SELECT string_agg(id || ':' || md5(doc::bytea) || ':' || automerge_heads(doc)::text || ':' || md5(doc::jsonb::text), ',' ORDER BY id) FROM docs;
SELECT md5(string_agg(id || ':' || md5(doc::bytea) || ':' || md5(data::text), ',' ORDER BY id)) FROM notes;
SELECT md5(doc::bytea) FROM big;
SELECT attcompression FROM pg_attribute WHERE attrelid = 'big'::regclass AND attname = 'doc';
SELECT count(*) FROM pg_index WHERE indrelid = 'notes'::regclass AND indisvalid;
SELECT tgname FROM pg_trigger WHERE tgrelid = 'notes'::regclass AND NOT tgisinternal;"
expected="$(psql_in default app -c "$FINGERPRINT")"
expect "restored data" "$expected" "$(psql_in restored app -c "$FINGERPRINT")"
grep -q "Bitmap Index Scan on notes_gin" <<<"$(psql_in restored app -c "SET enable_seqscan = off" \
    -c "EXPLAIN (COSTS OFF) SELECT id FROM notes WHERE doc @> '{\"items\": [{\"name\": \"eggs\"}]}'")" \
    || fail "restored GIN index not used"
check_log restored
docker rm -f -v "$(cname restored)" >/dev/null

# ---------------------------------------------------------------------------
log "restart on the same volume: data kept, init not re-run, UPDATE is a no-op"
check_log default
docker rm -f "$C1" >/dev/null
start_ready restart data
C2="$(cname restart)"
grep -q 'Skipping initialization' <<<"$(docker logs "$C2" 2>&1)" || { docker logs "$C2" >&2; fail "restart re-ran init"; }
expect "data after restart" "Groceries for Sunday|eggs" "$(psql_in restart app -c "SELECT (SELECT doc->>'title' FROM docs WHERE id = 1), (SELECT data #>> '{items,1,name}' FROM notes WHERE id = 7)")"
psql_in restart app -c 'SET client_min_messages = warning' -c 'ALTER EXTENSION pg_automerge UPDATE'
expect "version after UPDATE" "$VERSION" "$(psql_in restart app -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"

log "upgrade path in the image: each sql/snapshots version and variant, ALTER EXTENSION UPDATE with the image's scripts"
# As tests/upgrade.sh does against pgrx's Postgres: install the snapshot
# under the version name "V-snapshot", plus the first hop of each upgrade
# path from V that the image ships (renamed to start at V-snapshot), or an
# empty V-snapshot--<current> for the current version. So a release whose
# image lacks an upgrade script from an older version fails here.
EXTDIR=/usr/share/postgresql/18/extension
DOCS_FINGERPRINT="$(head -1 <<<"$FINGERPRINT")"
n=0
for snapshot in sql/snapshots/pg_automerge--*.sql sql/snapshots/variants/pg_automerge--*.sql; do
    [[ -e "$snapshot" ]] || continue
    v="${snapshot##*/pg_automerge--}"; v="${v%.sql}"; v="${v%%+*}"; old="$v-snapshot"; n=$((n + 1))
    docker cp -q "$snapshot" "$C2:$EXTDIR/pg_automerge--$old.sql"
    if [[ "$v" == "$VERSION" ]]; then
        docker exec "$C2" sh -c ": >'$EXTDIR/pg_automerge--$old--$VERSION.sql'"
    else
        docker exec "$C2" bash -euc 'cd "$1"; from="$2"; old="$3"; set -- pg_automerge--"$from"--*.sql; [ -e "$1" ] || exit 3
            for f; do cp "$f" "pg_automerge--$old--${f#pg_automerge--"$from"--}"; done' _ "$EXTDIR" "$v" "$old" \
            || fail "the image ships no upgrade script from $v (sql/pg_automerge--$v--*.sql)"
    fi
    psql_in restart postgres -c "CREATE DATABASE upgrade_$n"
    psql_in restart "upgrade_$n" "${FIXTURES[@]}" -v old="$old" <<'SQL'
CREATE EXTENSION pg_automerge VERSION :'old';
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
CREATE INDEX ON docs USING gin ((doc::jsonb));
INSERT INTO docs VALUES (1, :'base'), (2, :'alice'), (3, merge(:'base'::automerge, :'bob_changes'::bytea));
SQL
    before="$(psql_in restart "upgrade_$n" -c "$DOCS_FINGERPRINT")"
    psql_in restart "upgrade_$n" -c 'ALTER EXTENSION pg_automerge UPDATE'
    expect "$v: version after UPDATE" "$VERSION" "$(psql_in restart "upgrade_$n" -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"
    expect "$v: documents after UPDATE" "$before" "$(psql_in restart "upgrade_$n" -c "$DOCS_FINGERPRINT")"
    expect "$v: merge after UPDATE" "Groceries for Sunday|2" "$(psql_in restart "upgrade_$n" \
        -c "SELECT (d.doc || a.doc)->>'title', cardinality(automerge_heads(d.doc || a.doc)) FROM docs d, docs a WHERE d.id = 3 AND a.id = 2")"
    docker exec "$C2" sh -c "rm -f '$EXTDIR'/pg_automerge--'$old'*.sql"
done
((n)) || fail "no snapshots in sql/snapshots"
check_log restart
docker rm -f "$C2" >/dev/null

# ---------------------------------------------------------------------------
log "PG_AUTOMERGE_CREATE_EXTENSION=0 skips it; CREATE EXTENSION .. SCHEMA by hand works"
start_ready skip skip -e PG_AUTOMERGE_CREATE_EXTENSION=0
expect "extension with =0" "" "$(psql_in skip app -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"
expect "manual install into a schema" "automerge|{}" "$(psql_in skip app -c 'CREATE SCHEMA automerge' -c 'CREATE EXTENSION pg_automerge SCHEMA automerge' \
    -c "SELECT n.nspname, automerge.automerge_to_jsonb(''::bytea::automerge.automerge) FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace WHERE extname = 'pg_automerge'")"

log "settings: defaults, and superuser-only"
expect "defaults" "on|2GB" "$(psql_in skip app -c "LOAD 'pg_automerge'" \
    -c "SELECT current_setting('pg_automerge.verify_writes') || '|' || current_setting('pg_automerge.max_load_memory')")"
psql_in skip app -c "CREATE ROLE plain LOGIN" -c "GRANT USAGE ON SCHEMA automerge TO plain"
for setting in "pg_automerge.verify_writes = off" "pg_automerge.max_load_memory = -1"; do
    # The library is loaded first (by using the type), so SET meets the
    # real setting rather than a placeholder.
    if out="$(docker exec "$(cname skip)" psql -X -At -v ON_ERROR_STOP=1 -U plain -d app \
            -c "SELECT automerge.automerge_heads(''::bytea::automerge.automerge)" -c "SET $setting" 2>&1)"; then
        fail "a plain role could SET $setting"
    fi
    grep -q "permission denied to set parameter \"${setting%% *}\"" <<<"$out" || fail "SET $setting: $out"
done
check_log skip
docker rm -f -v "$(cname skip)" >/dev/null

log "PG_AUTOMERGE_CREATE_EXTENSION=yes fails initialization"
start bad bad -e PG_AUTOMERGE_CREATE_EXTENSION=yes
if wait_ready bad; then fail "$(cname bad) started with an invalid PG_AUTOMERGE_CREATE_EXTENSION"; fi
grep -qF "PG_AUTOMERGE_CREATE_EXTENSION must be 0 or 1, got 'yes'" <<<"$(docker logs "$(cname bad)" 2>&1)" \
    || { docker logs "$(cname bad)" >&2; fail "no error message for the invalid value"; }
docker rm -f -v "$(cname bad)" >/dev/null

log "PGHOST in the environment, and an init file mounted next to ours"
# The init script connects over the socket like the entrypoint's own psql,
# whatever PGHOST says; a single file mounted into
# /docker-entrypoint-initdb.d runs after it (README.md, Docker).
INIT="$DWORK/init"; mkdir -p "$INIT"; chmod 755 "$INIT"
printf '%s\n' 'CREATE TABLE app_t (id int PRIMARY KEY, doc automerge NOT NULL);' \
    "INSERT INTO app_t VALUES (1, '\\x$BASE');" >"$INIT/20-app.sql"
chmod 644 "$INIT/20-app.sql"
start_ready initfile initfile -e PGHOST=localhost -e PGHOSTADDR=127.0.0.1 \
    -v "$INIT/20-app.sql:/docker-entrypoint-initdb.d/20-app.sql:ro"
expect "PGHOST set: extension and the app's init file" "$VERSION|Groceries" \
    "$(docker exec -e PGHOST= -e PGHOSTADDR= "$(cname initfile)" psql -X -At -U postgres -d app -c "SELECT (SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'), (SELECT doc->>'title' FROM app_t)")"
check_log initfile
docker rm -f -v "$(cname initfile)" >/dev/null

log "a directory mounted over /docker-entrypoint-initdb.d runs pg-automerge-initdb itself"
printf '#!/bin/sh\nexec pg-automerge-initdb\n' >"$INIT/10-pg-automerge.sh"
chmod 755 "$INIT/10-pg-automerge.sh"
start_ready initdir initdir -v "$INIT:/docker-entrypoint-initdb.d:ro"
expect "directory mount: extension and the app's init file" "$VERSION|Groceries" \
    "$(psql_in initdir app -c "SELECT (SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'), (SELECT doc->>'title' FROM app_t)")"
check_log initdir
docker rm -f -v "$(cname initdir)" >/dev/null

# ---------------------------------------------------------------------------
if [[ "${DOCKER_TEST_SUITES:-1}" == 1 ]]; then
    PG_CONFIG="${PG_CONFIG:-$(sed -n 's/^pg18 *= *"\(.*\)"/\1/p' "${PGRX_HOME:-$HOME/.pgrx}/config.toml" 2>/dev/null)}"
    [[ -x "$PG_CONFIG" ]] || fail "the suites need Postgres 18 client tools: PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config (PGDG's postgresql-client-18), pgrx's (mise run pgrx-init), or DOCKER_TEST_SUITES=0"
    export PG_CONFIG

    log "suites against a fresh container"
    start_ready suites suites
    mapfile -t ENV < <(external_env suites)

    log "regress examples (pg_regress --use-existing)"
    psql_in suites postgres -c "CREATE DATABASE pg_automerge_regress TEMPLATE template0"
    if ! env "${ENV[@]}" PGPASSWORD="$PG_PASSWORD" "$("$PG_CONFIG" --pkglibdir)/pgxs/src/test/regress/pg_regress" \
            --use-existing --host=127.0.0.1 --port="$(host_port suites)" --user=postgres \
            --dbname=pg_automerge_regress --bindir="$("$PG_CONFIG" --bindir)" \
            --inputdir=tests/pg_regress --outputdir="$DWORK/regress" setup automerge >"$DWORK/regress.log" 2>&1; then
        cat "$DWORK/regress.log" "$DWORK/regress/regression.diffs" >&2 || true
        fail "regress examples"
    fi
    for suite in concurrency notify dump extension; do
        log "tests/$suite.sh"
        env "${ENV[@]}" bash "tests/$suite.sh" >"$DWORK/$suite.log" 2>&1 \
            || { cat "$DWORK/$suite.log" >&2; fail "tests/$suite.sh against the container"; }
    done
    check_log suites
    docker rm -f -v "$(cname suites)" >/dev/null

    memory="${DOCKER_TEST_MEMORY:-1g}"
    log "tests/limits.sh against a container with --memory=$memory"
    start_ready limits limits --memory="$memory" --memory-swap="$memory"
    mapfile -t ENV < <(external_env limits)
    env "${ENV[@]}" LIMITS_CONTAINER="$(cname limits)" bash tests/limits.sh >"$DWORK/limits.log" 2>&1 \
        || { cat "$DWORK/limits.log" >&2; fail "tests/limits.sh against the container"; }
    grep '^==>   ' "$DWORK/limits.log" | sort | uniq -c | sed 's/^ */    /'
    check_log limits --crashes-allowed
    docker rm -f -v "$(cname limits)" >/dev/null
else
    log "DOCKER_TEST_SUITES=0: skipping the regress examples, the multi-session suites and limits.sh"
fi

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
