#!/usr/bin/env bash
# Extension upgrade test (run via `mise run upgrade`, also part of `mise
# run test`). See docs/DESIGN.md, "Versioning and upgrades".
#
# For every released version V with a committed install script
# (sql/snapshots/pg_automerge--V.sql):
#
#   1. install V's script under the version name "V-snapshot" (so it never
#      collides with the script of the build), plus the first hop of every
#      upgrade path from V (sql/pg_automerge--V--W.sql, installed by
#      `cargo pgrx install`, copied as pg_automerge--V-snapshot--W.sql); for
#      V equal to the current version, an empty V-snapshot--V script;
#   2. CREATE EXTENSION pg_automerge VERSION 'V-snapshot', and what a
#      deployed database has on it: a table with a STORED generated
#      doc::jsonb column, GIN and B-tree expression indexes, a view and a
#      SQL function over the extension's, an automerge_notify() trigger,
#      and documents (plain, merged, rich text, a 2,000-level deep block);
#      read them all (the old catalog on the new library, as between a new
#      image's start and the UPDATE), then ALTER EXTENSION pg_automerge
#      UPDATE;
#   3. compare the extension's catalog (every member object and its
#      comment, the members' dependencies, function definitions with their
#      labels, symbols and ACLs, aggregates, types, casts, operators, the
#      extension row) with a fresh CREATE EXTENSION of the current
#      version, check the documents still read the same (bytes, heads, the
#      cast, the generated column, the view, the SQL function), the indexes
#      are valid, used and agree with a sequential scan, the trigger sends
#      its notification, and automerge_spans works;
#   4. the same update of an extension relocated (SET SCHEMA) before it:
#      every member ends up in its schema; and, for a version without
#      automerge_spans, that the update fails instead of adopting a user's
#      function of the same signature in the extension's schema.
#
# The current version must have a snapshot, and every upgrade script must
# be installed byte for byte as in sql/.
#
# So a SQL change without a new version and upgrade script fails here once
# a snapshot of the current version exists, and a broken upgrade script
# fails for the older versions.
#
# Env: see tests/lib.sh.

# shellcheck source=tests/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
# It copies SQL scripts into the server's extension directory.
require_pgrx_mode

DB=pg_automerge_upgrade_old
DB_NEW=pg_automerge_upgrade_new
DB_REL=pg_automerge_upgrade_relocated
DROP_DBS=("$DB" "$DB_NEW" "$DB_REL")
EXTDIR="$("$PG_CONFIG" --sharedir)/extension"
CURRENT="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
COPIED=()

on_exit() {
    for f in "${COPIED[@]}"; do rm -f "$f"; done
    if [[ $1 == 0 ]]; then log "all upgrade checks passed"; else echo "upgrade test FAILED" >&2; fi
}

log "generating fixture documents"
eval "$(cargo run -q -p pg_automerge_core --example gen_concurrency)"

install_extension
start_server

