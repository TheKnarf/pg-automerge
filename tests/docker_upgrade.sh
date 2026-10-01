#!/usr/bin/env bash
# Upgrade of a real 0.1.0 deployment to the current image (run via `mise
# run docker-upgrade-test`, which builds the current image first). Not part
# of `mise run test` or `mise run docker-test`: it needs a 0.1.0 image. See
# the docs' "Updating" (docs/src/pages/guide/updating.mdx) and "Versioning
# and upgrades" (docs/src/pages/design/versioning.mdx).
#
# What an app on the 0.1.0 image does when it moves to this one:
#   1. a container of the OLD image on a fresh volume (labelled
#      pg-automerge-test, like every container and volume here, all
#      removed on exit): the init script creates pg_automerge 0.1.0 in
#      POSTGRES_DB; tables with automerge columns, a STORED generated
#      doc::jsonb column, a GIN index on (doc::jsonb) and a B-tree one on
#      (doc::jsonb ->> 'title'), a view, an automerge_notify() trigger, and
#      documents (merged, rich text); the old image's install script must
#      be sql/snapshots/pg_automerge--0.1.0.sql (the release) or a variant
#      in sql/snapshots/variants (0.1.0+spans: images built from the
#      development tree after automerge_spans, still labelled 0.1.0);
#   2. stop it (the volume stays) and start the CURRENT image on the same
#      volume: init is not re-run, the extension is still 0.1.0 (its
#      catalog on the new library) and every document reads the same; the
#      2,000-level deep block, which crashes the released 0.1.0 library in
#      a generated doc::jsonb column, is stored fine already;
#   3. ALTER EXTENSION pg_automerge UPDATE to the current version (0.1.0
#      -> 0.2.0 -> ..., every script in one step); the documents, the view
#      and the generated column read the same; the indexes are valid, used
#      and agree with a sequential scan; the trigger sends its
#      notification; automerge_spans works (54000 on the deep block), and
#      so does automerge_memory_usage() (0.3.0); the
#      extension's catalog (tests/catalog.sql) is that of a fresh CREATE
#      EXTENSION in the same container; no server log shows a crash.
#
# The compose steps of the docs' Updating page (the "From ... to <current> with compose"
# section that names 0.1.0) are
# followed as written: the cluster's superuser is not postgres (POSTGRES_USER
# is the app's, as in skjera's compose.yml, so there is no postgres role),
# the pg_dump of step 1 and the psql commands of step 5 are read from the
# page's MDX source ($UPDATING_PAGE) and run with `docker compose exec [-T]
# postgres` as `docker exec -i`
# on the container and <user>/<db> filled in; and a service like skjera's
# (image: plus build: with args PG_AUTOMERGE_VERSION) is built with the old
# version arg (the Dockerfile refuses it) and after step 3's edit (it
# builds, reusing the cache of the image just built).
#
# Env: PG_AUTOMERGE_IMAGE (the new image, default
# pg-automerge:<Cargo.toml version>), PG_AUTOMERGE_OLD_VERSION (the
# version upgraded from: 0.1.0, the default, or 0.2.0; the steps are the
# same, 0.1.0 is used as the example above), PG_AUTOMERGE_OLD_IMAGE (the
# old image, a tag or an image ID; by default built from RELEASE_REV below
# with that revision's own scripts/docker-build.sh, as
# pg-automerge-test-old-<pid>:<old version>, removed on exit; needs the git
# history).

# shellcheck source=tests/docker_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/docker_lib.sh"

# The version upgraded from (PG_AUTOMERGE_OLD_VERSION, default 0.1.0) and
# the commit its image is built from when PG_AUTOMERGE_OLD_IMAGE is unset.
OLD_VERSION="${PG_AUTOMERGE_OLD_VERSION:-0.1.0}"
case "$OLD_VERSION" in
    # The last commit before automerge_spans: its SQL is 0.1.0's (the same
    # statements as the snapshot; pgrx may order them differently).
    0.1.0) RELEASE_REV=0918f56 ;;
    # The 0.2.0 release and its upgrade docs (skjera's image).
    0.2.0) RELEASE_REV=356a2f9 ;;
    *) fail "PG_AUTOMERGE_OLD_VERSION=$OLD_VERSION: only 0.1.0 and 0.2.0 are known" ;;
