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
#   2. CREATE EXTENSION pg_automerge VERSION 'V-snapshot', store documents,
#      ALTER EXTENSION pg_automerge UPDATE;
#   3. compare the extension's catalog (every member object, function
#      definitions and labels, types, casts, operators, aggregates,
#      comments) with a fresh CREATE EXTENSION of the current version, and
#      check the stored documents still read the same.
#
# So a SQL change without a new version and upgrade script fails here once
# a snapshot of the current version exists, and a broken upgrade script
# fails for the older versions.
#
# Env: see tests/lib.sh.

# shellcheck source=tests/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DB=pg_automerge_upgrade_old
DB_NEW=pg_automerge_upgrade_new
DROP_DBS=("$DB" "$DB_NEW")
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

# The extension's catalog, one line per fact, sorted.
CATALOG_SQL="$(cat <<'SQL'
WITH ext AS (SELECT oid FROM pg_extension WHERE extname = 'pg_automerge'),
members AS (
    SELECT d.classid, d.objid FROM pg_depend d, ext
    WHERE d.refclassid = 'pg_extension'::regclass AND d.refobjid = ext.oid AND d.deptype = 'e'
)
SELECT 'member ' || pg_describe_object(classid, objid, 0)
       || coalesce(' -- ' || obj_description(objid, (SELECT relname FROM pg_class WHERE oid = classid)::name), '')
FROM members
UNION ALL
SELECT 'function ' || p.oid::regprocedure || ' ' ||
       CASE WHEN p.prokind = 'a' THEN 'aggregate' ELSE pg_get_functiondef(p.oid) END
FROM members m JOIN pg_proc p ON m.classid = 'pg_proc'::regclass AND p.oid = m.objid
UNION ALL
SELECT format('aggregate %s trans=%s final=%s stype=%s space=%s combine=%s',
              a.aggfnoid::regprocedure, a.aggtransfn, a.aggfinalfn, a.aggtranstype::regtype,
              a.aggtransspace, a.aggcombinefn)
FROM members m JOIN pg_aggregate a ON m.classid = 'pg_proc'::regclass AND a.aggfnoid = m.objid
UNION ALL
SELECT format('type %s len=%s align=%s storage=%s in=%s out=%s recv=%s send=%s cat=%s',
              t.oid::regtype, t.typlen, t.typalign, t.typstorage, t.typinput, t.typoutput,
              t.typreceive, t.typsend, t.typcategory)
FROM members m JOIN pg_type t ON m.classid = 'pg_type'::regclass AND t.oid = m.objid
UNION ALL
SELECT format('cast %s -> %s func=%s context=%s method=%s',
              c.castsource::regtype, c.casttarget::regtype, c.castfunc::regprocedure,
              c.castcontext, c.castmethod)
FROM members m JOIN pg_cast c ON m.classid = 'pg_cast'::regclass AND c.oid = m.objid
UNION ALL
SELECT format('operator %s(%s, %s) code=%s com=%s neg=%s',
              o.oprname, o.oprleft::regtype, o.oprright::regtype, o.oprcode, o.oprcom::regoperator,
              o.oprnegate::regoperator)
FROM members m JOIN pg_operator o ON m.classid = 'pg_operator'::regclass AND o.oid = m.objid
UNION ALL
SELECT 'extension ' || extname || ' ' || extversion || ' relocatable=' || extrelocatable
FROM pg_extension WHERE extname = 'pg_automerge'
ORDER BY 1
SQL
)"
FINGERPRINT_SQL="SELECT string_agg(id || ':' || md5(doc::bytea) || ':' || automerge_heads(doc)::text
                                   || ':' || md5(doc::jsonb::text), ',' ORDER BY id) FROM docs"

DB="$DB_NEW" create_db
sql_on "$DB_NEW" -c "CREATE EXTENSION pg_automerge"
sql_on "$DB_NEW" -c "$CATALOG_SQL" >"$WORK/catalog.new"
[[ -s "$WORK/catalog.new" ]] || fail "empty catalog"

shopt -s nullglob
snapshots=(sql/snapshots/pg_automerge--*.sql)
((${#snapshots[@]})) || fail "no snapshots in sql/snapshots"
for snapshot in "${snapshots[@]}"; do
    version="${snapshot#sql/snapshots/pg_automerge--}"
    version="${version%.sql}"
    log "upgrading from $version (snapshot) to $CURRENT"
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
            cp "$hop" "$EXTDIR/pg_automerge--$old--$target"
            COPIED+=("$EXTDIR/pg_automerge--$old--$target")
        done
    fi

    create_db
    sql -v base="$BASE" -v a="$NEW_A" -v inc="$INC_A" -v old="$old" <<'SQL'
CREATE EXTENSION pg_automerge VERSION :'old';
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
CREATE INDEX ON docs USING gin ((doc::jsonb));
INSERT INTO docs VALUES (1, :'base'), (2, :'a'), (3, merge(:'base'::automerge, :'inc'::bytea));
SQL
    before="$(sql -c "$FINGERPRINT_SQL")"
    sql -c "ALTER EXTENSION pg_automerge UPDATE"
    [[ "$(sql -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge'")" == "$CURRENT" ]] \
        || fail "$version: not updated to $CURRENT"
    sql -c "$CATALOG_SQL" >"$WORK/catalog.old"
    diff -u "$WORK/catalog.new" "$WORK/catalog.old" >"$WORK/catalog.diff" \
        || fail "$version: the updated catalog differs from a fresh install (- fresh, + updated):
$(cat "$WORK/catalog.diff")"
    after="$(sql -c "$FINGERPRINT_SQL")"
    [[ -n "$before" && "$before" == "$after" ]] || fail "$version: stored documents read differently after the update"
    sql -c "SET enable_seqscan = off; SELECT count(*) FROM docs WHERE doc::jsonb @> '{\"a\": true}'" >/dev/null
done