# Rich-text fixtures of the regress examples: a note whose body has marks
# and blocks, and a block nested 2,000 levels deep.
fixture() { sed -n "s/^\\\\set $1 '\\\\\\\\x\\([0-9a-f]*\\)'\$/\\1/p" tests/pg_regress/sql/automerge.sql; }
NOTE="\\x$(fixture note)"; DEEP_BLOCK="\\x$(fixture deep_block)"
[[ ${#NOTE} -gt 2 && ${#DEEP_BLOCK} -gt 2 ]] || fail "fixtures not found in tests/pg_regress/sql/automerge.sql"

# The extension's catalog, one line per fact, sorted (tests/catalog.sql).
CATALOG_SQL="$(cat tests/catalog.sql)"
# The stored documents as every reader sees them: bytes, heads, jsonb (the
# cast and the STORED generated column), a view over the extension's
# functions and a SQL function on the type.
FINGERPRINT_SQL="SELECT string_agg(d.id || ':' || md5(d.doc::bytea) || ':' || automerge_heads(d.doc)::text
                                   || ':' || md5(d.doc::jsonb::text) || ':' || md5(d.data::text)
                                   || ':' || v.heads::text || ':' || coalesce(doc_title(d.doc), '-'),
                                   ',' ORDER BY d.id)
                 FROM docs d JOIN doc_heads v USING (id)"

DB="$DB_NEW" create_db
sql_on "$DB_NEW" -c "CREATE EXTENSION pg_automerge"
sql_on "$DB_NEW" -c "$CATALOG_SQL" >"$WORK/catalog.new"
[[ -s "$WORK/catalog.new" ]] || fail "empty catalog"
grep -q '^depends ' "$WORK/catalog.new" || fail "no dependencies in the catalog listing"

shopt -s nullglob
# Released scripts, and variants of a released version's catalog that
# exist in deployments (sql/snapshots/variants/pg_automerge--V+NAME.sql,
# updated with V's upgrade scripts; see docs/DESIGN.md).
snapshots=(sql/snapshots/pg_automerge--*.sql sql/snapshots/variants/pg_automerge--*.sql)
((${#snapshots[@]})) || fail "no snapshots in sql/snapshots"
[[ -e "sql/snapshots/pg_automerge--$CURRENT.sql" ]] \
    || fail "no snapshot of the current version $CURRENT (cargo pgrx schema pg18 -o sql/snapshots/pg_automerge--$CURRENT.sql)"
for snapshot in "${snapshots[@]}"; do
    label="${snapshot##*/pg_automerge--}"
    label="${label%.sql}"
    version="${label%%+*}"
    log "upgrading from $label (snapshot) to $CURRENT"
    old="$version-snapshot"
    cp "$snapshot" "$EXTDIR/pg_automerge--$old.sql"
    COPIED+=("$EXTDIR/pg_automerge--$old.sql")
    if [[ "$version" == "$CURRENT" ]]; then
        : >"$EXTDIR/pg_automerge--$old--$CURRENT.sql"
        COPIED+=("$EXTDIR/pg_automerge--$old--$CURRENT.sql")
    else
        hops=(sql/pg_automerge--"$version"--*.sql)
        ((${#hops[@]})) || fail "no upgrade script from $version (sql/pg_automerge--$version--*.sql)"
        for hop in "${hops[@]}"; do
            target="${hop#sql/pg_automerge--"$version"--}"
            # The installed copy must be the repository's (cargo pgrx
            # install ships sql/pg_automerge--*--*.sql).
            cmp -s "$hop" "$EXTDIR/$(basename "$hop")" || fail "$hop is not installed as is in $EXTDIR"
            cp "$hop" "$EXTDIR/pg_automerge--$old--$target"
            COPIED+=("$EXTDIR/pg_automerge--$old--$target")
        done
    fi

    # Objects a deployed database has on the old version: a STORED
    # generated doc::jsonb column, GIN and B-tree expression indexes, a
    # view and a SQL function over the extension's, a notify trigger, and
    # documents (plain, merged, rich text, a deep block).
    create_db
    sql -v base="$BASE" -v a="$NEW_A" -v inc="$INC_A" -v note="$NOTE" -v deep="$DEEP_BLOCK" -v old="$old" <<'SQL'
CREATE EXTENSION pg_automerge VERSION :'old';
CREATE TABLE docs (
    id int PRIMARY KEY,
    doc automerge NOT NULL,
    data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED
);
CREATE INDEX docs_gin ON docs USING gin ((doc::jsonb) jsonb_path_ops);
CREATE INDEX docs_title ON docs ((doc::jsonb ->> 'title'));
CREATE VIEW doc_heads AS SELECT id, automerge_heads(doc) AS heads FROM docs;
CREATE FUNCTION doc_title(d automerge) RETURNS text LANGUAGE sql IMMUTABLE RETURN d::jsonb ->> 'title';
CREATE TRIGGER docs_notify AFTER INSERT OR UPDATE OR DELETE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'id');
INSERT INTO docs VALUES (1, :'base'), (2, :'a'), (3, merge(:'base'::automerge, :'inc'::bytea)),
                        (4, :'note'), (5, :'deep');
SQL
    # A catalog that already has automerge_spans: a view on it must
    # survive the update (the functions are replaced in place).
    spans_view=0
    if grep -q 'automerge_spans' "$snapshot"; then
        spans_view=1
        sql -c "CREATE VIEW note_spans AS SELECT id, automerge_spans(doc, '{body}') AS s FROM docs WHERE id = 4"
        spans_before="$(sql -c "SELECT md5(s::text) FROM note_spans")"
    fi
    # The old catalog runs on the new library until the UPDATE (a new
    # image started on the old volume): everything reads the same.
    before="$(sql -c "$FINGERPRINT_SQL")"
    sql -c "ALTER EXTENSION pg_automerge UPDATE"
    [[ "$(sql -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")" == "$CURRENT" ]] \
        || fail "$label: not updated to $CURRENT"
    sql -c "$CATALOG_SQL" >"$WORK/catalog.old"
    diff -u "$WORK/catalog.new" "$WORK/catalog.old" >"$WORK/catalog.diff" \
        || fail "$label: the updated catalog differs from a fresh install (- fresh, + updated):
$(cat "$WORK/catalog.diff")"
    after="$(sql -c "$FINGERPRINT_SQL")"
    [[ -n "$before" && "$before" == "$after" ]] || fail "$label: stored documents read differently after the update"

    # Dependent objects: indexes valid and used, and they agree with a
    # sequential scan; the generated column recomputes to the same jsonb.
    out="$(sql <<'SQL'
SELECT count(*) FILTER (WHERE indisvalid AND indisready) || '/' || count(*) FROM pg_index WHERE indrelid = 'docs'::regclass;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT id FROM docs WHERE doc @> '{"base": true}';
EXPLAIN (COSTS OFF) SELECT id FROM docs WHERE doc::jsonb ->> 'title' = 'x';
SELECT string_agg(id::text, ',' ORDER BY id) FROM docs WHERE doc @> '{"base": true}';
RESET enable_seqscan;
SET enable_indexscan = off; SET enable_bitmapscan = off;
SELECT string_agg(id::text, ',' ORDER BY id) FROM docs WHERE doc @> '{"base": true}';
RESET enable_indexscan; RESET enable_bitmapscan;
SELECT bool_and(data = doc::jsonb) FROM docs;
UPDATE docs SET doc = doc WHERE id = 5;
SELECT bool_and(data = doc::jsonb) FROM docs;
SQL
)"
    mapfile -t lines <<<"$out"
    [[ "${lines[0]}" == 3/3 ]] || fail "$label: indexes after the update: ${lines[0]}"
    grep -q 'Bitmap Index Scan on docs_gin' <<<"$out" || fail "$label: GIN index not used after the update: $out"
    grep -q 'docs_title' <<<"$out" || fail "$label: B-tree expression index not used after the update: $out"
    [[ "$(grep -cx 't' <<<"$out")" == 2 ]] || fail "$label: generated column after the update: $out"
    by_index="$(sed -n '/^[0-9,]*$/p' <<<"$out" | sed -n 1p)"
    by_scan="$(sed -n '/^[0-9,]*$/p' <<<"$out" | sed -n 2p)"
    [[ "$by_index" == 1,2,3 && "$by_scan" == 1,2,3 ]] || fail "$label: index scan ($by_index) and seq scan ($by_scan) disagree"

    # The trigger still fires with the new library and catalog.
    out="$(sql -v inc="$INC_A" <<'SQL'
LISTEN docs_changed;
UPDATE docs SET doc = merge(doc, :'inc'::bytea) WHERE id = 1;
SELECT to_jsonb(automerge_heads(doc)) FROM docs WHERE id = 1;
SQL
)"
    payload="$(sed -n 's/^Asynchronous notification "docs_changed" with payload "\(.*\)" received from server process with PID [0-9]*\.$/\1/p' <<<"$out")"
    [[ -n "$payload" ]] || fail "$label: no notification after the update: $out"
    heads="$(tail -1 <<<"$out")"
    [[ "$(sql -v p="$payload" -v h="$heads" <<<"SELECT concat_ws(' ', p->>'table', p->>'op', p->'key', p->'columns'->'doc'->'heads' = :'h'::jsonb) FROM (SELECT :'p'::jsonb AS p) s")" \
        == 'public.docs UPDATE {"id": 1} t' ]] || fail "$label: notification payload after the update: $payload"

    if ((spans_view)); then
        [[ "$(sql -c "SELECT md5(s::text) FROM note_spans")" == "$spans_before" ]] \
            || fail "$label: a view on automerge_spans reads differently after the update"
    fi

    # The new functions work on the stored documents.
    [[ "$(sql -c "SELECT s->>'value' FROM docs, jsonb_array_elements(automerge_spans(doc, '{body}')) s WHERE id = 4 AND s->>'type' = 'text'" | tr '\n' '|')" \
        == 'Shopping tips|Buy |fresh milk| on |Sunday|.|' ]] || fail "$label: automerge_spans after the update"
    [[ "$(sql -c "SELECT automerge_spans(doc, '{body}', automerge_heads(doc)) = automerge_spans(doc, '{body}') FROM docs WHERE id = 4")" == t ]] \
        || fail "$label: automerge_spans at heads after the update"
    if out="$(sql -c "SELECT automerge_spans(doc, '{body}') FROM docs WHERE id = 5" 2>&1)"; then
        fail "$label: automerge_spans of the deep block did not fail"
    fi
    grep -q 'nested more than 32 levels deep' <<<"$out" || fail "$label: automerge_spans of the deep block: $out"

    # Relocated before the update: the new objects go to the extension's
    # current schema, every member stays together.
    log "  $label: relocated to another schema, then updated"
    DB="$DB_REL" create_db
    sql_on "$DB_REL" -v old="$old" <<'SQL'
CREATE SCHEMA am1;
CREATE SCHEMA am2;
CREATE EXTENSION pg_automerge VERSION :'old' SCHEMA am1;
CREATE TABLE t (doc am1.automerge);
ALTER EXTENSION pg_automerge SET SCHEMA am2;
ALTER EXTENSION pg_automerge UPDATE;
SQL
    out="$(sql_on "$DB_REL" -c "WITH m AS (SELECT classid, objid FROM pg_depend WHERE deptype = 'e'
            AND refobjid = (SELECT oid FROM pg_extension WHERE extname = 'pg_automerge'))
        SELECT string_agg(DISTINCT nsp::regnamespace::text, ',') FROM (
            SELECT pronamespace AS nsp FROM pg_proc JOIN m ON classid = 'pg_proc'::regclass AND objid = pg_proc.oid
            UNION ALL SELECT typnamespace FROM pg_type JOIN m ON classid = 'pg_type'::regclass AND objid = pg_type.oid
            UNION ALL SELECT oprnamespace FROM pg_operator JOIN m ON classid = 'pg_operator'::regclass AND objid = pg_operator.oid) s" \
        -c "SELECT am2.automerge_spans(''::bytea::am2.automerge, '{missing}') IS NULL")"
    [[ "$out" == $'am2\nt' ]] || fail "$label: relocated update: $out"

    # A function of the new signature that already exists in the
    # extension's schema is not adopted: the update fails and leaves the
    # old version (CREATE OR REPLACE in an extension script refuses an
    # object the extension does not own).
    if ! grep -q 'automerge_spans' "$snapshot"; then
        log "  $label: a user's automerge_spans in the extension's schema"
        sql_on "$DB_REL" -c "DROP SCHEMA am1, am2 CASCADE" -c "CREATE EXTENSION pg_automerge VERSION '$old'" \
            -c "CREATE FUNCTION automerge_spans(automerge, text[]) RETURNS jsonb LANGUAGE sql RETURN NULL::jsonb"
        if out="$(sql_on "$DB_REL" -c "ALTER EXTENSION pg_automerge UPDATE" 2>&1)"; then
            fail "$label: the update adopted a user's automerge_spans"
        fi
        grep -q 'is not a member of extension "pg_automerge"' <<<"$out" || fail "$label: update over a user's automerge_spans: $out"
        [[ "$(sql_on "$DB_REL" -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")" == "$old" ]] \
            || fail "$label: a failed update changed the version"
    fi
done