esac

# Not postgres: an app's own POSTGRES_USER (skjera's is skjera).
PG_USER=appowner

on_exit() {
    if [[ $1 == 0 ]]; then log "all docker upgrade checks passed"; else echo "docker upgrade test FAILED" >&2; fi
}

[[ "$VERSION" != "$OLD_VERSION" ]] || fail "the current version is $OLD_VERSION: nothing to upgrade to"
SNAPSHOT="sql/snapshots/pg_automerge--$OLD_VERSION.sql"
[[ -e "$SNAPSHOT" ]] || fail "$SNAPSHOT missing"

BASE="\\x$(fixture base)"; ALICE="\\x$(fixture alice)"; BOB_CHANGES="\\x$(fixture bob_changes)"
NOTE="\\x$(fixture note)"; DEEP_BLOCK="\\x$(fixture deep_block)"
for f in "$BASE" "$ALICE" "$BOB_CHANGES" "$NOTE" "$DEEP_BLOCK"; do
    [[ ${#f} -gt 2 ]] || fail "fixtures not found in tests/pg_regress/sql/automerge.sql"
done

OLD_IMAGE="${PG_AUTOMERGE_OLD_IMAGE:-}"
if [[ -z "$OLD_IMAGE" ]]; then
    log "building the $OLD_VERSION image from $RELEASE_REV (set PG_AUTOMERGE_OLD_IMAGE to use an existing one)"
    git cat-file -e "$RELEASE_REV^{commit}" 2>/dev/null \
        || fail "commit $RELEASE_REV is not in this clone (a shallow checkout?); set PG_AUTOMERGE_OLD_IMAGE"
    mkdir "$DWORK/old-src"
    git archive "$RELEASE_REV" | tar -x -C "$DWORK/old-src"
    old_repo="pg-automerge-test-old-$$"
    IMAGES+=("$old_repo:$OLD_VERSION" "$old_repo:dev")
    (cd "$DWORK/old-src" && PG_AUTOMERGE_IMAGE="$old_repo" bash scripts/docker-build.sh) >"$DWORK/old-build.log" 2>&1 \
        || { tail -50 "$DWORK/old-build.log" >&2; fail "building the $OLD_VERSION image"; }
    OLD_IMAGE="$old_repo:$OLD_VERSION"
fi
docker image inspect "$OLD_IMAGE" >/dev/null 2>&1 || fail "old image $OLD_IMAGE not found"

log "the old image ($OLD_IMAGE): version $OLD_VERSION, its install script a known $OLD_VERSION catalog"
expect "old image version label" "$OLD_VERSION" \
    "$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' "$OLD_IMAGE")"
docker run --rm --label "$LABEL" --network none --entrypoint cat "$OLD_IMAGE" \
    "/usr/share/postgresql/18/extension/pg_automerge--$OLD_VERSION.sql" >"$DWORK/old.sql"
# The released script, or a variant (sql/snapshots/variants): the same
# statements, in pgrx's order of the day and with its source-line comments.
statements() { grep -v '^-- src/' "$1" | sort; }
known=""
for f in "$SNAPSHOT" sql/snapshots/variants/pg_automerge--"$OLD_VERSION"+*.sql; do
    [[ -e "$f" ]] || continue
    if cmp -s <(statements "$DWORK/old.sql") <(statements "$f"); then known="$f"; break; fi
done
[[ -n "$known" ]] || fail "the old image's install script is none of the known $OLD_VERSION catalogs: $(diff "$SNAPSHOT" "$DWORK/old.sql" | head -20)"
log "  its install script is $known"
if grep -q automerge_spans "$DWORK/old.sql"; then OLD_HAS_SPANS=f; else OLD_HAS_SPANS=t; fi

# The Updating page's compose steps, as shell commands: [0] the pg_dump of step 1,
# [1] the ALTER EXTENSION and [2] the version check of step 5.
UPDATING_PAGE=docs/src/pages/guide/updating.mdx
section="$(sed -n "/^\*\*From .*$OLD_VERSION.* to $VERSION with compose\*\*/,/^There is no downgrade script/p" "$UPDATING_PAGE")"
[[ -n "$section" ]] || fail "$UPDATING_PAGE has no section \"From ... $OLD_VERSION ... to $VERSION with compose\""
mapfile -t STEP_CMDS < <(grep -o 'docker compose exec [^`]*' <<<"$section")
expect "Updating page compose commands" 3 "${#STEP_CMDS[@]}"
# step_cmd NAME I: run STEP_CMDS[I] against container NAME, in DWORK.
step_cmd() {
    local cmd="${STEP_CMDS[$2]}"
    [[ "$cmd" =~ ^docker\ compose\ exec\ (-T\ )?postgres\  ]] || fail "Updating page command not of the form 'docker compose exec [-T] postgres ...': $cmd"
    cmd="docker exec -i $(cname "$1") ${cmd#"${BASH_REMATCH[0]}"}"
    cmd="${cmd//<user>/$PG_USER}"; cmd="${cmd//<db>/app}"
    [[ "$cmd" != *'<'*'>'* ]] || fail "Updating page command with an unknown placeholder: $cmd"
    (cd "$DWORK" && eval "$cmd")
}

log "Updating step 3: a service with image: and build: args, like skjera's"
step3_tag="pg-automerge-test-step3-$$:$VERSION"
cat >"$DWORK/step3.yml" <<YAML
services:
  postgres:
    image: pg-automerge-test-step3-$$:$OLD_VERSION
    build:
      context: $ROOT_DIR
      dockerfile: docker/Dockerfile
      args:
        PG_AUTOMERGE_VERSION: $OLD_VERSION
YAML
if docker compose -p "$PROJECT-step3" -f "$DWORK/step3.yml" build >"$DWORK/step3-old.log" 2>&1; then
    IMAGES+=("pg-automerge-test-step3-$$:$OLD_VERSION")
    fail "the image built with PG_AUTOMERGE_VERSION=$OLD_VERSION"
fi
grep -q "versions: PG_AUTOMERGE_VERSION=$OLD_VERSION != $VERSION (Cargo.toml)" "$DWORK/step3-old.log" \
    || { tail -30 "$DWORK/step3-old.log" >&2; fail "the old version arg failed for another reason"; }
# Step 3's edit: the image tag and the build arg.
sed -i "s/:$OLD_VERSION\$/:$VERSION/; s/PG_AUTOMERGE_VERSION: $OLD_VERSION\$/PG_AUTOMERGE_VERSION: $VERSION/" "$DWORK/step3.yml"
IMAGES+=("$step3_tag")
docker compose -p "$PROJECT-step3" -f "$DWORK/step3.yml" build >"$DWORK/step3-new.log" 2>&1 \
    || { tail -30 "$DWORK/step3-new.log" >&2; fail "the service did not build after step 3"; }
expect "step 3 image version label" "$VERSION" \
    "$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' "$step3_tag")"

# ---------------------------------------------------------------------------
log "old image: pg_automerge $OLD_VERSION with tables, indexes, a view, a trigger and documents"
RUN_IMAGE="$OLD_IMAGE" start_ready old data
expect "old: extension version" "$OLD_VERSION" \
    "$(psql_in old app -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")"
psql_in old app -v base="$BASE" -v alice="$ALICE" -v bob_changes="$BOB_CHANGES" -v note="$NOTE" <<'SQL'
CREATE TABLE docs (
    id int PRIMARY KEY,
    doc automerge NOT NULL,
    data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED
);
CREATE INDEX docs_gin ON docs USING gin ((doc::jsonb));
CREATE INDEX docs_title ON docs ((doc::jsonb ->> 'title'));
CREATE TRIGGER docs_notify AFTER INSERT OR UPDATE OR DELETE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'id');
CREATE TABLE pairs (id int PRIMARY KEY, a automerge NOT NULL, b automerge);
CREATE VIEW doc_heads AS SELECT id, automerge_heads(doc) AS heads, automerge_change_count(doc) AS changes FROM docs;
INSERT INTO docs VALUES (1, :'base'), (2, :'alice'), (3, merge(:'base'::automerge, :'bob_changes'::bytea)), (4, :'note');
INSERT INTO docs SELECT 5, merge_agg(doc ORDER BY id) FROM docs WHERE id <= 3;
INSERT INTO pairs VALUES (1, :'base', :'alice'), (2, :'note', NULL);
SQL
FINGERPRINT="SELECT string_agg(d.id || ':' || md5(d.doc::bytea) || ':' || automerge_heads(d.doc)::text || ':' || md5(d.doc::jsonb::text)
                               || ':' || md5(d.data::text) || ':' || v.heads::text || ':' || v.changes, ',' ORDER BY d.id)
             FROM docs d JOIN doc_heads v USING (id) WHERE d.id <= 5;
SELECT string_agg(id || ':' || md5(a::bytea) || ':' || coalesce(md5((a || b)::jsonb::text), '-'), ',' ORDER BY id) FROM pairs;"
before="$(psql_in old app -c "$FINGERPRINT")"
[[ -n "$before" ]] || fail "old: no fingerprint"
log "Updating step 1: the dump, as $PG_USER"
step_cmd old 0
[[ -s "$DWORK/before-$VERSION.dump" ]] || fail "Updating step 1 wrote no before-$VERSION.dump"
docker exec -i "$(cname old)" pg_restore --list <"$DWORK/before-$VERSION.dump" | grep -q 'EXTENSION - pg_automerge' \
    || fail "the dump of Updating step 1 has no pg_automerge"
check_log old
docker stop -t 30 "$(cname old)" >/dev/null
docker rm "$(cname old)" >/dev/null

# ---------------------------------------------------------------------------
log "new image ($IMAGE) on the same volume, before the UPDATE"
start_ready new data
C="$(cname new)"
grep -q 'Skipping initialization' <<<"$(docker logs "$C" 2>&1)" || { docker logs "$C" >&2; fail "the new image re-ran init"; }
expect "new, before UPDATE: extension version, automerge_spans absent" "$OLD_VERSION|$OLD_HAS_SPANS" \
    "$(psql_in new app -c "SELECT extversion, to_regprocedure('automerge_spans(automerge,text[])') IS NULL FROM pg_extension WHERE extname = 'pg_automerge'")"
expect "new, before UPDATE: documents" "$before" "$(psql_in new app -c "$FINGERPRINT")"
# The library is the new one: the deep block is stored through the
# generated column (the released 0.1.0 library crashes here).
psql_in new app -v deep="$DEEP_BLOCK" <<<"INSERT INTO docs VALUES (6, :'deep');"

log "Updating step 5: ALTER EXTENSION pg_automerge UPDATE, as $PG_USER"
# What the Updating page's commands used to hardcode fails here, as in skjera's
# cluster.
if out="$(docker exec "$C" psql -X -U postgres -d app -c 'SELECT 1' 2>&1)"; then
    fail "the cluster has a postgres role: the Updating page's commands are not tested with another superuser"
fi
grep -q 'role "postgres" does not exist' <<<"$out" || fail "psql -U postgres: $out"
step_cmd new 1 >/dev/null
expect "version after UPDATE (Updating step 5)" "$VERSION" "$(step_cmd new 2)"
expect "documents after UPDATE" "$before" "$(psql_in new app -c "$FINGERPRINT")"

out="$(psql_in new app <<'SQL'
SELECT count(*) FILTER (WHERE indisvalid AND indisready) || '/' || count(*) FROM pg_index WHERE indrelid = 'docs'::regclass;
SELECT bool_and(data = doc::jsonb) FROM docs;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT id FROM docs WHERE doc @> '{"items": [{"name": "eggs"}]}';
EXPLAIN (COSTS OFF) SELECT id FROM docs WHERE doc::jsonb ->> 'title' = 'Groceries for Sunday';
SELECT 'gin ' || string_agg(id::text, ',' ORDER BY id) FROM docs WHERE doc @> '{"items": [{"name": "eggs"}]}';
SELECT 'btree ' || string_agg(id::text, ',' ORDER BY id) FROM docs WHERE doc::jsonb ->> 'title' = 'Groceries for Sunday';
RESET enable_seqscan;
SET enable_indexscan = off; SET enable_bitmapscan = off;
SELECT 'gin-seq ' || string_agg(id::text, ',' ORDER BY id) FROM docs WHERE doc @> '{"items": [{"name": "eggs"}]}';
SELECT 'btree-seq ' || string_agg(id::text, ',' ORDER BY id) FROM docs WHERE doc::jsonb ->> 'title' = 'Groceries for Sunday';
SQL
)"
mapfile -t lines <<<"$out"
expect "indexes valid" "3/3" "${lines[0]}"
expect "generated column" "t" "${lines[1]}"
grep -q 'Bitmap Index Scan on docs_gin' <<<"$out" || fail "GIN index not used after the update: $out"
grep -q 'on docs_title' <<<"$out" || fail "B-tree expression index not used after the update: $out"
gin="$(sed -n 's/^gin //p' <<<"$out")"; btree="$(sed -n 's/^btree //p' <<<"$out")"
[[ -n "$gin" && -n "$btree" ]] || fail "index scans found nothing: $out"
expect "GIN index vs sequential scan" "$gin" "$(sed -n 's/^gin-seq //p' <<<"$out")"
expect "B-tree index vs sequential scan" "$btree" "$(sed -n 's/^btree-seq //p' <<<"$out")"

log "the trigger after the UPDATE"
out="$(psql_in new app -v bob_changes="$BOB_CHANGES" <<'SQL'
LISTEN docs_changed;
UPDATE docs SET doc = merge(doc, :'bob_changes'::bytea) WHERE id = 1;
SELECT to_jsonb(automerge_heads(doc)) FROM docs WHERE id = 1;
SQL
)"
payload="$(sed -n 's/^Asynchronous notification "docs_changed" with payload "\(.*\)" received from server process with PID [0-9]*\.$/\1/p' <<<"$out")"
[[ -n "$payload" ]] || fail "no notification after the update: $out"
expect "notification payload" 'public.docs UPDATE {"id": 1} t' \
    "$(psql_in new app -v p="$payload" -v h="$(tail -1 <<<"$out")" <<<"SELECT concat_ws(' ', p->>'table', p->>'op', p->'key', p->'columns'->'doc'->'heads' = :'h'::jsonb) FROM (SELECT :'p'::jsonb AS p) s")"

log "automerge_spans after the UPDATE"
expect "spans of the stored note" 'Shopping tips|Buy |fresh milk| on |Sunday|.' \
    "$(psql_in new app -c "SELECT string_agg(s->>'value', '|') FROM docs, jsonb_array_elements(automerge_spans(doc, '{body}')) s WHERE id = 4 AND s->>'type' = 'text'")"
expect "spans at heads" "t" \
    "$(psql_in new app -c "SELECT automerge_spans(a, '{body}', automerge_heads(a)) = automerge_spans(a, '{body}') FROM pairs WHERE id = 2")"
if out="$(psql_in new app -v VERBOSITY=verbose -c "SELECT automerge_spans(doc, '{body}') FROM docs WHERE id = 6" 2>&1)"; then
    fail "automerge_spans of the deep block did not fail"
fi
grep -q 'ERROR:  54000: automerge text block is nested more than 32 levels deep' <<<"$out" || fail "automerge_spans of the deep block: $out"

log "automerge_memory_usage() after the UPDATE"
expect "memory counters of a session that read documents" "t" \
    "$(psql_in new app -c "SELECT count(*) FROM docs WHERE id <> 6 AND doc::jsonb IS NOT NULL" \
        -c "SELECT loads > 0 AND live_documents = 0 AND peak_allocated_bytes >= allocated_bytes FROM automerge_memory_usage()" | tail -1)"

log "the updated catalog is a fresh install's (tests/catalog.sql)"
psql_in new postgres -c "CREATE DATABASE fresh"
psql_in new fresh -c "CREATE EXTENSION pg_automerge"
psql_in new fresh <tests/catalog.sql >"$DWORK/catalog.fresh"
psql_in new app <tests/catalog.sql >"$DWORK/catalog.updated"
[[ -s "$DWORK/catalog.fresh" ]] || fail "empty catalog"
diff -u "$DWORK/catalog.fresh" "$DWORK/catalog.updated" >"$DWORK/catalog.diff" \
    || fail "the updated catalog differs from a fresh install (- fresh, + updated):
$(cat "$DWORK/catalog.diff")"

check_log new
